use libakarin_boot_proto::BootInfo;
use libakarin_machine_core::{cpu::PerCpuTrait, init::MachineInitTrait};

use crate::{
    RuntimeServices, Scheduler, TaskExit,
    arch::{Machine, PerCpu},
    debug, logger, memory, syscall,
};

pub fn initialize(info: &'static mut BootInfo) {
    Machine::early_init();
    logger::install(log::LevelFilter::Debug).expect("failed to install kernel logger");
    if let Some(writer) = Machine::early_writer() {
        let _ = logger::register_output(writer);
    }
    RuntimeServices::install_boot_info(info as *mut _);
    let _ = debug::symtab::Symtab::global();
    info!("BootInfo initialized.");
    memory::init_boot_heap(RuntimeServices::boot_info());
    let possible_cpu_num = Machine::possible_cpu_num();

    crate::RuntimeBootstrap::initialize(
        info,
        possible_cpu_num,
        &memory::MEMORY_SUBSYSTEM,
        |context| {
            let namespaces = context.namespaces();
            let device_manager = namespaces.device_manager();
            match device_manager.probe_all(
                namespaces.bootloader_manager(),
                namespaces.resource_manager(),
            ) {
                Ok(()) => {
                    info!("[kernel/device] probe completed");
                    match device_manager.lookup_handle("EFIFramebuffer") {
                        Ok(handle) => {
                            if let Err(err) = crate::logger::attach_efi_framebuffer_output(handle) {
                                warn!(
                                    "[kernel/device] failed to attach EFI framebuffer logger \
                                     output: {err:?}"
                                );
                            } else {
                                info!("[kernel/device] EFI framebuffer logger output attached");
                            }
                        }
                        Err(err) => warn!(
                            "[kernel/device] EFI framebuffer device lookup failed after probe: \
                             {err:?}"
                        ),
                    }
                }
                Err(err) => warn!("[kernel/device] probe failed: {err:?}"),
            }
        },
    )
    .expect("failed to bootstrap runtime");
    syscall::init().expect("failed to initialize syscall table");

    let cpu_count = PerCpu::count();
    memory::init_frame_allocator(RuntimeServices::boot_info(), cpu_count);
    unsafe { memory::enter_next_phase() };

    Machine::bsp_init();
    unsafe { memory::enter_next_phase() };
    Machine::device_init();
    RuntimeServices::global()
        .init_intercpu(PerCpu::count())
        .expect("failed to initialize inter-CPU mailbox runtime");

    Machine::smp_init();

    Scheduler::install(PerCpu::count());

    Machine::finalize_init();

    match Scheduler::spawn_on(0, Scheduler::kernel_process(), async {
        log::info!("[kernel/scheduler] bootstrap task executed");
        TaskExit::Completed
    }) {
        Ok(task_id) => {
            Scheduler::request_resched();
            log::info!(
                "[kernel/scheduler] bootstrap task submitted as task {}",
                task_id
            );
        }
        Err(err) => warn!("[kernel/scheduler] bootstrap task spawn failed: {:?}", err),
    }

    log::info!("Akarin OS kernel initialized successfully!");
}
