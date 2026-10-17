/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
use super::*;
use crate::gc::cell::Trace;
use crate::vm::interpreter::register_frame::RegisterFrame;

use std::{intrinsics::unlikely, ptr::null_mut};
#[allow(dead_code)]
pub struct Stack {
    mem: *mut u8,
    start: *mut JsValue,
    pub(crate) cursor: *mut JsValue,
    end: *mut JsValue,
    pub(crate) current: *mut CallFrame,
    register_frames: RegisterFrameStack,
}

pub const STACK_SIZE: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegisterFrameStackError {
    RegisterLimitExceeded {
        requested: usize,
        available: usize,
    },
    FrameLimitExceeded,
    AllocationFailed,
    InvalidReturnRegister {
        register: u16,
        caller_register_count: u32,
    },
}

fn validate_register_stack_limits(
    live_registers: usize,
    frame_count: usize,
    requested_registers: usize,
) -> Result<(), RegisterFrameStackError> {
    let Some(total_registers) = live_registers.checked_add(requested_registers) else {
        return Err(RegisterFrameStackError::RegisterLimitExceeded {
            requested: requested_registers,
            available: STACK_SIZE.saturating_sub(live_registers),
        });
    };
    if total_registers > STACK_SIZE {
        return Err(RegisterFrameStackError::RegisterLimitExceeded {
            requested: requested_registers,
            available: STACK_SIZE.saturating_sub(live_registers),
        });
    }
    if frame_count >= STACK_SIZE {
        return Err(RegisterFrameStackError::FrameLimitExceeded);
    }
    Ok(())
}

struct RegisterFrameStack {
    frames: Vec<Box<RegisterFrame>>,
    current: *mut RegisterFrame,
    register_count: usize,
}

impl RegisterFrameStack {
    fn new() -> Self {
        Self {
            frames: Vec::new(),
            current: null_mut(),
            register_count: 0,
        }
    }

    fn push(
        &mut self,
        mut frame: RegisterFrame,
    ) -> Result<*mut RegisterFrame, RegisterFrameStackError> {
        validate_register_stack_limits(
            self.register_count,
            self.frames.len(),
            frame.register_count() as usize,
        )?;
        let new_register_count = self.register_count + frame.register_count() as usize;
        if !self.current.is_null() {
            if let Some(register) = frame.return_register {
                let caller_register_count = unsafe { (*self.current).register_count() };
                if u32::from(register) >= caller_register_count {
                    return Err(RegisterFrameStackError::InvalidReturnRegister {
                        register,
                        caller_register_count,
                    });
                }
            }
        } else if let Some(register) = frame.return_register {
            return Err(RegisterFrameStackError::InvalidReturnRegister {
                register,
                caller_register_count: 0,
            });
        }
        self.frames
            .try_reserve(1)
            .map_err(|_| RegisterFrameStackError::AllocationFailed)?;
        frame.caller = self.current;
        let mut frame = Box::new(frame);
        let frame_ptr = &mut *frame as *mut RegisterFrame;
        self.frames.push(frame);
        self.current = frame_ptr;
        self.register_count = new_register_count;
        Ok(frame_ptr)
    }

    fn pop(&mut self) -> Option<Box<RegisterFrame>> {
        let frame = self.frames.pop()?;
        self.current = frame.caller;
        self.register_count -= frame.register_count() as usize;
        Some(frame)
    }
}

#[cfg(test)]
mod register_frame_stack_tests {
    use super::{validate_register_stack_limits, RegisterFrameStackError, STACK_SIZE};

    #[test]
    fn register_stack_accepts_exact_capacity() {
        assert_eq!(validate_register_stack_limits(STACK_SIZE - 4, 2, 4), Ok(()));
    }

    #[test]
    fn register_stack_rejects_capacity_overflow_with_available_slots() {
        assert_eq!(
            validate_register_stack_limits(STACK_SIZE - 4, 2, 5),
            Err(RegisterFrameStackError::RegisterLimitExceeded {
                requested: 5,
                available: 4,
            })
        );
    }

    #[test]
    fn register_stack_bounds_frame_depth() {
        assert_eq!(
            validate_register_stack_limits(0, STACK_SIZE, 0),
            Err(RegisterFrameStackError::FrameLimitExceeded)
        );
    }

