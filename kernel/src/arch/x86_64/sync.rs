use core::arch::asm;

use libakarin_machine_core::sync::{RawScopedGuard, class};

pub struct IrqGuard;

pub fn enter_irq_save_token() -> usize {
    let rflags: usize;
    unsafe {
        asm!("pushfq; pop {}", out(reg) rflags);
    }
    if rflags & (1 << 9) != 0 {
        unsafe {
            asm!("cli");
        }
    }
    rflags
}

pub fn exit_irq_save_token(saved_rflags: usize) {
    if saved_rflags & (1 << 9) != 0 {
        unsafe {
            asm!("sti");
        }
    }
}

impl RawScopedGuard for IrqGuard {
    type Class = class::IrqGuard;

    #[doc = " Enter the critical section or acquire the resource."]
    fn enter() -> Self {
        unsafe {
            asm!("cli");
        }
        Self
    }

    #[doc = " Exit the critical section or release the resource."]
    fn exit(self) {
        unsafe {
            asm!("sti");
        }
    }
}

pub struct IrqSaveGuard(Option<IrqGuard>);

impl RawScopedGuard for IrqSaveGuard {
    type Class = class::IrqSaveGuard;

    #[doc = " Enter the critical section or acquire the resource, and save the current interrupt \
             state."]
    fn enter() -> Self {
        let saved_rflags = enter_irq_save_token();
        if saved_rflags & (1 << 9) != 0 {
            Self(Some(IrqGuard))
        } else {
            Self(None)
        }
    }

    #[doc = " Exit the critical section or release the resource, and restore the previous \
             interrupt state."]
    fn exit(self) {
        if self.0.is_some() {
            exit_irq_save_token(1 << 9);
        }
    }
}

pub struct NoPreemptGuard(IrqSaveGuard);

impl RawScopedGuard for NoPreemptGuard {
    type Class = class::NoPreemptGuard;

    #[doc = " Enter the critical section or acquire the resource, and save the current interrupt \
             state."]
    fn enter() -> Self {
        Self(IrqSaveGuard::enter())
    }

    #[doc = " Exit the critical section or release the resource, and restore the previous \
             interrupt state."]
    fn exit(self) {
        IrqSaveGuard::exit(self.0);
    }
}
