use comet::internal::{finalize_trait::FinalizeTrait, trace_trait::TraceTrait};

use super::{
    opcodes::{OpCode, RegisterId},
    TypeFeedBack,
};
use crate::{
    gc::{GcCell, GcPointer},
    vm::{array_storage::ArrayStorage, structure::Structure, symbol_table::Symbol},
};
use std::convert::TryFrom;

/// Zero-based program counter measured in register instructions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InstructionIndex(pub u32);

/// Position of a branch instruction whose target is not known yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BranchSite(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BranchKind {
    Jump,
    JumpIfTrue(RegisterId),
    JumpIfFalse(RegisterId),
    EnterTry,
    ForInInit {
        iterator: RegisterId,
        source: RegisterId,
    },
    ForInNext {
        iterator: RegisterId,
        dest: RegisterId,
    },
    ForOfNext {
        iterator: RegisterId,
        dest: RegisterId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BytecodeValidationError {
    RegisterCountTooLarge(u32),
    ParameterCountExceedsRegisters {
        parameters: u32,
        registers: u32,
    },
    RegisterOutOfRange {
        register: RegisterId,
        count: u32,
    },
    RegisterWindowOutOfRange {
        start: RegisterId,
        slots: u32,
        count: u32,
    },
    PoolIndexOutOfRange {
        pool: &'static str,
        index: u32,
        len: usize,
    },
    BranchTargetOutOfRange {
        pc: u32,
        target: i64,
        len: usize,
    },
    BranchSiteOutOfRange {
        pc: u32,
        len: usize,
    },
    BranchOffsetOutOfRange {
        pc: u32,
        target: u32,
    },
    BranchKindMismatch {
        pc: u32,
    },
}

fn relative_branch_offset(pc: u32, target: u32) -> Result<i32, BytecodeValidationError> {
    let offset = i64::from(target) - (i64::from(pc) + 1);
    i32::try_from(offset)
        .map_err(|_| BytecodeValidationError::BranchOffsetOutOfRange { pc, target })
}

fn resolve_branch_target(
    pc: u32,
    offset: i32,
    instruction_count: usize,
) -> Result<usize, BytecodeValidationError> {
    let target = i64::from(pc) + 1 + i64::from(offset);
    if target < 0 || target >= instruction_count as i64 {
        Err(BytecodeValidationError::BranchTargetOutOfRange {
            pc,
            target,
            len: instruction_count,
        })
    } else {
        Ok(target as usize)
    }
}

fn patch_branch_instruction(
    instruction: &mut OpCode,
    pc: u32,
    target: InstructionIndex,
) -> Result<(), BytecodeValidationError> {
    let offset = relative_branch_offset(pc, target.0)?;
    match instruction {
        OpCode::Jump { offset: branch } => *branch = offset,
        OpCode::JumpIfTrue { offset: branch, .. } | OpCode::JumpIfFalse { offset: branch, .. } => {
            *branch = offset
        }
        OpCode::EnterTry { catch_offset } => *catch_offset = offset,
        OpCode::ForInInit {
            done_offset: branch,
            ..
        }
        | OpCode::ForInNext {
            done_offset: branch,
            ..
        }
        | OpCode::ForOfNext {
            done_offset: branch,
            ..
        } => *branch = offset,
        _ => return Err(BytecodeValidationError::BranchKindMismatch { pc }),
    }
    Ok(())
}

pub enum BytecodeBlock {
    Compiled(BytecodeBlockInternal),
}
impl BytecodeBlock {
    pub fn compiled(&self) -> &BytecodeBlockInternal {
        match self {
            Self::Compiled(x) => x,
            _ => unreachable!(),
        }
    }

    pub fn compiled_mut(&mut self) -> &mut BytecodeBlockInternal {
        match self {
            Self::Compiled(x) => x,
            _ => unreachable!(),
        }
    }
}
impl GcCell for BytecodeBlock {}
impl TraceTrait for BytecodeBlock {
    fn trace(&self, vis: &mut comet::visitor::Visitor) {
        match self {
            Self::Compiled(ref block) => {
                block.trace(vis);
            }
            _ => (),
        }
    }
}

pub struct BytecodeBlockInternal {
    /// Register instruction stream. This typed IR is the canonical representation;
    /// it has no persisted or platform-dependent byte encoding.
    pub(crate) code: Vec<OpCode>,
    /// Number of allocated register slots, including reserved parameter registers.
    pub(crate) register_count: u32,
    /// Input parameters occupy registers `0..parameter_count`.
    pub(crate) parameter_count: u32,
    pub(crate) literals: GcPointer<ArrayStorage>,
    pub(crate) parent_blocks: Vec<GcPointer<BytecodeBlock>>,
    pub(crate) structures: Vec<GcPointer<Structure>>,
    pub(crate) feedback: Vec<TypeFeedBack>,
    pub(crate) names: Vec<Symbol>,
}

impl BytecodeBlockInternal {
    pub fn instructions(&self) -> &[OpCode] {
        &self.code
    }

    pub fn register_count(&self) -> u32 {
        self.register_count
    }

    pub fn parameter_count(&self) -> u32 {
        self.parameter_count
    }

    pub fn emit(&mut self, instruction: OpCode) -> InstructionIndex {
        let index = u32::try_from(self.code.len()).expect("register bytecode exceeds u32 PCs");
        self.code.push(instruction);
        InstructionIndex(index)
    }

    pub fn emit_branch(&mut self, kind: BranchKind) -> BranchSite {
        let index = u32::try_from(self.code.len()).expect("register bytecode exceeds u32 PCs");
        let instruction = match kind {
            BranchKind::Jump => OpCode::Jump { offset: 0 },
            BranchKind::JumpIfTrue(source) => OpCode::JumpIfTrue { source, offset: 0 },
            BranchKind::JumpIfFalse(source) => OpCode::JumpIfFalse { source, offset: 0 },
            BranchKind::EnterTry => OpCode::EnterTry { catch_offset: 0 },
            BranchKind::ForInInit { iterator, source } => OpCode::ForInInit {
                iterator,
                source,
                done_offset: 0,
            },
            BranchKind::ForInNext { iterator, dest } => OpCode::ForInNext {
                iterator,
                dest,
                done_offset: 0,
            },
            BranchKind::ForOfNext { iterator, dest } => OpCode::ForOfNext {
                iterator,
                dest,
                done_offset: 0,
            },
        };
        self.code.push(instruction);
        BranchSite(index)
    }

    /// Patches an offset relative to the instruction after `site`.
    pub fn patch_branch(
        &mut self,
        site: BranchSite,
        target: InstructionIndex,
    ) -> Result<(), BytecodeValidationError> {
        let pc = site.0;
        let len = self.code.len();
        let Some(instruction) = self.code.get_mut(pc as usize) else {
            return Err(BytecodeValidationError::BranchSiteOutOfRange { pc, len });
        };
        patch_branch_instruction(instruction, pc, target)
    }

    /// Validates register operands, pool references, and instruction-relative targets.
    pub fn validate(&self) -> Result<(), BytecodeValidationError> {
        if self.register_count > u32::from(u16::MAX) + 1 {
            return Err(BytecodeValidationError::RegisterCountTooLarge(
                self.register_count,
            ));
        }
        if self.parameter_count > self.register_count {
            return Err(BytecodeValidationError::ParameterCountExceedsRegisters {
                parameters: self.parameter_count,
                registers: self.register_count,
            });
        }

        let check_register = |register: RegisterId| {
            if u32::from(register) >= self.register_count {
                Err(BytecodeValidationError::RegisterOutOfRange {
                    register,
                    count: self.register_count,
                })
            } else {
                Ok(())
            }
        };
        let check_window = |start: RegisterId, slots: u32| {
            if u64::from(start) + u64::from(slots) > u64::from(self.register_count) {
                Err(BytecodeValidationError::RegisterWindowOutOfRange {
                    start,
                    slots,
                    count: self.register_count,
                })
            } else {
                Ok(())
            }
        };
        let check_pool = |pool: &'static str, index: u32, len: usize| {
            if index as usize >= len {
                Err(BytecodeValidationError::PoolIndexOutOfRange { pool, index, len })
            } else {
                Ok(())
            }
        };
        let check_branch =
            |pc: u32, offset: i32| resolve_branch_target(pc, offset, self.code.len()).map(|_| ());

        for (pc, instruction) in self.code.iter().enumerate() {
            let pc = pc as u32;
            match instruction {
                OpCode::Move(dst, src) | OpCode::Swap(dst, src) => {
                    check_register(*dst)?;
                    check_register(*src)?;
                }
                OpCode::GetLocal(dst, _)
                | OpCode::GetConstant(dst, _)
                | OpCode::GetFunction(dst, _) => {
                    check_register(*dst)?;
                    match instruction {
                        OpCode::GetConstant(_, index) => {
                            check_pool("literal", *index, self.literals.size() as usize)?
                        }
                        OpCode::GetFunction(_, index) => {
                            check_pool("function", *index, self.parent_blocks.len())?
                        }
                        _ => (),
                    }
                }
                OpCode::SetLocal(src, _) | OpCode::LoadInt(src, _) | OpCode::LoadDouble(src, _) => {
                    check_register(*src)?
                }
                OpCode::LoadFalse(dst)
                | OpCode::LoadTrue(dst)
                | OpCode::LoadNull(dst)
                | OpCode::LoadUndefined(dst)
                | OpCode::LoadNaN(dst)
                | OpCode::CreateObject { dest: dst }
                | OpCode::GetGlobal { dst }
                | OpCode::GlobalThis { dst } => check_register(*dst)?,
                OpCode::Nop => (),
                OpCode::Call {
                    receiver,
                    argv,
                    argc,
                }
                | OpCode::CallBuiltin {
                    dest: receiver,
                    argv,
                    argc,
                    ..
                } => {
                    check_register(*receiver)?;
                    check_window(*argv, u32::from(*argc) + 2)?;
                }
                OpCode::TailCall { argv, argc } => {
                    check_window(*argv, u32::from(*argc) + 2)?;
                }
                OpCode::CallConstructor { dest, argv, argc } => {
                    check_register(*dest)?;
                    check_window(*argv, u32::from(*argc) + 2)?;
                }
                OpCode::CreateArray {
                    dest,
                    source,
                    length,
                } => {
                    check_register(*dest)?;
                    check_window(*source, *length)?;
                }
                OpCode::CreateArrayFromIterable { dest, source }
                | OpCode::Spread {
                    dst: dest,
                    src: source,
                }
                | OpCode::NewGenerator {
                    dest,
                    function: source,
                } => {
                    check_register(*dest)?;
                    check_register(*source)?;
                }
                OpCode::CreateObjectWithStructure { dest, structure } => {
                    check_register(*dest)?;
                    check_pool("structure", *structure, self.structures.len())?;
                }
                OpCode::LoadThis { dst: dest } | OpCode::GetEnvironment { dst: dest, .. } => {
                    check_register(*dest)?
                }
                OpCode::GetVar { dst, env, .. } => {
                    check_register(*dst)?;
                    check_register(*env)?;
                }
                OpCode::DeleteVar { dst, env, .. } => {
                    check_register(*dst)?;
                    check_register(*env)?;
                }
                OpCode::DeleteById { dst, object, prop } => {
                    check_register(*dst)?;
                    check_register(*object)?;
                    check_pool("name", *prop, self.names.len())?;
                }
                OpCode::DeleteByVal { dst, object, key } => {
                    check_register(*dst)?;
                    check_register(*object)?;
                    check_register(*key)?;
                }
                OpCode::Return { source }
                | OpCode::Throw { value: source }
                | OpCode::StoreThis { src: source } => {
                    check_register(*source)?;
                }
                OpCode::Jump { offset } => check_branch(pc, *offset)?,
                OpCode::JumpIfTrue { source, offset } | OpCode::JumpIfFalse { source, offset } => {
                    check_register(*source)?;
                    check_branch(pc, *offset)?;
                }
                OpCode::Add { dest, lhs, rhs }
                | OpCode::Sub { dest, lhs, rhs }
                | OpCode::Div { dest, lhs, rhs }
                | OpCode::Mul { dest, lhs, rhs }
                | OpCode::Rem { dest, lhs, rhs }
                | OpCode::Shr { dest, lhs, rhs }
                | OpCode::Shl { dest, lhs, rhs }
                | OpCode::UShr { dest, lhs, rhs }
                | OpCode::Or { dest, lhs, rhs }
                | OpCode::And { dest, lhs, rhs }
                | OpCode::Xor { dest, lhs, rhs }
                | OpCode::Equal { dest, lhs, rhs }
                | OpCode::NotEqual { dest, lhs, rhs }
                | OpCode::StrictEq { dest, lhs, rhs }
                | OpCode::StrictNEq { dest, lhs, rhs }
                | OpCode::Greater { dest, lhs, rhs }
                | OpCode::GreaterOrEqual { dest, lhs, rhs }
                | OpCode::Less { dest, lhs, rhs }
                | OpCode::LessOrEqual { dest, lhs, rhs } => {
                    check_register(*dest)?;
                    check_register(*lhs)?;
                    check_register(*rhs)?;
                }
                OpCode::In { src_dest, rhs } | OpCode::InstanceOf { src_dest, rhs } => {
                    check_register(*src_dest)?;
                    check_register(*rhs)?;
                }
                OpCode::TypeOf { src_dest } => check_register(*src_dest)?,
                OpCode::Not { dest, src }
                | OpCode::LogicalNot { dest, src }
                | OpCode::UnaryPlus { dest, src }
                | OpCode::Negate { dest, src } => {
                    check_register(*dest)?;
                    check_register(*src)?;
                }
                OpCode::EnterTry { catch_offset } => check_branch(pc, *catch_offset)?,
                OpCode::LeaveTry | OpCode::InitialYield => (),
                OpCode::TryGetById {
                    dst,
                    object,
                    prop,
                    fdbk,
                }
                | OpCode::GetById {
                    dst,
                    object,
                    prop,
                    fdbk,
                } => {
                    check_register(*dst)?;
                    check_register(*object)?;
                    check_pool("name", *prop, self.names.len())?;
                    check_pool("feedback", *fdbk, self.feedback.len())?;
                }
                OpCode::PutById {
                    object,
                    value,
                    prop,
                    fdbk,
                } => {
                    check_register(*object)?;
                    check_register(*value)?;
                    check_pool("name", *prop, self.names.len())?;
                    check_pool("feedback", *fdbk, self.feedback.len())?;
                }
                OpCode::PutByVal { object, key, value } => {
                    check_register(*object)?;
                    check_register(*key)?;
                    check_register(*value)?;
                }
                OpCode::GetByVal { dst, object, key } => {
                    check_register(*dst)?;
                    check_register(*object)?;
                    check_register(*key)?;
                }
                OpCode::SetEnvironment { src, .. } => check_register(*src)?,
                OpCode::DeclLet { env, .. }
                | OpCode::DeclConst { env, .. }
                | OpCode::DeclVar { env, .. } => check_register(*env)?,
                OpCode::SetVar { src, env, .. } => {
                    check_register(*src)?;
                    check_register(*env)?;
                }
                OpCode::DeleteGlobal { dst, prop } => {
                    check_register(*dst)?;
                    check_pool("name", *prop, self.names.len())?;
                }
                OpCode::ToObject { dst, src }
                | OpCode::ToLength { dst, src }
                | OpCode::ToIntOrInf { dst, src } => {
                    check_register(*dst)?;
                    check_register(*src)?;
                }
                OpCode::IsCallable { dst, src }
                | OpCode::IsCtor { dst, src }
                | OpCode::IsObject { dst, src } => {
                    check_register(*dst)?;
                    check_register(*src)?;
                }
                OpCode::Yield { src_dest }
                | OpCode::YieldStar { src_dest }
                | OpCode::Await { src_dest } => check_register(*src_dest)?,
                OpCode::ForInInit {
                    iterator,
                    source,
                    done_offset,
                } => {
                    check_register(*iterator)?;
                    check_register(*source)?;
                    check_branch(pc, *done_offset)?;
                }
                OpCode::ForInNext {
                    iterator,
                    dest,
                    done_offset,
                }
                | OpCode::ForOfNext {
                    iterator,
                    dest,
                    done_offset,
                } => {
                    check_register(*iterator)?;
                    check_register(*dest)?;
                    check_branch(pc, *done_offset)?;
                }
                OpCode::ForInClose { iterator } | OpCode::ForOfClose { iterator } => {
                    check_register(*iterator)?;
                }
                OpCode::ForOfInit { iterator, source } => {
                    check_register(*iterator)?;
                    check_register(*source)?;
                }
            }
        }
        Ok(())
    }
}

