#![no_std]
#![feature(associated_type_defaults)]

extern crate alloc;

use crate::{
    init::MachineInitTrait,
    memory::AddressSpaceTrait,
    scheduler::SchedulerHelper,
    sync::{
        DroppableScopedGuard, IrqGuard, IrqSaveGuard, NoOp, NoOpGuard, NoPreemptGuard, ScopedGuard,
    },
};

pub mod backtrace;
pub mod context;
pub mod cpu;
pub mod exception;
pub mod init;
pub mod interrupt;
pub mod io;
pub mod memory;
pub mod scheduler;
pub mod sync;

/// ISA contract exposed to the rest of the kernel.
///
/// `Machine` is intentionally a type-only surface: it exports machine-owned
/// data structures and guard classes, while runtime state must be installed via
/// dedicated subsystems such as `cpu::install_percpu_runtime`.
pub trait MachineTrait: MachineInitTrait + AddressSpaceTrait + SchedulerHelper {
    /// Architecture-specific SIMD/FPU save area exported to the kernel through
    /// the machine contract.
    type SimdContext: context::SimdContextTrait;
    /// Architecture-specific trap frame representation.
    type TrapContext: context::TrapContextTrait<SimdContext = Self::SimdContext>;
    type StackFrame: backtrace::StackFrameTrait<Self::TrapContext>;
    type NoOpGuard: NoOpGuard + DroppableScopedGuard = ScopedGuard<NoOp>;
    type IrqGuard: IrqGuard + DroppableScopedGuard;
    type IrqSaveGuard: IrqSaveGuard + DroppableScopedGuard;
    type NoPreemptGuard: NoPreemptGuard + DroppableScopedGuard;
    type PerCpu: cpu::PerCpuTrait;

    type InterruptController: interrupt::InterruptControllerTrait;
    type PortIo: io::PortIoTrait;
}
