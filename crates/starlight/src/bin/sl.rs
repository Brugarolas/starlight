/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use starlight::vm::context::Context;
use starlight::{letroot, prelude::*};
use structopt::*;
use std::time::{Duration, Instant};

#[cfg(not(debug_assertions))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    Platform::initialize();
    let options = Options::from_args();

    let mut vm = VirtualMachine::new(options, None);

    #[cfg(all(target_pointer_width = "64", feature = "ffi"))]
    if vm.options().enable_ffi {
        vm.add_ffi();
    }

    let mut ctx = Context::new(&mut vm);

    let exit_code = match std::fs::read_to_string(&vm.options().file) {
        Ok(source) => {
            let name = vm.options().file.to_string_lossy().into_owned();
            let result = if vm.options().register {
                run_register(ctx, &source)
            } else {
                run_module(ctx, &name, &source)
            };
            match result {
                Ok(elapsed) => {
                    eprintln!("Executed in {}ms", elapsed.as_secs_f64() * 1000.0);
                    0
                }
                Err((stage, error)) => {
                    let message = error.to_string(ctx).unwrap_or_else(|_| "<unknown error>".into());
                    eprintln!("{}: {}", stage, message);
                    let stacktrace = ctx.take_stacktrace();
                    if !stacktrace.is_empty() {
                        eprintln!("Stacktrace: \n{}", stacktrace);
                    }
                    1
                }
            }
        }
        Err(error) => {
            eprintln!("Error while reading JS source: {}", error);
            1
        }
    };
    unsafe {
        vm.dispose();
    }

    std::process::exit(exit_code);
}

type ExecutionResult = Result<Duration, (&'static str, JsValue)>;

fn run_register(mut ctx: GcPointer<Context>, source: &str) -> ExecutionResult {
    letroot!(
        bytecode = stack,
        ctx.compile_register(source).map_err(|error| ("Compilation failed", error))?
    );
    let start = Instant::now();
    ctx.execute_register(bytecode)
        .map_err(|error| ("Uncaught exception", error))?;
    Ok(start.elapsed())
}

fn run_module(mut ctx: GcPointer<Context>, name: &str, source: &str) -> ExecutionResult {
    letroot!(
        function = stack,
        ctx.compile_module(name, "<script>", source)
            .map_err(|error| ("Compilation failed", error))?
            .get_jsobject()
    );
    let global = ctx.global_object();
    letroot!(module_object = stack, JsObject::new_empty(ctx));
    let exports = JsObject::new_empty(ctx);
    module_object
        .put(ctx, "@exports".intern(), JsValue::new(exports), false)
        .map_err(|error| ("Compilation failed", error))?;
    let mut args = [JsValue::new(module_object)];
    let mut args = Arguments::new(JsValue::encode_object_value(global), &mut args);
    let start = Instant::now();
    let f = function;
    function
        .as_function_mut()
        .call(ctx, &mut args, JsValue::new(f))
        .map_err(|error| ("Uncaught exception", error))?;
    Ok(start.elapsed())
}
