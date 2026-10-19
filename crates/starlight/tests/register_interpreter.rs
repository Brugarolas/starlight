use std::sync::Mutex;

use starlight::{prelude::*, vm::context::Context};

// VM initialization and the process-wide collector are shared by these tests.
static VM_LOCK: Mutex<()> = Mutex::new(());

fn with_context(test: impl FnOnce(GcPointer<Context>)) {
    let _lock = VM_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    Platform::initialize();
    let mut vm = VirtualMachine::new(Options::default(), None);
    let ctx = Context::new(&mut vm);
    test(ctx);
    unsafe { vm.dispose() };
}

fn run(mut ctx: GcPointer<Context>, source: &str) -> JsValue {
    ctx.eval_register(source).unwrap_or_else(|error| {
        panic!(
            "register execution failed: {}\nsource: {}",
            error.to_string(ctx).unwrap_or_else(|_| "<unknown>".into()),
            source
        )
    });
    ctx.global_object().get(ctx, "result".intern()).unwrap()
}

fn number(ctx: GcPointer<Context>, source: &str, expected: f64) {
    let result = run(ctx, source);
    assert!(result.is_number(), "expected numeric result for {}", source);
    assert_eq!(result.get_number(), expected, "source: {}", source);
}

#[test]
fn arithmetic_preserves_javascript_number_semantics() {
    with_context(|ctx| {
        for (expression, expected) in [
            ("2147483647 + 1", 2147483648.0),
            ("-2147483648 - 1", -2147483649.0),
            ("46341 * 46341", 2147488281.0),
            ("-(-2147483648)", 2147483648.0),
            ("-7 % 3", -1.0),
            ("2 ** 10", 1024.0),
            ("-1 >>> 0", 4294967295.0),
            ("(1 << 33) + (32 >> 2) + (9 ^ 3) + (9 & 3) + (9 | 3)", 32.0),
            ("1 / -0", f64::NEG_INFINITY),
            ("1 / (-1 * 0)", f64::NEG_INFINITY),
        ] {
            number(ctx, &format!("result = {};", expression), expected);
        }
        assert!(run(ctx, "result = 0 / 0;").get_number().is_nan());
    });
}

#[test]
fn coercion_comparisons_and_unary_operators() {
    with_context(|ctx| {
        for (expression, expected) in [
            ("'9' - 2", 7.0),
            ("true + null", 1.0),
            ("('10' < '2') ? 1 : 0", 1.0),
            ("('10' < 2) ? 1 : 0", 0.0),
            ("('2' == 2 && '2' !== 2 && null == undefined) ? 1 : 0", 1.0),
            ("(0 / 0 !== 0 / 0 && !(0 / 0 < 1) && !(0 / 0 >= 1)) ? 1 : 0", 1.0),
            ("~5", -6.0),
            ("+'12'", 12.0),
        ] {
            number(ctx, &format!("result = {};", expression), expected);
        }
        assert_eq!(run(ctx, "result = '4' + 2;").to_string(ctx).unwrap(), "42");
        assert_eq!(
            run(ctx, "result = typeof neverDeclared;").to_string(ctx).unwrap(),
            "undefined"
        );
    });
}

#[test]
fn short_circuiting_returns_operands_without_evaluating_skipped_branches() {
    with_context(|ctx| {
        number(ctx, "var n = 0; var a = 0 && (n = 100); var b = 7 || (n = 100); var c = null ?? (n = 3); result = a + b + c + n;", 13.0);
        number(ctx, "var n = 0; result = (false ? (n = 100) : 9) + n;", 9.0);
    });
}

#[test]
fn loops_break_continue_and_update_expressions() {
    with_context(|ctx| {
        number(ctx, "var total = 0; for (var i = 0; i < 20; i++) { if (i % 2 === 0) continue; if (i === 13) break; total += i; } var j = 3; while (j > 0) { total += j; j--; } do { total += 2; } while (false); result = total;", 44.0);
        number(ctx, "var n = 4; var old = n++; var next = ++n; result = old * 100 + next * 10 + n;", 466.0);
    });
}

