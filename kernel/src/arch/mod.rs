#![allow(dead_code)]

#[cfg(target_arch = "x86_64")]
mod x86_64;

#[cfg(target_arch = "x86_64")]
mod __private {
    use libakarin_machine_core::MachineTrait;

    pub use super::x86_64::Machine;

    pub type PerCpu = <Machine as MachineTrait>::PerCpu;
    /// Architecture-exported SIMD/FPU save-area type.
    pub type SimdContext = <Machine as MachineTrait>::SimdContext;
    pub type TrapContext = <Machine as MachineTrait>::TrapContext;
    pub type StackFrame = <Machine as MachineTrait>::StackFrame;

    pub mod interrupt {
        use libakarin_machine_core::{MachineTrait, interrupt::InterruptControllerTrait};

        use super::Machine;

        pub type InterruptController = <Machine as MachineTrait>::InterruptController;
        pub type IrqLine = <InterruptController as InterruptControllerTrait>::Line;
        pub type IrqSession = <InterruptController as InterruptControllerTrait>::Session;
        pub type IrqMessageBlock = <InterruptController as InterruptControllerTrait>::MessageBlock;
    }
    pub mod guards {
        use libakarin_machine_core::MachineTrait;

        use super::Machine;

        pub type IrqGuard = <Machine as MachineTrait>::IrqGuard;
        pub type IrqSaveGuard = <Machine as MachineTrait>::IrqSaveGuard;
        pub type NoPreemptGuard = <Machine as MachineTrait>::NoPreemptGuard;
        pub type NoOpGuard = <Machine as MachineTrait>::NoOpGuard;
    }

    pub mod vm {
        use libakarin_machine_core::memory::{AddressSpaceTrait, PageSizeTrait};

        pub use super::Machine;

        pub type Page = <Machine as AddressSpaceTrait>::Page;
        pub type PageSize = <Machine as AddressSpaceTrait>::PageSize;
        pub type PageTable = <Machine as AddressSpaceTrait>::PageTable;
        pub type PageTableEntry = <Machine as AddressSpaceTrait>::PageTableEntry;
        pub type PhysFrame = <Machine as AddressSpaceTrait>::PhysFrame;

        pub const UNIT_PAGE_SIZE: usize = <PageSize as PageSizeTrait>::UNIT_PAGE_SIZE;
    }
}

pub use __private::*;