impl TraceTrait for BytecodeBlockInternal {
    fn trace(&self, vis: &mut comet::visitor::Visitor) {
        self.literals.trace(vis);
        self.parent_blocks.trace(vis);
        self.structures.trace(vis);
        self.feedback.trace(vis);
    }
}

impl FinalizeTrait<BytecodeBlock> for BytecodeBlockInternal {}
impl FinalizeTrait<BytecodeBlock> for BytecodeBlock {}

#[cfg(test)]
mod tests {
    use super::{
        patch_branch_instruction, relative_branch_offset, resolve_branch_target,
        BytecodeValidationError, InstructionIndex,
    };
    use crate::bytecode::opcodes::OpCode;

    #[test]
    fn branch_offsets_are_relative_to_the_following_instruction() {
        assert_eq!(relative_branch_offset(2, 5), Ok(2));
        assert_eq!(relative_branch_offset(4, 1), Ok(-4));
        assert_eq!(relative_branch_offset(5, 6), Ok(0));
    }

    #[test]
    fn branch_target_resolution_accepts_forward_and_backward_edges() {
        assert_eq!(resolve_branch_target(2, 2, 6), Ok(5));
        assert_eq!(resolve_branch_target(4, -4, 6), Ok(1));
    }

    #[test]
    fn branch_target_resolution_rejects_targets_outside_the_instruction_stream() {
        assert_eq!(
            resolve_branch_target(2, 3, 6),
            Err(BytecodeValidationError::BranchTargetOutOfRange {
                pc: 2,
                target: 6,
                len: 6,
            })
        );
        assert!(matches!(
            resolve_branch_target(0, -2, 4),
            Err(BytecodeValidationError::BranchTargetOutOfRange { target: -1, .. })
        ));
    }

    #[test]
    fn branch_offset_must_fit_the_instruction_operand() {
        assert!(matches!(
            relative_branch_offset(0, u32::MAX),
            Err(BytecodeValidationError::BranchOffsetOutOfRange {
                pc: 0,
                target: u32::MAX,
            })
        ));
    }

    #[test]
    fn branch_patching_stores_offset_from_the_next_instruction() {
        let mut instruction = OpCode::Jump { offset: 0 };

        patch_branch_instruction(&mut instruction, 3, InstructionIndex(7)).unwrap();

        assert_eq!(instruction, OpCode::Jump { offset: 3 });
    }

    #[test]
    fn branch_patching_rejects_non_branch_instructions() {
        assert_eq!(
            patch_branch_instruction(&mut OpCode::Nop, 3, InstructionIndex(7)),
            Err(BytecodeValidationError::BranchKindMismatch { pc: 3 })
        );
    }
}
