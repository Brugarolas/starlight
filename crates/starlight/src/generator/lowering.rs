use super::{Generator, RegisterCompileError, VariableKind};
use crate::bytecode::opcodes::OpCode;
use crate::vm::symbol_table::Internable;
use std::{convert::TryFrom, rc::Rc};
use swc_ecmascript::ast::{Decl, Expr, Lit, Pat, Stmt, VarDecl, VarDeclKind, VarDeclOrExpr};

pub(super) fn is_strict(statements: &[Stmt]) -> bool {
    for statement in statements {
        let Stmt::Expr(expression) = statement else {
            break;
        };
        let Expr::Lit(Lit::Str(value)) = &*expression.expr else {
            break;
        };
        if value.value.to_string() == "use strict" {
            return true;
        }
    }
    false
}

impl Generator {
    pub(super) fn compile_body(
        &mut self,
        statements: &[Stmt],
        parameter_count: u32,
        append_implicit_return: bool,
    ) -> Result<(), RegisterCompileError> {
        if parameter_count > u32::from(u16::MAX) + 1 {
            return Err(RegisterCompileError::RegisterLimitExceeded {
                requested: parameter_count as usize,
                available: usize::from(u16::MAX) + 1,
            });
        }
        self.registers.reserve_parameters(parameter_count);
        self.predeclare_var_bindings(statements)?;
        self.predeclare_lexical_bindings(statements)?;
        for statement in statements {
            self.compile_statement(statement)?;
        }
        if append_implicit_return {
            let result = self.registers.allocate(true);
            self.emit(OpCode::LoadUndefined(result));
            self.emit(OpCode::Return { source: result });
        }
        self.finalize_register_bytecode(parameter_count)?;
        Ok(())
    }

