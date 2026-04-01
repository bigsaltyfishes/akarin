//! Akarin microkernel binary.
//!
//! This crate wires together:
//! - machine-specific bootstrap and trap entry;
//! - the object, process, scheduler, and VM runtimes;
//! - the syscall registry and early userspace bootstrap path.
//!
//! The kernel keeps ISA-specific implementation details under `arch`, while
//! runtime subsystems such as scheduling, IPC, and VM live in dedicated
//! modules that are reused across the rest of the kernel.
#![deny(missing_docs)]
#![deny(closure_returning_async_block)]
#![feature(naked_functions_rustic_abi)]
#![no_std]
#![no_main]

#[macro_use]
extern crate log;

extern crate alloc;

mod arch;
mod clock;
mod debug;
mod device;
mod error;
mod init;
mod interrupt;
mod ipc;
mod memory;
mod runtime;
mod sched;
mod service;
mod syscall;
mod tests;
mod user;

use core::sync::atomic::{AtomicBool, Ordering};

pub use clock::{ClockSourceManager, ClockTickHook};
pub use debug::logger;
use libakarin_boot_proto::{BootInfo, Requirements};
use libakarin_machine_core::{cpu::PerCpuTrait, sync::RawScopedGuard};
use libakarin_macros::cpu_local;
pub use runtime::{
    BootstrapContext, NamespaceBootstrap, NamespaceSet, RuntimeBootstrap, RuntimeServices,
    init_standard_namespaces,
};
pub use sched::{
    PreemptGuard, ProcessId, Scheduler, Sleep, SpawnError, TaskExit, TaskId, Timer, Yield,
    process::{Process, ProcessControl, ProcessManager},
    scheduler,
};
pub use syscall::{UserPtr, UserPtrError, UserSlice};

use crate::{
    arch::{PerCpu, guards::IrqSaveGuard},
    debug::backtrace,
    tests::KernelSelfTests,
};

cpu_local! {
    #[allow(dead_code)]
    static PERCPU_BOOT_MARKER: usize = 0;
}

const KERNEL_LOAD_BASE: usize = 0xffff_ffff_8000_0000;
const BSP_STACK_BASE: usize = 0xffff_ff00_0000_0000;
/// Number of 4 KiB pages reserved for the bootstrap processor stack.
pub const STACK_PAGE_NUM: usize = 4;

#[unsafe(link_section = "__REQ, __requirements")]
#[used]
static REQUIREMENTS: Requirements = Requirements::new(
    KERNEL_LOAD_BASE,
    STACK_PAGE_NUM,
    BSP_STACK_BASE,
    true,
    true,
    true,
);

#[unsafe(no_mangle)]
fn main(info: &'static mut BootInfo) {
    init::initialize(info);
    match Scheduler::spawn_on(0, Scheduler::kernel_process(), kmain()) {
        Ok(task_id) => {
            info!("[kernel] kmain spawned as task {}", task_id);
            Scheduler::request_resched();
        }
        Err(err) => panic!("failed to spawn kmain task: {:?}", err),
    }
    Scheduler::enter_current_cpu()
}

async fn kmain() -> TaskExit {
    info!("[kernel] kmain self-tests start");
    KernelSelfTests::run().await;
    info!("[kernel] kmain self-tests done");
    info!("[kernel/bootstrap] spawning first user task");
    match user::spawn_bootstrap_program() {
        Ok(task_id) => info!("[kernel/bootstrap] spawned first user task {}", task_id),
        Err(err) => warn!(
            "[kernel/bootstrap] failed to spawn first user task: {:?}",
            err
        ),
    }
    TaskExit::Completed
}

static PANIC_MODE: AtomicBool = AtomicBool::new(false);

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let _irq_guard = IrqSaveGuard::enter();
    logger::enter_panic_mode();

    while true == PANIC_MODE.swap(true, Ordering::SeqCst) {
        core::hint::spin_loop();
    }
    error!("Kernel panic as core {}: {}", PerCpu::id(), info);
    backtrace();
    PANIC_MODE.store(false, Ordering::Release);
    loop {
        PerCpu::halt();
    }
}
