//! x86_64 trap entry dispatch.
//!
//! `trap.S` saves the machine frame into one [`TrapContext`] and then calls
//! [`trap_handler`]. The handler keeps the split between:
//! - hard interrupt delivery;
//! - kernel fault fixup and panic paths;
//! - user-visible synchronous traps that must return through the task runtime.

use libakarin_core::memory::VmLayoutSegment;
use libakarin_machine_core::{
    context::{TrapContextTrait, TrapReason},
    cpu::PerCpuTrait,
    exception::ExceptionTable,
    interrupt::InterruptControllerTrait,
    memory::VirtAddr,
};

use crate::{
    RuntimeServices,
    arch::{TrapContext, x86_64::percpu::PerCpu},
    scheduler,
};

/// Dispatch one architecture trap after the assembly entry stubs have built
/// one stable [`TrapContext`].
///
/// The handler only performs machine-level triage:
/// - hardware interrupts are forwarded to the interrupt controller and may
///   trigger one trap-boundary reschedule before returning;
/// - kernel page faults first consult the exception table fixup path used by
///   uaccess and other recoverable probe sites;
/// - all other kernel faults panic immediately because they represent broken
///   invariants above the trap layer.
#[unsafe(no_mangle)]
extern "C" fn trap_handler(ctx: &mut TrapContext) {
    let cpuid = PerCpu::id();
    let reason = ctx.reason();
    let ic = RuntimeServices::global().interrupt_controller();
    match reason {
        TrapReason::Interrupt(vector) => {
            // IRQ delivery is completed before any optional scheduler
            // hand-off so the physical controller sees end-of-interrupt in one
            // bounded region.
            if let Err(e) = ic.handle_irq(vector) {
                log::warn!("[x86_64/idt] unhandled irq vector={} err={:?}", vector, e);
            }
            if ctx.is_kernel_mode() {
                // Kernel traps are the only place where one preempted kernel
                // task may hand control directly to another kernel task
                // without first returning through userspace state.
                if let Some(mut next) = scheduler::Scheduler::schedule_current_cpu(ctx)
                    .ok()
                    .flatten()
                {
                    unsafe {
                        next.run(true);
                    }
                    unreachable!("kernel trap scheduler resumed unexpectedly");
                }
            }
        }
        TrapReason::SoftwareBreakpoint | TrapReason::Syscall => {}
        TrapReason::PageFault(addr, flags) if ctx.is_kernel_mode() => {
            // Recoverable kernel probes, especially uaccess fixups, rely on
            // the exception table before the trap path escalates to panic.
            if let Some(entry) =
                ExceptionTable::new().find_fixup(VirtAddr::new(ctx.instruction_pointer()))
            {
                ctx.set_instruction_pointer(entry.as_usize());
                return;
            }

            let region = VmLayoutSegment::classify(addr);
            panic!(
                "Kernel page fault at core {}: addr={:#x} flags={:?} region={:?}\nContext: {:#x?}",
                cpuid, addr, flags, region, ctx
            );
        }
        _ => {
            if ctx.is_kernel_mode() {
                panic!(
                    "Kernel fault trap at core {}: {:#x?}\nContext: {:#x?}",
                    cpuid, reason, ctx
                );
            }
        }
    }

    if ctx.is_kernel_mode() {
        if let Some(task) = scheduler::Scheduler::current_task_ref() {
            task.save_trap_simd(ctx);
        }
    }

    unsafe { ctx.run(true) };
}
