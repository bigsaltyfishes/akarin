//! Interrupt Descriptor Table setup for x86_64.
//!
//! The IDT is populated from the assembly vector table generated in
//! `trap.S`. This file only programs descriptor attributes such as user
//! privilege level and IST selection.

use alloc::boxed::Box;
use core::arch::asm;

use x86::irq::{
    BREAKPOINT_VECTOR, DEBUG_VECTOR, DOUBLE_FAULT_VECTOR, MACHINE_CHECK_VECTOR,
    NONMASKABLE_INTERRUPT_VECTOR, OVERFLOW_VECTOR,
};
use x86_64::{
    PrivilegeLevel, VirtAddr,
    structures::{DescriptorTablePointer, idt::*},
};

use crate::arch::x86_64::gdt::{
    DEBUG_IST_INDEX, DOUBLE_FAULT_IST_INDEX, MACHINE_CHECK_IST_INDEX, NMI_IST_INDEX,
};

/// Build and load the single kernel IDT instance.
///
/// Most vectors use the default task stack. Only a narrow set of handlers use
/// IST:
/// - debug and breakpoint share a dedicated debug stack so nested debug
///   activity does not consume arbitrary task stack depth;
/// - NMI, double fault, and machine check use dedicated emergency stacks;
/// - page fault intentionally does not use IST because the kernel expects the
///   handler to observe the interrupted kernel stack directly and recover
///   through exception-table fixups when possible.
pub fn init() {
    unsafe extern "C" {
        #[link_name = "__vectors"]
        static VECTORS: [extern "C" fn(); 256];
    }

    let idt = Box::leak(Box::new(InterruptDescriptorTable::new()));
    let entries: &'static mut [Entry<HandlerFunc>; 256] =
        unsafe { core::mem::transmute_copy(&idt) };
    for i in 0..256 {
        let opt = entries[i].set_handler_fn(unsafe { core::mem::transmute(VECTORS[i]) });
        // Enable user space `int3` and `into`, and install the small set of
        // IST-backed emergency stacks. Ordinary IRQs and `#PF` return on the
        // interrupted task stack so fault fixup keeps direct access to the
        // original call chain.
        unsafe {
            match i as u8 {
                DEBUG_VECTOR => opt.set_stack_index(DEBUG_IST_INDEX),
                NONMASKABLE_INTERRUPT_VECTOR => opt.set_stack_index(NMI_IST_INDEX),
                BREAKPOINT_VECTOR => {
                    opt.set_privilege_level(PrivilegeLevel::Ring3);
                    opt.set_stack_index(DEBUG_IST_INDEX)
                }
                OVERFLOW_VECTOR => opt.set_privilege_level(PrivilegeLevel::Ring3),
                DOUBLE_FAULT_VECTOR => opt.set_stack_index(DOUBLE_FAULT_IST_INDEX),
                MACHINE_CHECK_VECTOR => opt.set_stack_index(MACHINE_CHECK_IST_INDEX),
                // SYSCALL_IRQ => opt.set_privilege_level(PrivilegeLevel::Ring3),
                _ => continue,
            };
        }
    }

    idt.load();
}

/// Read the current IDTR contents for diagnostics.
#[allow(dead_code)]
#[inline]
fn sidt() -> DescriptorTablePointer {
    let mut dtp = DescriptorTablePointer {
        limit: 0,
        base: VirtAddr::zero(),
    };
    unsafe {
        asm!("sidt [{}]", in(reg) &mut dtp);
    }
    dtp
}