    #[test]
    fn register_stack_rejects_count_arithmetic_overflow() {
        assert_eq!(
            validate_register_stack_limits(usize::MAX, 0, 1),
            Err(RegisterFrameStackError::RegisterLimitExceeded {
                requested: 1,
                available: 0,
            })
        );
    }
}

impl Trace for RegisterFrameStack {
    fn trace(&self, visitor: &mut Visitor) {
        for frame in &self.frames {
            frame.trace(visitor);
        }
    }
}

impl Stack {
    pub fn new() -> Self {
        let mut map = unsafe { libc::calloc(1, STACK_SIZE * 8).cast::<u8>() };
        unsafe {
            let mut scan = map.cast::<JsValue>();
            let end = scan.add(STACK_SIZE);
            while scan < end {
                scan.write(JsValue::encode_undefined_value());
                scan = scan.add(1);
            }
        }
        Self {
            start: map.cast(),
            end: unsafe { map.cast::<JsValue>().add(STACK_SIZE) },
            cursor: map.cast(),
            current: null_mut(),
            mem: map.cast(),
            register_frames: RegisterFrameStack::new(),
        }
    }

    /// Pushes a register frame while enforcing the VM's aggregate register-stack
    /// limit. The returned pointer remains stable until this frame is popped.
    pub fn push_register_frame(
        &mut self,
        frame: RegisterFrame,
    ) -> Result<*mut RegisterFrame, RegisterFrameStackError> {
        self.register_frames.push(frame)
    }

    pub fn pop_register_frame(&mut self) -> Option<Box<RegisterFrame>> {
        self.register_frames.pop()
    }

    pub fn current_register_frame(&mut self) -> Option<&mut RegisterFrame> {
        if self.register_frames.current.is_null() {
            None
        } else {
            unsafe { Some(&mut *self.register_frames.current) }
        }
    }

    pub fn register_stack_usage(&self) -> (usize, usize) {
        (
            self.register_frames.register_count,
            self.register_frames.frames.len(),
        )
    }
    pub fn new_frame(
        &mut self,
        iloc_count: u32,
        _callee: JsValue,
        env: GcPointer<Environment>,
    ) -> Option<*mut CallFrame> {
        unsafe {
            if self.cursor.add(iloc_count as _) >= self.end {
                return None;
            }

            let frame = Box::into_raw(Box::new(CallFrame {
                exit_on_return: true,
                ctor: false,
                prev: self.current,
                try_stack: vec![],
                env,
                this: JsValue::encode_empty_value(),
                sp: self.cursor,
                limit: self.cursor,
                code_block: None,

                callee: JsValue::encode_undefined_value(),
                ip: null_mut(),
            }));
            self.current = frame;

            Some(frame)
        }
    }

    pub fn pop_frame(&mut self) -> Option<Box<CallFrame>> {
        if self.current.is_null() {
            return None;
        }

        unsafe {
            let frame = Box::from_raw(self.current);
            self.current = frame.prev;
            self.cursor = if frame.prev.is_null() {
                self.start
            } else {
                (*frame.prev).sp
            };

            Some(frame)
        }
    }
    #[inline]
    pub fn push(&mut self, val: JsValue) {
        if unlikely(self.cursor == self.end) {
            panic!("stack overflow");
        }
        unsafe {
            self.cursor.write(val);
            self.cursor = self.cursor.add(1);
        }
    }

    #[inline]
    pub fn pop(&mut self) -> JsValue {
        if unlikely(self.cursor == self.start) {
            //    panic!("Stack underflow");
        }

        unsafe {
            self.cursor = self.cursor.sub(1);
            self.cursor.read()
        }
    }
}

impl Trace for Stack {
    fn trace(&self, visitor: &mut Visitor) {
        self.register_frames.trace(visitor);
        if !self.current.is_null() {
            unsafe {
                let end = (*self.current).sp;
                let mut scan = self.start;
                while scan < end {
                    (&mut *scan).trace(visitor);
                    scan = scan.add(1);
                }
                //visitor.add_conservative(scan as _, end as _);

                let mut frame = self.current;
                while !frame.is_null() {
                    (*frame).trace(visitor);
                    frame = (*frame).prev;
                }
            }
        }
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        unsafe {
            libc::free(self.mem.cast());
        }
    }
}
