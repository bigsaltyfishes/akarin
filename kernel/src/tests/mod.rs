use core::arch::asm;

use libakarin_machine_core::exception::ExceptionTable;

use crate::debug::backtrace;

mod pci;
mod sched;

/// Kernel-internal self-test entry point.
pub struct KernelSelfTests;

impl KernelSelfTests {
    /// Unified switch for all runtime self-tests.
    pub const ENABLED: bool = true;

    /// Run all enabled kernel self-tests.
    pub async fn run() {
        if !Self::ENABLED {
            return;
        }

        Self::run_breakpoint_self_test();
        sched::SchedulerSelfTests::run().await;
        info!("[kernel/tests] pci self-tests start");
        pci::PciSelfTests::run().await;
        info!("[kernel/tests] pci self-tests done");
        info!("[kernel/tests] debug self-test start");
        Self::run_debug_self_test();
        info!("[kernel/tests] debug self-test done");
    }

    fn run_breakpoint_self_test() {
        unsafe {
            asm!("int3");
        }
        log::info!("Returned from int3.");
    }

    fn run_debug_self_test() {
        info!("[kernel] exception tables: {:#x?}", ExceptionTable::new());
        backtrace();
    }
}