#[test]
fn property_access_preserves_receiver_and_assignment_order() {
    with_context(|ctx| {
        number(ctx, "var obj = {x: 20, get: function(n) { return this.x + n; }}; var a = [1, 2, 3]; a[1] = obj.get(20); result = a[1] + a.length - a[0];", 42.0);
        number(ctx, "var trace = 0; var obj = {x: 5}; function base() { trace = trace * 10 + 1; return obj; } function key() { trace = trace * 10 + 2; return 'x'; } function rhs() { trace = trace * 10 + 3; return 7; } base()[key()] += rhs(); result = trace * 100 + obj.x;", 12312.0);
        number(ctx, "var obj = {x: 1}; var old = obj.x++; var next = ++obj.x; delete obj.x; result = old * 100 + next * 10 + (('x' in obj) ? 1 : 0);", 130.0);
    });
}

#[test]
fn calls_recursion_and_captured_variables() {
    with_context(|ctx| {
        number(ctx, "function fib(n) { if (n < 2) return n; return fib(n - 1) + fib(n - 2); } result = fib(10);", 55.0);
        number(ctx, "function counter(start) { var value = start; return function(step) { value += step; return value; }; } var next = counter(30); next(5); result = next(7);", 42.0);
        number(ctx, "var object = {value: 40, method: function() { var arrow = () => this.value + 2; return arrow(); }}; result = object.method();", 42.0);
        number(ctx, "function pair(a, b) { return a + (b === undefined ? 2 : b); } result = pair(40);", 42.0);
    });
}

#[test]
fn constructors_native_calls_and_prototype_properties() {
    with_context(|ctx| {
        number(ctx, "function Box(value) { this.value = value; } Box.prototype.get = function() { return this.value; }; var object = new Box(42); result = object instanceof Box ? object.get() : 0;", 42.0);
        number(ctx, "result = Math.max(Math.abs(-40), 20) + Math.floor(2.9);", 42.0);
        number(ctx, "var object = {x: 40}; Object.defineProperty(object, 'answer', {get: function() { return this.x + 2; }}); result = object.answer;", 42.0);
    });
}

#[test]
fn stack_and_register_functions_can_call_each_other() {
    with_context(|mut ctx| {
        ctx.eval("legacy = function(callback, value) { return callback(value) + 1; };").unwrap();
        number(ctx, "registerCallback = function(value) { return value * 2; }; result = legacy(registerCallback, 20);", 41.0);
        ctx.eval("result = registerCallback(21);").unwrap();
        assert_eq!(ctx.global_object().get(ctx, "result".intern()).unwrap().get_number(), 42.0);
    });
}

#[test]
fn exceptions_and_finally_preserve_abrupt_completions() {
    with_context(|ctx| {
        number(ctx, "function fail() { throw 40; } var value = 0; try { fail(); value = 100; } catch (error) { value = error; } finally { value += 2; } result = value;", 42.0);
        number(ctx, "function answer() { try { return 1; } finally { return 42; } } result = answer();", 42.0);
        number(ctx, "var value = 0; for (var i = 0; i < 5; i++) { try { if (i === 1) continue; if (i === 3) break; value += 10; } finally { value++; } } result = value;", 24.0);
        number(ctx, "var value = 0; try { try { throw 40; } finally { value++; } } catch (error) { value += error; } result = value + 1;", 42.0);
    });
}

#[test]
fn errors_do_not_poison_subsequent_execution() {
    with_context(|mut ctx| {
        for source in [
            "throw 7;",
            "missingVariable;",
            "null.x;",
            "var f = 1; f();",
            "result = beforeInitialization; let beforeInitialization = 1;",
            "const immutable = 1; immutable = 2;",
            "var broken = ;",
        ] {
            assert!(ctx.eval_register(source).is_err(), "expected an error: {}", source);
            number(ctx, "result = 6 * 7;", 42.0);
        }
    });
}

#[test]
fn registers_closures_and_literals_survive_collection() {
    with_context(|mut ctx| {
        number(ctx, "function answer() { var object = {x: 40}; var text = 'retained'; gc(); return object.x + text.length - 6; } result = answer();", 42.0);
        run(ctx, "saved = (function() { var object = {x: 40}; return function() { gc(); return object.x + 2; }; })(); result = 0;");
        ctx.heap().gc();
        number(ctx, "result = saved();", 42.0);
    });
}

#[test]
fn compiled_register_bytecode_can_be_executed_again() {
    with_context(|mut ctx| {
        run(ctx, "result = 0;");
        let code = ctx.compile_register("result = result + 1;").unwrap();
        for _ in 0..10 {
            ctx.execute_register(code).unwrap();
        }
        assert_eq!(ctx.global_object().get(ctx, "result".intern()).unwrap().get_number(), 10.0);
    });
}
