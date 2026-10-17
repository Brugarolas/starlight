use crate::{
    bytecode::{block::BytecodeBlock, opcodes::RegisterId},
    gc::cell::{GcPointer, Trace, Visitor},
    vm::{environment::Environment, value::JsValue},
};

pub const MAX_REGISTER_COUNT: u32 = 65_536;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegisterFrameError {
    RegisterCountTooLarge(u32),
    ParameterCountExceedsRegisters { parameters: u32, registers: u32 },
    RegisterIndexOutOfRange { register: RegisterId, count: u32 },
    SnapshotDoesNotMatchBytecode { snapshot: u32, bytecode: u32 },
    AllocationFailed,
}

fn initialized_registers(
    register_count: u32,
    parameter_count: u32,
) -> Result<Vec<JsValue>, RegisterFrameError> {
    if register_count > MAX_REGISTER_COUNT {
        return Err(RegisterFrameError::RegisterCountTooLarge(register_count));
    }
    if parameter_count > register_count {
        return Err(RegisterFrameError::ParameterCountExceedsRegisters {
            parameters: parameter_count,
            registers: register_count,
        });
    }

    let mut registers = Vec::new();
    registers
        .try_reserve_exact(register_count as usize)
        .map_err(|_| RegisterFrameError::AllocationFailed)?;
    registers.resize(register_count as usize, JsValue::encode_undefined_value());
    Ok(registers)
}

/// State owned by a register-bytecode invocation. Every slot is traced, including
/// slots currently containing `undefined`, so future register reuse is safe.
pub struct RegisterFrame {
    pub caller: *mut RegisterFrame,
    registers: Vec<JsValue>,
    pub callee: JsValue,
    pub this: JsValue,
    pub new_target: JsValue,
    pub code_block: GcPointer<BytecodeBlock>,
    pub environment: GcPointer<Environment>,
    /// Instruction index, rather than a pointer into a relocatable instruction vector.
    pub pc: u32,
    pub return_register: Option<RegisterId>,
    pub is_constructor: bool,
}

impl RegisterFrame {
    pub fn new(
        callee: JsValue,
        this: JsValue,
        new_target: JsValue,
        code_block: GcPointer<BytecodeBlock>,
        environment: GcPointer<Environment>,
        return_register: Option<RegisterId>,
        is_constructor: bool,
    ) -> Result<Self, RegisterFrameError> {
        let bytecode = code_block.compiled();
        Ok(Self {
            caller: std::ptr::null_mut(),
            registers: initialized_registers(
                bytecode.register_count(),
                bytecode.parameter_count(),
            )?,
            callee,
            this,
            new_target,
            code_block,
            environment,
            pc: 0,
            return_register,
            is_constructor,
        })
    }

    pub fn register_count(&self) -> u32 {
        self.registers.len() as u32
    }

    pub fn registers(&self) -> &[JsValue] {
        &self.registers
    }

    pub fn registers_mut(&mut self) -> &mut [JsValue] {
        &mut self.registers
    }

    pub fn register(&self, index: RegisterId) -> Option<JsValue> {
        self.registers.get(usize::from(index)).copied()
    }

    pub fn set_register(
        &mut self,
        index: RegisterId,
        value: JsValue,
    ) -> Result<(), RegisterFrameError> {
        let count = self.register_count();
        let register = self.registers.get_mut(usize::from(index)).ok_or(
            RegisterFrameError::RegisterIndexOutOfRange {
                register: index,
                count,
            },
        )?;
        *register = value;
        Ok(())
    }

    pub fn snapshot(&self) -> Result<RegisterFrameSnapshot, RegisterFrameError> {
        let mut registers = Vec::new();
        registers
            .try_reserve_exact(self.registers.len())
            .map_err(|_| RegisterFrameError::AllocationFailed)?;
        registers.extend_from_slice(&self.registers);

        Ok(RegisterFrameSnapshot {
            registers,
            callee: self.callee,
            this: self.this,
            new_target: self.new_target,
            code_block: self.code_block,
            environment: self.environment,
            pc: self.pc,
            return_register: self.return_register,
            is_constructor: self.is_constructor,
        })
    }

    pub fn restore(snapshot: &RegisterFrameSnapshot) -> Result<Self, RegisterFrameError> {
        let bytecode_register_count = snapshot.code_block.compiled().register_count();
        if snapshot.registers.len() != bytecode_register_count as usize {
            return Err(RegisterFrameError::SnapshotDoesNotMatchBytecode {
                snapshot: snapshot.registers.len() as u32,
                bytecode: bytecode_register_count,
            });
        }
        let mut frame = Self {
            caller: std::ptr::null_mut(),
            registers: initialized_registers(
                bytecode_register_count,
                snapshot.code_block.compiled().parameter_count(),
            )?,
            callee: snapshot.callee,
            this: snapshot.this,
            new_target: snapshot.new_target,
            code_block: snapshot.code_block,
            environment: snapshot.environment,
            pc: snapshot.pc,
            return_register: snapshot.return_register,
            is_constructor: snapshot.is_constructor,
        };
        frame.registers.copy_from_slice(&snapshot.registers);
        Ok(frame)
    }
}

impl Trace for RegisterFrame {
    fn trace(&self, visitor: &mut Visitor) {
        self.registers.trace(visitor);
        self.callee.trace(visitor);
        self.this.trace(visitor);
        self.new_target.trace(visitor);
        self.code_block.trace(visitor);
        self.environment.trace(visitor);
    }
}

/// Owned, GC-traceable state for a suspended register frame.
///
/// A snapshot omits the raw caller pointer. Restoring it creates a detached frame
/// that the register stack can attach to the active caller when resumed.
pub struct RegisterFrameSnapshot {
    registers: Vec<JsValue>,
    callee: JsValue,
    this: JsValue,
    new_target: JsValue,
    code_block: GcPointer<BytecodeBlock>,
    environment: GcPointer<Environment>,
    pc: u32,
    return_register: Option<RegisterId>,
    is_constructor: bool,
}

impl RegisterFrameSnapshot {
    pub fn register_count(&self) -> u32 {
        self.registers.len() as u32
    }

    pub fn registers(&self) -> &[JsValue] {
        &self.registers
    }

    pub fn pc(&self) -> u32 {
        self.pc
    }
}

impl Trace for RegisterFrameSnapshot {
    fn trace(&self, visitor: &mut Visitor) {
        self.registers.trace(visitor);
        self.callee.trace(visitor);
        self.this.trace(visitor);
        self.new_target.trace(visitor);
        self.code_block.trace(visitor);
        self.environment.trace(visitor);
    }
}

#[cfg(test)]
mod tests {
    use super::{initialized_registers, RegisterFrameError};
    #[test]
    fn register_slots_are_initialized_to_undefined() {
        let registers = initialized_registers(4, 2).unwrap();

        assert_eq!(registers.len(), 4);
        assert!(registers.iter().all(|value| value.is_undefined()));
    }

    #[test]
    fn register_count_respects_the_register_id_range() {
        assert_eq!(
            initialized_registers(u32::from(u16::MAX) + 2, 0).unwrap_err(),
            RegisterFrameError::RegisterCountTooLarge(u32::from(u16::MAX) + 2)
        );
    }

    #[test]
    fn parameter_count_must_fit_the_register_file() {
        assert_eq!(
            initialized_registers(2, 3).unwrap_err(),
            RegisterFrameError::ParameterCountExceedsRegisters {
                parameters: 3,
                registers: 2,
            }
        );
    }

    #[test]
    fn zero_register_frame_is_valid() {
        assert!(initialized_registers(0, 0).unwrap().is_empty());
    }
}
