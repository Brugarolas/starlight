use crate::bytecode::opcodes::RegisterId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegisterScopeError {
    RegisterLimitExceeded { requested: usize, available: usize },
}

pub struct RegisterScope {
    registers: Vec<bool>,
    contexts: Vec<Vec<usize>>,
}

impl RegisterScope {
    pub fn new() -> Self {
        Self {
            registers: vec![],
            contexts: vec![vec![]],
        }
    }
    pub fn register(&mut self, reg: usize) -> bool {
        assert!(
            reg <= usize::from(u16::MAX),
            "register bytecode supports at most 65536 registers"
        );
        if reg >= self.registers.len() {
            while self.registers.get(reg).is_none() {
                self.registers.push(false);
            }
        }
        self.registers[reg]
    }

    pub fn allocate(&mut self, temp: bool) -> RegisterId {
        self.try_allocate(temp)
            .expect("register bytecode supports at most 65536 registers")
    }

    pub fn try_allocate(&mut self, temp: bool) -> Result<RegisterId, RegisterScopeError> {
        for i in 0..self.registers.len() {
            if !self.register(i) {
                self.registers[i] = !temp;
                return Ok(i as _);
            }
        }
        let index = self.registers.len();
        if index > usize::from(u16::MAX) {
            return Err(RegisterScopeError::RegisterLimitExceeded {
                requested: 1,
                available: usize::from(u16::MAX) + 1 - index,
            });
        }
        let r = self.register(index);
        assert!(!r);
        self.registers[index] = !temp;
        Ok(index as _)
    }

    pub fn reserve_parameters(&mut self, count: u32) {
        assert!(
            count <= u32::from(u16::MAX) + 1,
            "register bytecode supports at most 65536 parameter registers"
        );
        for index in 0..count {
            let register = index as RegisterId;
            self.register(index as usize);
            self.protect(register);
        }
    }

    pub fn register_count(&self) -> u32 {
        self.registers.len() as u32
    }

    pub fn pop_context(&mut self) {
        if let Some(ctx) = self.contexts.pop() {
            for reg in ctx.iter() {
                if !self.contexts.iter().any(|context| context.contains(reg)) {
                    self.registers[*reg] = false;
                }
            }
        }
    }

    pub fn push_context(&mut self) {
        self.contexts.push(vec![]);
    }

    pub fn protect(&mut self, reg: RegisterId) {
        self.contexts.last_mut().unwrap().push(reg as _);
        self.registers[reg as usize] = true;
    }

    pub fn unprotect(&mut self, reg: RegisterId) {
        let reg = reg as usize;
        self.contexts.last_mut().unwrap().retain(|x| *x != reg);
        if !self.contexts.iter().any(|context| context.contains(&reg)) {
            self.registers[reg] = false;
        }
    }

    pub fn register_window(&mut self, argc: usize) -> RegisterId {
        self.try_register_window(argc)
            .expect("register window exceeds register capacity")
    }

    pub fn try_register_window(&mut self, argc: usize) -> Result<RegisterId, RegisterScopeError> {
        if argc == 0 {
            return Ok(self.registers.len() as _);
        }

        let mut start = 0;
        let mut found = 0;
        for reg in 0..self.registers.len() {
            if !self.registers[reg] {
                if found == 0 {
                    start = reg;
                }
                found += 1;
                if found == argc {
                    for register in start..start + argc {
                        self.registers[register] = true;
                    }
                    return Ok(start as _);
                }
            } else {
                found = 0;
            }
        }

        let start = self.registers.len();
        if start + argc > usize::from(u16::MAX) + 1 {
            return Err(RegisterScopeError::RegisterLimitExceeded {
                requested: argc,
                available: usize::from(u16::MAX) + 1 - start,
            });
        }
        for i in 0..argc {
            self.register(start + i);
            self.registers[start + i] = true;
        }
        Ok(start as _)
    }

    pub fn unprotect_window(&mut self, argv: RegisterId, argc: usize) {
        for i in 0..argc {
            let reg = argv as usize + i;
            if !self.contexts.iter().any(|context| context.contains(&reg)) {
                self.registers[reg] = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RegisterScope;

    #[test]
    fn register_window_allocates_exact_number_of_contiguous_registers() {
        let mut scope = RegisterScope::new();

        let one = scope.register_window(1);
        let next = scope.register_window(2);

        assert_eq!(one, 0);
        assert_eq!(next, 1);
        assert!(scope.register(0));
        assert!(scope.register(1));
        assert!(scope.register(2));
    }

    #[test]
    fn register_window_reuses_contiguous_free_registers() {
        let mut scope = RegisterScope::new();
        assert_eq!(scope.allocate(false), 0);
        assert_eq!(scope.allocate(false), 1);
        assert_eq!(scope.allocate(false), 2);
        scope.unprotect(1);

        assert_eq!(scope.register_window(1), 1);
        assert!(scope.register(1));
        assert!(scope.register(2));
    }

    #[test]
    fn zero_argument_window_does_not_reserve_a_register() {
        let mut scope = RegisterScope::new();
        let start = scope.register_window(0);

        assert_eq!(start, 0);
        assert!(!scope.register(0));
    }

    #[test]
    fn popping_nested_context_preserves_outer_protection() {
        let mut scope = RegisterScope::new();
        let register = scope.allocate(true);
        scope.push_context();
        scope.protect(register);
        scope.push_context();
        scope.protect(register);

        scope.pop_context();

        assert!(scope.register(register as usize));
        scope.pop_context();
        assert!(!scope.register(register as usize));
    }

    #[test]
    fn unprotecting_nested_context_preserves_outer_protection() {
        let mut scope = RegisterScope::new();
        let register = scope.allocate(true);
        scope.push_context();
        scope.protect(register);
        scope.push_context();
        scope.protect(register);

        scope.unprotect(register);

        assert!(scope.register(register as usize));
        scope.pop_context();
        assert!(scope.register(register as usize));
        scope.pop_context();
        assert!(!scope.register(register as usize));
    }

    #[test]
    fn parameter_registers_are_reserved_before_temporary_allocation() {
        let mut scope = RegisterScope::new();
        scope.reserve_parameters(3);

        assert_eq!(scope.register_count(), 3);
        assert_eq!(scope.allocate(true), 3);
        assert!(scope.register(0));
        assert!(scope.register(1));
        assert!(scope.register(2));
    }

    #[test]
    fn protected_expression_result_survives_nested_allocation() {
        let mut scope = RegisterScope::new();
        let test = scope.allocate(true);
        scope.protect(test);
        let consequent = scope.allocate(true);
        scope.protect(consequent);
        let destination = scope.allocate(true);
        scope.protect(destination);

        scope.unprotect(test);
        scope.unprotect(consequent);
        let alternate = scope.allocate(true);

        assert_ne!(alternate, destination);
    }

    #[test]
    fn oversized_window_reports_capacity_instead_of_panicking() {
        assert_eq!(
            RegisterScope::new().try_register_window(65_537),
            Err(super::RegisterScopeError::RegisterLimitExceeded {
                requested: 65_537,
                available: 65_536,
            })
        );
    }

    #[test]
    fn allocation_reports_register_limit_without_panicking() {
        let mut scope = RegisterScope::new();
        scope.registers.resize(usize::from(u16::MAX) + 1, true);

        assert_eq!(
            scope.try_allocate(true),
            Err(super::RegisterScopeError::RegisterLimitExceeded {
                requested: 1,
                available: 0,
            })
        );
    }
}