    pub(super) fn predeclare_lexical_bindings(
        &mut self,
        statements: &[Stmt],
    ) -> Result<(), RegisterCompileError> {
        for statement in statements {
            if let Stmt::Decl(Decl::Var(declaration)) = statement {
                if declaration.kind != VarDeclKind::Var {
                    self.predeclare_lexical_declaration(declaration)?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn predeclare_lexical_declaration(
        &mut self,
        declaration: &VarDecl,
    ) -> Result<(), RegisterCompileError> {
        let is_global_lexical = self.top_level && Rc::ptr_eq(&self.scope, &self.function_scope);
        let kind = match (declaration.kind, is_global_lexical) {
            (VarDeclKind::Let, false) => VariableKind::Let,
            (VarDeclKind::Const, false) => VariableKind::Const,
            (VarDeclKind::Let, true) => VariableKind::GlobalLet,
            (VarDeclKind::Const, true) => VariableKind::GlobalConst,
            (VarDeclKind::Var, _) => return Ok(()),
        };
        for declarator in &declaration.decls {
            let Pat::Ident(binding) = &declarator.name else {
                return Err(RegisterCompileError::Unsupported(
                    "destructuring declarations are not implemented by register lowering".into(),
                ));
            };
            let name = binding.id.sym.intern();
            if self.scope.borrow().variables.contains_key(&name) {
                return Err(RegisterCompileError::Unsupported(format!(
                    "duplicate lexical declaration for {name:?}"
                )));
            }
            let scope = self.scope.clone();
            let (index, inserted) = self.allocate_binding(&scope, name, kind)?;
            if inserted {
                let env = self.registers.allocate(true);
                self.emit(OpCode::GetEnvironment { dst: env, depth: 0 });
                let declaration = if matches!(kind, VariableKind::Let | VariableKind::GlobalLet) {
                    OpCode::DeclLet { env, at: index }
                } else {
                    OpCode::DeclConst { env, at: index }
                };
                self.emit(declaration);
            }
        }
        Ok(())
    }

    fn predeclare_var_bindings(&mut self, statements: &[Stmt]) -> Result<(), RegisterCompileError> {
        let mut declarations = Vec::new();
        collect_var_declarations(statements, &mut declarations);
        for declaration in declarations {
            for declarator in &declaration.decls {
                let Pat::Ident(binding) = &declarator.name else {
                    return Err(RegisterCompileError::Unsupported(
                        "destructuring declarations are not implemented by register lowering"
                            .into(),
                    ));
                };
                let name = binding.id.sym.intern();
                let kind = if self.top_level {
                    VariableKind::Global
                } else {
                    VariableKind::Var
                };
                let scope = self.function_scope.clone();
                let (_, inserted) = self.allocate_binding(&scope, name, kind)?;
                if inserted && kind == VariableKind::Global {
                    let prop = self.get_sym(name);
                    self.emit(OpCode::DeclareGlobal { prop });
                }
            }
        }
        Ok(())
    }

    pub(super) fn compile_var_declaration(
        &mut self,
        declaration: &VarDecl,
    ) -> Result<(), RegisterCompileError> {
        for declarator in &declaration.decls {
            let Pat::Ident(binding) = &declarator.name else {
                return Err(RegisterCompileError::Unsupported(
                    "destructuring declarations are not implemented by register lowering".into(),
                ));
            };
            let name = binding.id.sym.intern();
            if declaration.kind == VarDeclKind::Const && declarator.init.is_none() {
                return Err(RegisterCompileError::Unsupported(
                    "const declarations require an initializer".into(),
                ));
            }
            let access = self.access_var(name);
            let initializer = match &declarator.init {
                Some(initializer) => Some(self.compile_expression(initializer)?),
                None if declaration.kind != VarDeclKind::Var => {
                    let value = self.registers.allocate(true);
                    self.emit(OpCode::LoadUndefined(value));
                    Some(value)
                }
                None => None,
            };
            let Some(initializer) = initializer else {
                continue;
            };
            self.registers.protect(initializer);
            if declaration.kind == VarDeclKind::Var {
                self.access_set(initializer, access)?;
            } else {
                match access {
                    super::Access::VariableCurrent(index, _, _) => {
                        self.emit(OpCode::InitializeLocal(initializer, index));
                    }
                    super::Access::Variable(index, depth) => {
                        let env = self.registers.allocate(true);
                        let depth = i16::try_from(depth).map_err(|_| {
                            RegisterCompileError::Unsupported(
                                "environment depth exceeds register bytecode limits".into(),
                            )
                        })?;
                        self.emit(OpCode::GetEnvironment { dst: env, depth });
                        self.emit(OpCode::InitializeVar {
                            src: initializer,
                            env,
                            at: index,
                        });
                    }
                    _ => {
                        return Err(RegisterCompileError::Unsupported(
                            "lexical declaration did not resolve to a local binding".into(),
                        ));
                    }
                }
            }
            self.registers.unprotect(initializer);
        }
        Ok(())
    }
}

fn collect_var_declarations<'a>(statements: &'a [Stmt], output: &mut Vec<&'a VarDecl>) {
    for statement in statements {
        match statement {
            Stmt::Decl(Decl::Var(declaration)) if declaration.kind == VarDeclKind::Var => {
                output.push(declaration);
            }
            Stmt::Block(block) => collect_var_declarations(&block.stmts, output),
            Stmt::If(statement) => {
                collect_var_declarations(std::slice::from_ref(&*statement.cons), output);
                if let Some(alternate) = &statement.alt {
                    collect_var_declarations(std::slice::from_ref(&**alternate), output);
                }
            }

            Stmt::While(statement) => {
                collect_var_declarations(std::slice::from_ref(&*statement.body), output);
            }
            Stmt::DoWhile(statement) => {
                collect_var_declarations(std::slice::from_ref(&*statement.body), output);
            }
            Stmt::For(statement) => {
                if let Some(VarDeclOrExpr::VarDecl(declaration)) = &statement.init {
                    if declaration.kind == VarDeclKind::Var {
                        output.push(declaration);
                    }
                }
                collect_var_declarations(std::slice::from_ref(&*statement.body), output);
            }
            Stmt::Labeled(statement) => {
                collect_var_declarations(std::slice::from_ref(&*statement.body), output);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{collect_var_declarations, is_strict};
    use swc_common::{sync::Lrc, FileName, SourceMap};
    use swc_ecmascript::parser::{Parser, StringInput, Syntax};

    fn parse_script(source: &str) -> swc_ecmascript::ast::Script {
        let source_map: Lrc<SourceMap> = Default::default();
        let file = source_map.new_source_file(
            FileName::Custom("<register-lowering-test>".into()),
            source.to_owned(),
        );
        let mut parser = Parser::new(
            Syntax::Es(crate::vm::init_es_config()),
            StringInput::from(&*file),
            None,
        );
        parser.parse_script().expect("test source should parse")
    }

    #[test]
    fn strict_directive_is_only_recognized_in_the_directive_prologue() {
        assert!(is_strict(&parse_script("'use strict'; result = 1;").body));
        assert!(!is_strict(
            &parse_script("'not strict'; result = 1; 'use strict';").body
        ));
        assert!(!is_strict(&parse_script("result = 1; 'use strict';").body));
    }

    #[test]
    fn var_declarations_are_collected_from_nested_statement_bodies() {
        let script = parse_script(
            "var outer; { var nested; let lexical; } if (true) var branch; for (var index;;) {}",
        );
        let mut declarations = Vec::new();
        collect_var_declarations(&script.body, &mut declarations);

        assert_eq!(declarations.len(), 4);
    }
}
