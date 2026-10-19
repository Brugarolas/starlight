use std::{cell::RefCell, collections::HashMap, convert::TryFrom, rc::Rc};

use crate::{
    bytecode::{
        block::{BranchKind, BranchSite, BytecodeBlock, BytecodeValidationError, InstructionIndex},
        opcodes::{OpCode, RegisterId},
    },
    gc::GcPointer,
    vm::context::Context,
    vm::{
        symbol_table::{Internable, Symbol},
        value::JsValue,
    },
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegisterCompileError {
    Unsupported(String),
    RegisterLimitExceeded { requested: usize, available: usize },
    BytecodeValidation(BytecodeValidationError),
}

impl From<BytecodeValidationError> for RegisterCompileError {
    fn from(error: BytecodeValidationError) -> Self {
        Self::BytecodeValidation(error)
    }
}

pub type ScopeRef = Rc<RefCell<Scope>>;
/// JS variable scope representation at compile-time.
pub struct Scope {
    pub parent: Option<ScopeRef>,
    pub variables: HashMap<Symbol, Variable>,
    pub env: RegisterId,
    pub depth: u32,
}
impl Scope {
    pub fn add_var(&mut self, name: Symbol, ix: u16) -> u16 {
        self.variables.insert(
            name,
            Variable {
                kind: VariableKind::Var,
                name,
                index: ix,
                dont_free: false,
            },
        );
        ix
    }
    pub fn add_const_var(&mut self, name: Symbol, ix: u16) -> u16 {
        self.variables.insert(
            name,
            Variable {
                kind: VariableKind::Const,
                name,
                index: ix,
                dont_free: true,
            },
        );
        ix
    }

    pub fn add_let_var(&mut self, name: Symbol, ix: u16) -> u16 {
        self.variables.insert(
            name,
            Variable {
                kind: VariableKind::Let,
                name,
                index: ix,
                dont_free: true,
            },
        );
        ix
    }
}

pub struct Variable {
    pub name: Symbol,
    pub index: u16,
    pub kind: VariableKind,
    pub dont_free: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum VariableKind {
    Let,
    Const,
    Var,
    Global,
    GlobalLet,
    GlobalConst,
}
#[derive(Clone, Debug)]
pub enum Access {
    Variable(u16, u32),
    VariableCurrent(u16, u32, u16),
    Global(Symbol),
    ById(RegisterId, Symbol),
    ArrayPat(Vec<(usize, Access)>),
    ByVal(RegisterId, RegisterId),
    This,
}

impl Access {
    pub fn expects_this(&self) -> bool {
        match self {
            Self::ById { .. } => true,
            Self::ByVal { .. } => true,
            Self::ArrayPat { .. } => true,
            _ => false,
        }
    }
}

use self::register_scope::{RegisterScope, RegisterScopeError};
mod lowering;
pub mod register_scope;
pub struct Generator {
    pub ctx: GcPointer<Context>,
    pub block: GcPointer<BytecodeBlock>,
    pub parent: *mut Self,
    pub registers: RegisterScope,
    pub scope: ScopeRef,
    function_scope: ScopeRef,
    top_level: bool,
    loop_stack: Vec<LoopControl>,
    /// Freelist of unused variable indexes
    pub variable_freelist: Vec<u32>,
    next_environment_index: u32,
    pub name_map: HashMap<Symbol, u32>,
}

#[derive(Default)]
struct LoopControl {
    breaks: Vec<BranchSite>,
    continues: Vec<BranchSite>,
    label: Option<Symbol>,
    is_loop: bool,
    break_cleanup_depth: usize,
    continue_cleanup_depth: usize,
}

impl Generator {
    pub fn compile_script(
        mut ctx: GcPointer<Context>,
        script: &swc_ecmascript::ast::Script,
    ) -> Result<GcPointer<BytecodeBlock>, RegisterCompileError> {
        let _defer = ctx.heap().defer();
        let literals = crate::vm::array_storage::ArrayStorage::new(ctx.heap(), 0);
        let block = ctx.heap().allocate(BytecodeBlock::Compiled(
            crate::bytecode::block::BytecodeBlockInternal {
                code: Vec::new(),
                register_count: 0,
                parameter_count: 0,
                environment_size: 0,
                strict: lowering::is_strict(&script.body),
                name: "".intern(),
                is_arrow: false,
                literals,
                parent_blocks: Vec::new(),
                structures: Vec::new(),
                feedback: Vec::new(),
                names: Vec::new(),
            },
        ));
        let scope = Rc::new(RefCell::new(Scope {
            parent: None,
            variables: HashMap::new(),
            env: 0,
            depth: 0,
        }));
        let mut generator = Self::new(ctx, block, scope);
        generator.top_level = true;
        generator.compile_body(&script.body, 0, true)?;
        Ok(generator.block)
    }

    pub fn new(ctx: GcPointer<Context>, block: GcPointer<BytecodeBlock>, scope: ScopeRef) -> Self {
        Self {
            ctx,
            block,
            parent: std::ptr::null_mut(),
            registers: RegisterScope::new(),
            function_scope: scope.clone(),
            scope,
            top_level: false,
            loop_stack: Vec::new(),
            variable_freelist: Vec::new(),
            next_environment_index: 0,
            name_map: HashMap::new(),
        }
    }

    pub fn fdbk(&mut self) -> u32 {
        let ix = self.block.compiled_mut().feedback.len();
        self.block
            .compiled_mut()
            .feedback
            .push(crate::bytecode::TypeFeedBack::None);
        ix as _
    }
    pub fn emit(&mut self, opcode: OpCode) {
        self.block.compiled_mut().emit(opcode);
    }
    pub fn emit_branch(&mut self, kind: BranchKind) -> BranchSite {
        self.block.compiled_mut().emit_branch(kind)
    }
    pub fn patch_branch(
        &mut self,
        site: BranchSite,
        target: InstructionIndex,
    ) -> Result<(), BytecodeValidationError> {
        self.block.compiled_mut().patch_branch(site, target)
    }
    pub fn finalize_register_bytecode(
        &mut self,
        parameter_count: u32,
    ) -> Result<(), BytecodeValidationError> {
        let block = self.block.compiled_mut();
        block.register_count = self.registers.register_count();
        block.parameter_count = parameter_count;
        block.environment_size = self.next_environment_index;
        block.validate()
    }

    fn allocate_binding(
        &mut self,
        scope: &ScopeRef,
        name: Symbol,
        kind: VariableKind,
    ) -> Result<(u16, bool), RegisterCompileError> {
        if let Some(variable) = scope.borrow().variables.get(&name) {
            if std::mem::discriminant(&variable.kind) != std::mem::discriminant(&kind) {
                return Err(RegisterCompileError::Unsupported(format!(
                    "conflicting declarations for {name:?}"
                )));
            }
            return Ok((variable.index, false));
        }

        let index = if kind == VariableKind::Global {
            0
        } else if matches!(
            kind,
            VariableKind::Let
                | VariableKind::Const
                | VariableKind::GlobalLet
                | VariableKind::GlobalConst
        ) {
            // Lexical slots carry initialization and mutability state in the environment.
            let index = u16::try_from(self.next_environment_index).map_err(|_| {
                RegisterCompileError::Unsupported("environment exceeds 65536 binding slots".into())
            })?;
            self.next_environment_index += 1;
            index
        } else {
            self.registers
                .try_allocate(false)
                .map_err(|error| match error {
                    RegisterScopeError::RegisterLimitExceeded {
                        requested,
                        available,
                    } => RegisterCompileError::RegisterLimitExceeded {
                        requested,
                        available,
                    },
                })?
        };
        scope.borrow_mut().variables.insert(
            name,
            Variable {
                name,
                index,
                kind,
                dont_free: true,
            },
        );
        Ok((index, true))
    }
    /// Push scope and return current scope depth
    pub fn push_scope(&mut self) -> u32 {
        let d = self.scope.borrow().depth;
        let env = self.scope.borrow().env;
        let new_scope = Rc::new(RefCell::new(Scope {
            parent: Some(self.scope.clone()),
            depth: self.scope.borrow().depth,
            env,
            variables: Default::default(),
        }));
        self.scope = new_scope;
        d
    }
    pub fn pop_scope(&mut self) {
        let scope = self.scope.clone();
        self.scope = scope.borrow().parent.clone().expect("No scopes left");
        for var in scope.borrow().variables.iter() {
            if !var.1.dont_free {
                self.variable_freelist.push(var.1.index as u32);
            }
        }
    }
    pub fn get_sym(&mut self, name: Symbol) -> u32 {
        if let Some(ix) = self.name_map.get(&name) {
            return *ix;
        }

        let ix = self.block.compiled_mut().names.len();
        self.block.compiled_mut().names.push(name);
        self.name_map.insert(name, ix as _);
        ix as _
    }
    pub fn lookup_scope(&self, var: Symbol) -> Option<(u16, ScopeRef)> {
        let scope = self.scope.clone();

        if let Some(var) = scope.borrow().variables.get(&var).map(|x| x.index) {
            return Some((var, scope.clone()));
        }
        let mut scope = self.scope.borrow().parent.clone();
        while let Some(ns) = scope {
            if let Some(var) = ns.borrow().variables.get(&var).map(|x| x.index) {
                return Some((var, ns.clone()));
            }
            scope = ns.borrow().parent.clone();
        }
        None
    }

    pub fn access_var(&self, var: Symbol) -> Access {
        if let Some((ix, scope)) = self.lookup_scope(var) {
            let kind = scope.borrow().variables[&var].kind;
            if kind == VariableKind::Global {
                return Access::Global(var);
            }
            let cur_depth = self.scope.borrow().depth;
            let depth = cur_depth - scope.borrow().depth;
            if matches!(
                kind,
                VariableKind::Let
                    | VariableKind::Const
                    | VariableKind::GlobalLet
                    | VariableKind::GlobalConst
            ) {
                Access::Variable(ix, depth)
            } else if depth == 0 {
                return Access::VariableCurrent(ix, depth, scope.borrow().env);
            } else {
                Access::Variable(ix, depth)
            }
        } else {
            Access::Global(var)
        }
    }

    pub fn access_set(&mut self, src: RegisterId, acc: Access) -> Result<(), RegisterCompileError> {
        match acc {
            Access::Variable(index, depth) => {
                let dst = self.registers.allocate(true);
                self.emit(OpCode::GetEnvironment {
                    dst,
                    depth: i16::try_from(depth).map_err(|_| {
                        RegisterCompileError::Unsupported(
                            "environment depth exceeds register bytecode limits".into(),
                        )
                    })?,
                });
                self.emit(OpCode::SetVar {
                    src,
                    env: dst,
                    at: index,
                });
            }
            Access::VariableCurrent(index, _, _) => {
                self.emit(OpCode::SetLocal(src, index));
            }
            Access::Global(sym) => {
                let dst = self.registers.allocate(true);
                self.emit(OpCode::GetGlobal { dst });
                let prop = self.get_sym(sym);
                let fdbk = self.fdbk();
                self.emit(OpCode::PutById {
                    object: dst,
                    value: src,
                    prop,
                    fdbk,
                });
            }
            Access::ByVal(object, val) => {
                self.emit(OpCode::PutByVal {
                    object,
                    value: src,
                    key: val,
                });
                self.registers.unprotect(val);
                self.registers.unprotect(object);
            }
            Access::ById(object, sym) => {
                let fdbk = self.fdbk();
                let prop = self.get_sym(sym);
                self.emit(OpCode::PutById {
                    object,
                    value: src,
                    fdbk,
                    prop,
                });
                self.registers.unprotect(object);
            }
            Access::This => {
                self.emit(OpCode::StoreThis { src });
            }

            Access::ArrayPat(_) => {
                return Err(RegisterCompileError::Unsupported(
                    "array-pattern assignment is not supported by register lowering".into(),
                ));
            }
        }
        Ok(())
    }

    pub fn acess_get(&mut self, acc: Access) -> Result<RegisterId, RegisterCompileError> {
        match acc {
            Access::This => {
                let dst = self.registers.allocate(true);
                self.emit(OpCode::LoadThis { dst });
                Ok(dst)
            }
            Access::Global(name) => {
                let name = self.get_sym(name);
                let dst = self.registers.allocate(true);
                let fdbk = self.fdbk();
                self.emit(OpCode::GetGlobal { dst });
                self.emit(OpCode::TryGetById {
                    dst,
                    object: dst,
                    prop: name,
                    fdbk,
                });
                Ok(dst)
            }
            Access::ById(object, name) => {
                let name = self.get_sym(name);
                let dst = self.registers.allocate(true);
                let fdbk = self.fdbk();
                self.emit(OpCode::GetById {
                    dst,
                    object,
                    prop: name,
                    fdbk,
                });
                self.registers.unprotect(object);
                Ok(dst)
            }
            Access::ByVal(object, key) => {
                let dst = self.registers.allocate(true);
                self.emit(OpCode::GetByVal { dst, object, key });
                self.registers.unprotect(key);
                self.registers.unprotect(object);
                Ok(dst)
            }
            Access::Variable(index, depth) => {
                let dst = self.registers.allocate(true);
                let depth = i16::try_from(depth).map_err(|_| {
                    RegisterCompileError::Unsupported(
                        "environment depth exceeds register bytecode limits".into(),
                    )
                })?;
                self.emit(OpCode::GetEnvironment { dst, depth });
                self.emit(OpCode::GetVar {
                    dst,
                    env: dst,
                    at: index as _,
                });
                Ok(dst)
            }
            Access::VariableCurrent(index, _, _) => {
                let dst = self.registers.allocate(true);
                self.emit(OpCode::GetLocal(dst, index));
                Ok(dst)
            }
            Access::ArrayPat(_) => Err(RegisterCompileError::Unsupported(
                "array-pattern reads are not supported by register lowering".into(),
            )),
        }
    }

    pub fn add_literal(&mut self, value: JsValue) -> u32 {
        let block = self.block.compiled_mut();
        let index = block.literals.size();
        block.literals.push_back(self.ctx.heap(), value);
        index
    }

    pub fn instruction_index(&self) -> InstructionIndex {
        InstructionIndex(self.block.compiled().instructions().len() as u32)
    }

    fn register_window(&mut self, count: usize) -> Result<RegisterId, RegisterCompileError> {
        self.registers
            .try_register_window(count)
            .map_err(|error| match error {
                RegisterScopeError::RegisterLimitExceeded {
                    requested,
                    available,
                } => RegisterCompileError::RegisterLimitExceeded {
                    requested,
                    available,
                },
            })
    }

    pub fn compile_expression(
        &mut self,
        expression: &swc_ecmascript::ast::Expr,
    ) -> Result<RegisterId, RegisterCompileError> {
        expression.visit(self)
    }

    pub fn compile_statement(
        &mut self,
        statement: &swc_ecmascript::ast::Stmt,
    ) -> Result<(), RegisterCompileError> {
        use swc_ecmascript::ast::Stmt;

        match statement {
            Stmt::Empty(_) | Stmt::Debugger(_) => Ok(()),
            Stmt::Expr(statement) => {
                self.compile_expression(&statement.expr)?;
                Ok(())
            }
            Stmt::Block(block) => {
                self.registers.push_context();
                self.push_scope();
                self.predeclare_lexical_bindings(&block.stmts)?;
                for statement in &block.stmts {
                    self.compile_statement(statement)?;
                }
                self.pop_scope();
                self.registers.pop_context();
                Ok(())
            }
            Stmt::Decl(declaration) => match declaration {
                swc_ecmascript::ast::Decl::Var(declaration) => {
                    self.compile_var_declaration(declaration)
                }
                _ => Err(RegisterCompileError::Unsupported(format!(
                    "declaration lowering is not implemented for {declaration:?}"
                ))),
            },
            Stmt::Return(statement) => {
                let source = if let Some(expression) = &statement.arg {
                    self.compile_expression(expression)?
                } else {
                    let source = self.registers.allocate(true);
                    self.emit(OpCode::LoadUndefined(source));
                    source
                };
                self.emit(OpCode::Return { source });
                Ok(())
            }
            Stmt::Throw(statement) => {
                let value = self.compile_expression(&statement.arg)?;
                self.emit(OpCode::Throw { value });
                Ok(())
            }
            Stmt::If(statement) => {
                let test = self.compile_expression(&statement.test)?;
                self.registers.protect(test);
                let otherwise = self.emit_branch(BranchKind::JumpIfFalse(test));
                self.compile_statement(&statement.cons)?;
                if let Some(alternate) = &statement.alt {
                    let end = self.emit_branch(BranchKind::Jump);
                    self.patch_branch(otherwise, self.instruction_index())?;
                    self.compile_statement(alternate)?;
                    self.patch_branch(end, self.instruction_index())?;
                } else {
                    self.patch_branch(otherwise, self.instruction_index())?;
                }
                self.registers.unprotect(test);
                Ok(())
            }
            Stmt::While(statement) => {
                let loop_start = self.instruction_index();
                let test = self.compile_expression(&statement.test)?;
                self.registers.protect(test);
                let end = self.emit_branch(BranchKind::JumpIfFalse(test));
                self.registers.unprotect(test);
                self.loop_stack.push(LoopControl::default());
                self.compile_statement(&statement.body)?;
                let loop_control = self.loop_stack.pop().expect("loop context was pushed");
                for site in loop_control.continues {
                    self.patch_branch(site, loop_start)?;
                }
                let back_edge = self.emit_branch(BranchKind::Jump);
                self.patch_branch(back_edge, loop_start)?;
                let loop_end = self.instruction_index();
                self.patch_branch(end, loop_end)?;
                for site in loop_control.breaks {
                    self.patch_branch(site, loop_end)?;
                }
                Ok(())
            }
            Stmt::DoWhile(statement) => {
                let loop_start = self.instruction_index();
                self.loop_stack.push(LoopControl::default());
                self.compile_statement(&statement.body)?;
                let test_start = self.instruction_index();
                let loop_control = self.loop_stack.pop().expect("loop context was pushed");
                for site in loop_control.continues {
                    self.patch_branch(site, test_start)?;
                }
                let test = self.compile_expression(&statement.test)?;
                self.registers.protect(test);
                let back_edge = self.emit_branch(BranchKind::JumpIfTrue(test));
                self.patch_branch(back_edge, loop_start)?;
                self.registers.unprotect(test);
                let loop_end = self.instruction_index();
                for site in loop_control.breaks {
                    self.patch_branch(site, loop_end)?;
                }
                Ok(())
            }
            Stmt::For(statement) => {
                self.push_scope();
                if let Some(swc_ecmascript::ast::VarDeclOrExpr::VarDecl(declaration)) =
                    &statement.init
                {
                    if declaration.kind != swc_ecmascript::ast::VarDeclKind::Var {
                        self.predeclare_lexical_declaration(declaration)?;
                    }
                }
                if let Some(initializer) = &statement.init {
                    match initializer {
                        swc_ecmascript::ast::VarDeclOrExpr::Expr(expression) => {
                            self.compile_expression(expression)?;
                        }
                        swc_ecmascript::ast::VarDeclOrExpr::VarDecl(declaration) => {
                            self.compile_var_declaration(declaration)?;
                        }
                    }
                }
                let loop_start = self.instruction_index();
                let end = if let Some(test) = &statement.test {
                    let test = self.compile_expression(test)?;
                    self.registers.protect(test);
                    let end = self.emit_branch(BranchKind::JumpIfFalse(test));
                    self.registers.unprotect(test);
                    Some(end)
                } else {
                    None
                };
                self.loop_stack.push(LoopControl::default());
                self.compile_statement(&statement.body)?;
                let update_start = self.instruction_index();
                let loop_control = self.loop_stack.pop().expect("loop context was pushed");
                for site in loop_control.continues {
                    self.patch_branch(site, update_start)?;
                }
                if let Some(update) = &statement.update {
                    self.compile_expression(update)?;
                }
                let back_edge = self.emit_branch(BranchKind::Jump);
                self.patch_branch(back_edge, loop_start)?;
                let loop_end = self.instruction_index();
                if let Some(end) = end {
                    self.patch_branch(end, loop_end)?;
                }
                for site in loop_control.breaks {
                    self.patch_branch(site, loop_end)?;
                }
                self.pop_scope();
                Ok(())
            }
            Stmt::Break(statement) if statement.label.is_none() => {
                let site = self.emit_branch(BranchKind::Jump);
                let Some(loop_control) = self.loop_stack.last_mut() else {
                    return Err(RegisterCompileError::Unsupported(
                        "break statement is not inside a loop".into(),
                    ));
                };
                loop_control.breaks.push(site);
                Ok(())
            }
            Stmt::Continue(statement) if statement.label.is_none() => {
                let site = self.emit_branch(BranchKind::Jump);
                let Some(loop_control) = self.loop_stack.last_mut() else {
                    return Err(RegisterCompileError::Unsupported(
                        "continue statement is not inside a loop".into(),
                    ));
                };
                loop_control.continues.push(site);
                Ok(())
            }
            _ => Err(RegisterCompileError::Unsupported(format!(
                "statement lowering is not implemented for {statement:?}"
            ))),
        }
    }

    pub fn compile_statements(
        &mut self,
        statements: &[swc_ecmascript::ast::Stmt],
        parameter_count: u32,
    ) -> Result<(), RegisterCompileError> {
        self.compile_body(statements, parameter_count, true)
    }
}

pub trait Visit {
    type Result;
    fn visit(&self, generator: &mut Generator) -> Self::Result;
    fn visit_mut(&mut self, generator: &mut Generator) -> Self::Result {
        self.visit(generator)
    }
}

impl Visit for swc_ecmascript::ast::Expr {
    type Result = Result<RegisterId, RegisterCompileError>;
    fn visit(&self, generator: &mut Generator) -> Self::Result {
        match self {
            Self::Lit(literal) => literal.visit(generator),
            Self::Ident(ident) => {
                let access = ident.visit(generator)?;
                generator.acess_get(access)
            }
            Self::Paren(paren) => paren.expr.visit(generator),
            Self::Assign(assign) => {
                if assign.op != swc_ecmascript::ast::AssignOp::Assign {
                    return Err(RegisterCompileError::Unsupported(format!(
                        "compound assignment {:?} is not implemented",
                        assign.op
                    )));
                }
                let value = assign.right.visit(generator)?;
                generator.registers.protect(value);
                let access = match &assign.left {
                    swc_ecmascript::ast::PatOrExpr::Expr(expression) => match &**expression {
                        swc_ecmascript::ast::Expr::Ident(ident) => ident.visit(generator)?,
                        swc_ecmascript::ast::Expr::Member(member) => {
                            compile_member_access(member, generator)?
                        }
                        _ => {
                            return Err(RegisterCompileError::Unsupported(
                                "assignment target is not supported by register lowering".into(),
                            ));
                        }
                    },
                    swc_ecmascript::ast::PatOrExpr::Pat(pattern) => match &**pattern {
                        swc_ecmascript::ast::Pat::Ident(ident) => ident.id.visit(generator)?,
                        _ => {
                            return Err(RegisterCompileError::Unsupported(
                                "assignment pattern is not supported by register lowering".into(),
                            ));
                        }
                    },
                };
                generator.access_set(value, access)?;
                generator.registers.unprotect(value);
                Ok(value)
            }
            Self::Bin(binary) => {
                use swc_ecmascript::ast::BinaryOp;

                let lhs = binary.left.visit(generator)?;
                generator.registers.protect(lhs);
                if matches!(
                    binary.op,
                    BinaryOp::LogicalAnd | BinaryOp::LogicalOr | BinaryOp::NullishCoalescing
                ) {
                    let branch_kind = if binary.op == BinaryOp::LogicalAnd {
                        BranchKind::JumpIfFalse(lhs)
                    } else if binary.op == BinaryOp::LogicalOr {
                        BranchKind::JumpIfTrue(lhs)
                    } else {
                        return Err(RegisterCompileError::Unsupported(
                            "nullish coalescing is not implemented by register lowering".into(),
                        ));
                    };
                    let short_circuit = generator.emit_branch(branch_kind);
                    let rhs = binary.right.visit(generator)?;
                    generator.emit(OpCode::Move(lhs, rhs));
                    let end = generator.emit_branch(BranchKind::Jump);
                    let short_circuit_target = generator.instruction_index();
                    generator.patch_branch(short_circuit, short_circuit_target)?;
                    let end_target = generator.instruction_index();
                    generator.patch_branch(end, end_target)?;
                    generator.registers.unprotect(lhs);
                    generator.registers.unprotect(rhs);
                    return Ok(lhs);
                }
                let rhs = binary.right.visit(generator)?;
                let opcode = lower_binary_operator(binary.op, lhs, rhs)?;
                generator.emit(opcode);
                generator.registers.unprotect(lhs);
                generator.registers.unprotect(rhs);
                Ok(lhs)
            }
            Self::Unary(unary) => {
                if unary.op == swc_ecmascript::ast::UnaryOp::Delete {
                    let access = match &*unary.arg {
                        swc_ecmascript::ast::Expr::Ident(ident) => ident.visit(generator)?,
                        swc_ecmascript::ast::Expr::Member(member) => {
                            compile_member_access(member, generator)?
                        }
                        _ => {
                            return Err(RegisterCompileError::Unsupported(
                                "delete operand is not supported by register lowering".into(),
                            ));
                        }
                    };
                    let dest = generator.registers.allocate(true);
                    match access {
                        Access::Global(name) => {
                            let prop = generator.get_sym(name);
                            generator.emit(OpCode::DeleteGlobal { dst: dest, prop });
                        }
                        Access::Variable(_, _) => {
                            if generator.block.compiled().strict {
                                return Err(RegisterCompileError::Unsupported(
                                    "deleting a binding is invalid in strict mode".into(),
                                ));
                            }
                            generator.emit(OpCode::LoadFalse(dest));
                        }
                        Access::VariableCurrent(_, _, _) => {
                            if generator.block.compiled().strict {
                                return Err(RegisterCompileError::Unsupported(
                                    "deleting a binding is invalid in strict mode".into(),
                                ));
                            }
                            generator.emit(OpCode::LoadFalse(dest));
                        }
                        Access::ById(object, name) => {
                            let prop = generator.get_sym(name);
                            generator.emit(OpCode::DeleteById {
                                dst: dest,
                                object,
                                prop,
                            });
                            generator.registers.unprotect(object);
                        }
                        Access::ByVal(object, key) => {
                            generator.emit(OpCode::DeleteByVal {
                                dst: dest,
                                object,
                                key,
                            });
                            generator.registers.unprotect(key);
                            generator.registers.unprotect(object);
                        }
                        Access::This | Access::ArrayPat(_) => {
                            return Err(RegisterCompileError::Unsupported(
                                "delete operand is not supported by register lowering".into(),
                            ));
                        }
                    }
                    return Ok(dest);
                }
                let src = unary.arg.visit(generator)?;
                generator.registers.protect(src);
                let dst = generator.registers.allocate(true);
                let opcode = match unary.op {
                    swc_ecmascript::ast::UnaryOp::Minus => OpCode::Negate { dest: dst, src },
                    swc_ecmascript::ast::UnaryOp::Plus => OpCode::UnaryPlus { dest: dst, src },
                    swc_ecmascript::ast::UnaryOp::Tilde => OpCode::Not { dest: dst, src },
                    swc_ecmascript::ast::UnaryOp::Bang => OpCode::LogicalNot { dest: dst, src },
                    swc_ecmascript::ast::UnaryOp::TypeOf => {
                        generator.emit(OpCode::Move(dst, src));
                        generator.emit(OpCode::TypeOf { src_dest: dst });
                        generator.registers.unprotect(src);
                        return Ok(dst);
                    }
                    swc_ecmascript::ast::UnaryOp::Void => {
                        generator.emit(OpCode::LoadUndefined(dst));
                        generator.registers.unprotect(src);
                        return Ok(dst);
                    }
                    _ => {
                        return Err(RegisterCompileError::Unsupported(format!(
                            "unary operator {:?} is not implemented",
                            unary.op
                        )));
                    }
                };
                generator.emit(opcode);
                generator.registers.unprotect(src);
                Ok(dst)
            }
            Self::This(_) => {
                let dst = generator.registers.allocate(true);
                generator.emit(OpCode::LoadThis { dst });
                Ok(dst)
            }
            Self::Member(member) => {
                let access = compile_member_access(member, generator)?;
                generator.acess_get(access)
            }
            Self::Cond(conditional) => {
                let test = conditional.test.visit(generator)?;
                generator.registers.protect(test);
                let otherwise = generator.emit_branch(BranchKind::JumpIfFalse(test));
                let consequent = conditional.cons.visit(generator)?;
                generator.registers.protect(consequent);
                let dest = generator.registers.allocate(true);
                generator.emit(OpCode::Move(dest, consequent));
                generator.registers.protect(dest);
                let end = generator.emit_branch(BranchKind::Jump);
                generator.patch_branch(otherwise, generator.instruction_index())?;
                let alternate = conditional.alt.visit(generator)?;
                generator.emit(OpCode::Move(dest, alternate));
                generator.patch_branch(end, generator.instruction_index())?;
                generator.registers.unprotect(test);
                generator.registers.unprotect(consequent);
                generator.registers.unprotect(alternate);
                generator.registers.unprotect(dest);
                Ok(dest)
            }
            Self::Seq(sequence) => {
                let mut result = None;
                for expression in &sequence.exprs {
                    result = Some(expression.visit(generator)?);
                }
                result.ok_or_else(|| {
                    RegisterCompileError::Unsupported(
                        "empty sequence expression cannot be lowered".into(),
                    )
                })
            }
            Self::Array(array) => compile_array_literal(array, generator),
            Self::Object(object) => compile_object_literal(object, generator),
            Self::Call(call) => compile_call(call, generator, false),
            Self::New(new_expression) => compile_new(new_expression, generator),
            _ => Err(RegisterCompileError::Unsupported(format!(
                "expression lowering is not implemented for {self:?}"
            ))),
        }
    }
}

impl Visit for swc_ecmascript::ast::Ident {
    type Result = Result<Access, RegisterCompileError>;
    fn visit(&self, generator: &mut Generator) -> Self::Result {
        Ok(generator.access_var(self.sym.intern()))
    }
}

impl Visit for swc_ecmascript::ast::Lit {
    type Result = Result<RegisterId, RegisterCompileError>;
    fn visit(&self, generator: &mut Generator) -> Self::Result {
        match self {
            swc_ecmascript::ast::Lit::Bool(x) => {
                let dst = generator.registers.allocate(true);
                if x.value {
                    generator.emit(OpCode::LoadTrue(dst))
                } else {
                    generator.emit(OpCode::LoadFalse(dst))
                }
                return Ok(dst);
            }
            swc_ecmascript::ast::Lit::Num(x) => {
                let dst = generator.registers.allocate(true);
                if x.value as i32 as f64 == x.value {
                    generator.emit(OpCode::LoadInt(dst, x.value as i32));
                } else {
                    generator.emit(OpCode::LoadDouble(dst, x.value));
                }
                return Ok(dst);
            }
            swc_ecmascript::ast::Lit::Null(_) => {
                let dst = generator.registers.allocate(true);
                generator.emit(OpCode::LoadNull(dst));
                Ok(dst)
            }
            swc_ecmascript::ast::Lit::Str(string) => {
                let value = JsValue::encode_object_value(crate::vm::string::JsString::new(
                    generator.ctx,
                    string.value.to_string(),
                ));
                let index = generator.add_literal(value);
                let dst = generator.registers.allocate(true);
                generator.emit(OpCode::GetConstant(dst, index));
                Ok(dst)
            }
            _ => Err(RegisterCompileError::Unsupported(format!(
                "literal lowering is not implemented for {self:?}"
            ))),
        }
    }
}

fn compile_member_access(
    member: &swc_ecmascript::ast::MemberExpr,
    generator: &mut Generator,
) -> Result<Access, RegisterCompileError> {
    use swc_ecmascript::ast::{Expr, ExprOrSuper};

    let object = match &member.obj {
        ExprOrSuper::Expr(object) => object.visit(generator)?,
        ExprOrSuper::Super(_) => {
            return Err(RegisterCompileError::Unsupported(
                "super property access is not implemented by register lowering".into(),
            ));
        }
    };
    generator.registers.protect(object);
    let access = if member.computed {
        let key = member.prop.visit(generator)?;
        generator.registers.protect(key);
        Access::ByVal(object, key)
    } else {
        match &*member.prop {
            Expr::Ident(property) => Access::ById(object, property.sym.to_string().intern()),
            _ => {
                return Err(RegisterCompileError::Unsupported(
                    "non-computed member property must be an identifier".into(),
                ));
            }
        }
    };
    Ok(access)
}

fn lower_binary_operator(
    operator: swc_ecmascript::ast::BinaryOp,
    lhs: RegisterId,
    rhs: RegisterId,
) -> Result<OpCode, RegisterCompileError> {
    use swc_ecmascript::ast::BinaryOp;

    let dest = lhs;
    Ok(match operator {
        BinaryOp::Add => OpCode::Add { dest, lhs, rhs },
        BinaryOp::Sub => OpCode::Sub { dest, lhs, rhs },
        BinaryOp::Mul => OpCode::Mul { dest, lhs, rhs },
        BinaryOp::Div => OpCode::Div { dest, lhs, rhs },
        BinaryOp::Mod => OpCode::Rem { dest, lhs, rhs },
        BinaryOp::BitAnd => OpCode::And { dest, lhs, rhs },
        BinaryOp::BitOr => OpCode::Or { dest, lhs, rhs },
        BinaryOp::BitXor => OpCode::Xor { dest, lhs, rhs },
        BinaryOp::LShift => OpCode::Shl { dest, lhs, rhs },
        BinaryOp::RShift => OpCode::Shr { dest, lhs, rhs },
        BinaryOp::ZeroFillRShift => OpCode::UShr { dest, lhs, rhs },
        BinaryOp::EqEq => OpCode::Equal { dest, lhs, rhs },
        BinaryOp::NotEq => OpCode::NotEqual { dest, lhs, rhs },
        BinaryOp::EqEqEq => OpCode::StrictEq { dest, lhs, rhs },
        BinaryOp::NotEqEq => OpCode::StrictNEq { dest, lhs, rhs },
        BinaryOp::Gt => OpCode::Greater { dest, lhs, rhs },
        BinaryOp::GtEq => OpCode::GreaterOrEqual { dest, lhs, rhs },
        BinaryOp::Lt => OpCode::Less { dest, lhs, rhs },
        BinaryOp::LtEq => OpCode::LessOrEqual { dest, lhs, rhs },
        BinaryOp::In => OpCode::In { src_dest: lhs, rhs },
        BinaryOp::InstanceOf => OpCode::InstanceOf { src_dest: lhs, rhs },
        _ => {
            return Err(RegisterCompileError::Unsupported(format!(
                "binary operator {operator:?} is not implemented"
            )));
        }
    })
}

fn compile_array_literal(
    array: &swc_ecmascript::ast::ArrayLit,
    generator: &mut Generator,
) -> Result<RegisterId, RegisterCompileError> {
    let mut values = Vec::with_capacity(array.elems.len());
    for element in &array.elems {
        let value = match element {
            Some(element) if element.spread.is_none() => element.expr.visit(generator)?,
            Some(_) => {
                return Err(RegisterCompileError::Unsupported(
                    "array spread is not implemented by register lowering".into(),
                ));
            }
            None => {
                let value = generator.registers.allocate(true);
                generator.emit(OpCode::LoadUndefined(value));
                value
            }
        };
        generator.registers.protect(value);
        values.push(value);
    }

    let source = generator.register_window(values.len())?;
    for (offset, value) in values.iter().enumerate() {
        generator.emit(OpCode::Move(source + offset as RegisterId, *value));
    }
    let dest = generator.registers.allocate(true);
    generator.emit(OpCode::CreateArray {
        dest,
        source,
        length: values.len() as u32,
    });
    generator.registers.unprotect_window(source, values.len());
    for value in values {
        generator.registers.unprotect(value);
    }
    Ok(dest)
}

fn compile_object_literal(
    object: &swc_ecmascript::ast::ObjectLit,
    generator: &mut Generator,
) -> Result<RegisterId, RegisterCompileError> {
    use swc_ecmascript::ast::{Prop, PropName, PropOrSpread};

    let dest = generator.registers.allocate(true);
    generator.emit(OpCode::CreateObject { dest });
    generator.registers.protect(dest);

    for property in &object.props {
        let property = match property {
            PropOrSpread::Prop(property) => property,
            PropOrSpread::Spread(_) => {
                return Err(RegisterCompileError::Unsupported(
                    "object spread is not implemented by register lowering".into(),
                ));
            }
        };
        match &**property {
            Prop::KeyValue(property) => match &property.key {
                PropName::Ident(name) => {
                    let value = property.value.visit(generator)?;
                    generator.registers.protect(value);
                    let prop = generator.get_sym(name.sym.to_string().intern());
                    let fdbk = generator.fdbk();
                    generator.emit(OpCode::PutById {
                        object: dest,
                        value,
                        prop,
                        fdbk,
                    });
                    generator.registers.unprotect(value);
                }
                PropName::Str(name) => {
                    let value = property.value.visit(generator)?;
                    generator.registers.protect(value);
                    let prop = generator.get_sym(name.value.to_string().intern());
                    let fdbk = generator.fdbk();
                    generator.emit(OpCode::PutById {
                        object: dest,
                        value,
                        prop,
                        fdbk,
                    });
                    generator.registers.unprotect(value);
                }
                PropName::Computed(name) => {
                    let key = name.expr.visit(generator)?;
                    generator.registers.protect(key);
                    let value = property.value.visit(generator)?;
                    generator.registers.protect(value);
                    generator.emit(OpCode::PutByVal {
                        object: dest,
                        key,
                        value,
                    });
                    generator.registers.unprotect(value);
                    generator.registers.unprotect(key);
                }
                _ => {
                    return Err(RegisterCompileError::Unsupported(
                        "numeric and bigint object keys are not implemented by register lowering"
                            .into(),
                    ));
                }
            },
            Prop::Shorthand(name) => {
                let access = name.visit(generator)?;
                let value = generator.acess_get(access)?;
                generator.registers.protect(value);
                let prop = generator.get_sym(name.sym.to_string().intern());
                let fdbk = generator.fdbk();
                generator.emit(OpCode::PutById {
                    object: dest,
                    value,
                    prop,
                    fdbk,
                });
                generator.registers.unprotect(value);
            }
            _ => {
                return Err(RegisterCompileError::Unsupported(
                    "object methods and accessors are not implemented by register lowering".into(),
                ));
            }
        }
    }

    generator.registers.unprotect(dest);
    Ok(dest)
}

fn compile_call(
    call: &swc_ecmascript::ast::CallExpr,
    generator: &mut Generator,
    constructor: bool,
) -> Result<RegisterId, RegisterCompileError> {
    use swc_ecmascript::ast::ExprOrSuper;

    let (callee, receiver) = match &call.callee {
        ExprOrSuper::Expr(expression) => match &**expression {
            swc_ecmascript::ast::Expr::Member(member) => {
                let access = compile_member_access(member, generator)?;
                let receiver = match &access {
                    Access::ById(object, _) | Access::ByVal(object, _) => Some(*object),
                    _ => None,
                };
                let callee = generator.acess_get(access)?;
                if let Some(receiver) = receiver {
                    generator.registers.protect(receiver);
                }
                (callee, receiver)
            }
            expression => (expression.visit(generator)?, None),
        },
        ExprOrSuper::Super(_) => {
            return Err(RegisterCompileError::Unsupported(
                "super calls are not implemented by register lowering".into(),
            ));
        }
    };
    generator.registers.protect(callee);

    let member_receiver = receiver;
    let receiver = if constructor {
        callee
    } else if let Some(receiver) = member_receiver {
        receiver
    } else {
        let receiver = generator.registers.allocate(true);
        generator.emit(OpCode::LoadUndefined(receiver));
        receiver
    };
    generator.registers.protect(receiver);

    let mut arguments = Vec::with_capacity(call.args.len());
    for argument in &call.args {
        if argument.spread.is_some() {
            return Err(RegisterCompileError::Unsupported(
                "spread call arguments are not implemented by register lowering".into(),
            ));
        }
        let argument = argument.expr.visit(generator)?;
        generator.registers.protect(argument);
        arguments.push(argument);
    }
    let argc = u16::try_from(arguments.len()).map_err(|_| {
        RegisterCompileError::Unsupported("call has more than u16::MAX arguments".into())
    })?;
    let window_len = arguments.len() + 2;
    let argv = generator.register_window(window_len)?;
    generator.emit(OpCode::Move(argv, callee));
    generator.emit(OpCode::Move(argv + 1, receiver));
    for (offset, argument) in arguments.iter().enumerate() {
        generator.emit(OpCode::Move(argv + offset as RegisterId + 2, *argument));
    }

    let dest = generator.registers.allocate(true);
    if constructor {
        generator.emit(OpCode::CallConstructor { dest, argv, argc });
    } else {
        generator.emit(OpCode::Call {
            receiver: dest,
            argv,
            argc,
        });
    }

    generator.registers.unprotect_window(argv, window_len);
    for argument in arguments {
        generator.registers.unprotect(argument);
    }

    generator.registers.unprotect(receiver);
    generator.registers.unprotect(callee);
    if let Some(receiver) = member_receiver {
        generator.registers.unprotect(receiver);
    }
    Ok(dest)
}

fn compile_new(
    expression: &swc_ecmascript::ast::NewExpr,
    generator: &mut Generator,
) -> Result<RegisterId, RegisterCompileError> {
    let args = expression.args.as_deref().unwrap_or_default();
    let call = swc_ecmascript::ast::CallExpr {
        span: expression.span,
        callee: swc_ecmascript::ast::ExprOrSuper::Expr(expression.callee.clone()),
        args: args.to_vec(),
        type_args: expression.type_args.clone(),
    };
    compile_call(&call, generator, true)
}

#[cfg(test)]
mod register_lowering_tests {
    use super::{lower_binary_operator, RegisterCompileError};
    use crate::bytecode::opcodes::OpCode;
    use swc_ecmascript::ast::BinaryOp;

    #[test]
    fn lowers_binary_operators_to_register_instructions() {
        assert_eq!(
            lower_binary_operator(BinaryOp::Add, 1, 2),
            Ok(OpCode::Add {
                dest: 1,
                lhs: 1,
                rhs: 2,
            })
        );
        assert_eq!(
            lower_binary_operator(BinaryOp::EqEqEq, 3, 4),
            Ok(OpCode::StrictEq {
                dest: 3,
                lhs: 3,
                rhs: 4,
            })
        );
    }

    #[test]
    fn unsupported_binary_operators_return_errors() {
        assert!(matches!(
            lower_binary_operator(BinaryOp::Exp, 0, 1),
            Err(RegisterCompileError::Unsupported(_))
        ));
    }
}
