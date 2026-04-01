use core::arch::asm;

use libakarin_machine_core::{self, backtrace};

use crate::arch::TrapContext;

pub struct StackFrame {
    curr: *const usize,
}

impl backtrace::StackFrameTrait<TrapContext> for StackFrame {
    fn from_ctx(ctx: &TrapContext) -> Self {
        Self {
            curr: ctx.rbp as *const usize,
        }
    }

    fn next(&mut self) -> Option<*const usize> {
        if self.curr.is_null() {
            return None;
        }
        let ra = unsafe { *self.curr.offset(1) };
        self.curr = unsafe { *self.curr as *const usize };
        if ra != 0 {
            Some((ra - 1) as *const usize)
        } else {
            None
        }
    }
}

impl Default for StackFrame {
    fn default() -> Self {
        let rbp: *const usize;
        unsafe {
            asm!("mov {}, rbp", out(reg) rbp);
        }
        Self { curr: rbp }
    }
}
