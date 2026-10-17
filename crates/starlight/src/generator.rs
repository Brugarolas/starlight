use std::{cell::RefCell, collections::HashMap, rc::Rc};

use crate::{
    bytecode::{
        block::{BranchKind, BranchSite, BytecodeBlock, BytecodeValidationError, InstructionIndex},
        opcodes::{OpCode, RegisterId},
    },
    gc::GcPointer,
    vm::{
        symbol_table::{Internable, Symbol},
        value::JsValue,
    },
};
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

pub enum VariableKind {
    Let,
    Const,
    Var,
    Global,
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

use self::register_scope::RegisterScope;
pub mod register_scope;
pub struct Generator {
    pub block: GcPointer<BytecodeBlock>,
    pub parent: *mut Self,
    pub registers: RegisterScope,
    pub scope: ScopeRef,
    /// Freelist of unused variable indexes
    pub variable_freelist: Vec<u32>,
    pub name_map: HashMap<Symbol, u32>,
}
impl Generator {
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
        block.validate()
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
            let cur_depth = self.scope.borrow().depth;
            let depth = cur_depth - scope.borrow().depth;
            if depth == 0 {
                return Access::VariableCurrent(ix, depth, scope.borrow().env);
            }
            Access::Variable(ix, depth)
        } else {
            Access::Global(var)
        }
    }

    pub fn access_set(&mut self, src: RegisterId, acc: Access) -> Result<(), JsValue> {
        match acc {
            Access::Variable(index, depth) => {
                let dst = self.registers.allocate(true);
                self.emit(OpCode::GetEnvironment {
                    dst,
                    depth: depth as _,
                });
                self.emit(OpCode::SetVar {
                    src,
                    env: dst,
                    at: index,
                });
            }
            Access::VariableCurrent(index, _, env) => {
                self.emit(OpCode::SetVar {
                    src,
                    env,
                    at: index,
                });
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
                })
            }
            Access::This => {
                self.emit(OpCode::StoreThis { src });
            }

            _ => todo!(),
        }
        Ok(())
    }

    pub fn acess_get(&mut self, acc: Access) -> Result<RegisterId, JsValue> {
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
                Ok(dst)
            }
            Access::ByVal(object, key) => {
                let dst = self.registers.allocate(true);
                self.emit(OpCode::GetByVal { dst, object, key });
                Ok(dst)
            }
            Access::Variable(index, depth) => {
                let dst = self.registers.allocate(true);
                self.emit(OpCode::GetEnvironment {
                    dst,
                    depth: depth as _,
                });
                self.emit(OpCode::GetVar {
                    dst,
                    env: dst,
                    at: index as _,
                });
                Ok(dst)
            }
            Access::VariableCurrent(index, _, env) => {
                let dst = self.registers.allocate(true);
                self.emit(OpCode::GetVar {
                    dst,
                    env,
                    at: index as _,
                });
                Ok(dst)
            }
            _ => todo!(),
        }
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
    type Result = Result<RegisterId, JsValue>;
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
                    todo!("Register compound-assignment code generation")
                }
                let value = assign.right.visit(generator)?;
                generator.registers.protect(value);
                let access = match &assign.left {
                    swc_ecmascript::ast::PatOrExpr::Expr(expression) => match &**expression {
                        swc_ecmascript::ast::Expr::Ident(ident) => ident.visit(generator)?,
                        _ => todo!("Register assignment target code generation"),
                    },
                    swc_ecmascript::ast::PatOrExpr::Pat(pattern) => match &**pattern {
                        swc_ecmascript::ast::Pat::Ident(ident) => ident.id.visit(generator)?,
                        _ => todo!("Register assignment pattern code generation"),
                    },
                };
                generator.access_set(value, access)?;
                generator.registers.unprotect(value);
                Ok(value)
            }
            Self::Bin(binary) => {
                let lhs = binary.left.visit(generator)?;
                generator.registers.protect(lhs);
                let rhs = binary.right.visit(generator)?;
                let opcode = match binary.op {
                    swc_ecmascript::ast::BinaryOp::Add => OpCode::Add {
                        dest: lhs,
                        lhs,
                        rhs,
                    },
                    swc_ecmascript::ast::BinaryOp::Sub => OpCode::Sub {
                        dest: lhs,
                        lhs,
                        rhs,
                    },
                    swc_ecmascript::ast::BinaryOp::Mul => OpCode::Mul {
                        dest: lhs,
                        lhs,
                        rhs,
                    },
                    swc_ecmascript::ast::BinaryOp::Div => OpCode::Div {
                        dest: lhs,
                        lhs,
                        rhs,
                    },
                    swc_ecmascript::ast::BinaryOp::Mod => OpCode::Rem {
                        dest: lhs,
                        lhs,
                        rhs,
                    },
                    swc_ecmascript::ast::BinaryOp::BitAnd => OpCode::And {
                        dest: lhs,
                        lhs,
                        rhs,
                    },
                    swc_ecmascript::ast::BinaryOp::BitOr => OpCode::Or {
                        dest: lhs,
                        lhs,
                        rhs,
                    },
                    swc_ecmascript::ast::BinaryOp::BitXor => OpCode::Xor {
                        dest: lhs,
                        lhs,
                        rhs,
                    },
                    swc_ecmascript::ast::BinaryOp::LShift => OpCode::Shl {
                        dest: lhs,
                        lhs,
                        rhs,
                    },
                    swc_ecmascript::ast::BinaryOp::RShift => OpCode::Shr {
                        dest: lhs,
                        lhs,
                        rhs,
                    },
                    swc_ecmascript::ast::BinaryOp::ZeroFillRShift => OpCode::UShr {
                        dest: lhs,
                        lhs,
                        rhs,
                    },
                    swc_ecmascript::ast::BinaryOp::EqEq => OpCode::Equal {
                        dest: lhs,
                        lhs,
                        rhs,
                    },
                    swc_ecmascript::ast::BinaryOp::NotEq => OpCode::NotEqual {
                        dest: lhs,
                        lhs,
                        rhs,
                    },
                    swc_ecmascript::ast::BinaryOp::EqEqEq => OpCode::StrictEq {
                        dest: lhs,
                        lhs,
                        rhs,
                    },
                    swc_ecmascript::ast::BinaryOp::NotEqEq => OpCode::StrictNEq {
                        dest: lhs,
                        lhs,
                        rhs,
                    },
                    swc_ecmascript::ast::BinaryOp::Gt => OpCode::Greater {
                        dest: lhs,
                        lhs,
                        rhs,
                    },
                    swc_ecmascript::ast::BinaryOp::GtEq => OpCode::GreaterOrEqual {
                        dest: lhs,
                        lhs,
                        rhs,
                    },
                    swc_ecmascript::ast::BinaryOp::Lt => OpCode::Less {
                        dest: lhs,
                        lhs,
                        rhs,
                    },
                    swc_ecmascript::ast::BinaryOp::LtEq => OpCode::LessOrEqual {
                        dest: lhs,
                        lhs,
                        rhs,
                    },
                    _ => todo!("Unsupported register binary operator: {:?}", binary.op),
                };
                generator.emit(opcode);
                generator.registers.unprotect(lhs);
                generator.registers.unprotect(rhs);
                Ok(lhs)
            }
            Self::Unary(unary) => {
                let src = unary.arg.visit(generator)?;
                generator.registers.protect(src);
                let dst = generator.registers.allocate(true);
                let opcode = match unary.op {
                    swc_ecmascript::ast::UnaryOp::Minus => OpCode::Negate { dest: dst, src },
                    swc_ecmascript::ast::UnaryOp::Plus => OpCode::UnaryPlus { dest: dst, src },
                    swc_ecmascript::ast::UnaryOp::Tilde => OpCode::Not { dest: dst, src },
                    swc_ecmascript::ast::UnaryOp::Bang => OpCode::LogicalNot { dest: dst, src },
                    _ => todo!("Unsupported register unary operator: {:?}", unary.op),
                };
                generator.emit(opcode);
                generator.registers.unprotect(src);
                Ok(dst)
            }
            _ => todo!(),
        }
    }
}

impl Visit for swc_ecmascript::ast::Ident {
    type Result = Result<Access, JsValue>;
    fn visit(&self, generator: &mut Generator) -> Self::Result {
        Ok(generator.access_var(self.sym.intern()))
    }
}

impl Visit for swc_ecmascript::ast::Lit {
    type Result = Result<RegisterId, JsValue>;
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
            _ => todo!(),
        }
    }
}
