use alloc::boxed::Box;
use core::arch::{asm, naked_asm};

use libakarin_machine_core::{
    cpu::PerCpuTrait,
    init,
    interrupt::InterruptControllerTrait,
    scheduler::SchedulerHelper,
    sync::{NoOp, ScopedGuard},
};
use raw_cpuid::CpuId;
use x86_64::registers::{
    control::{Cr0, Cr0Flags, Cr4, Cr4Flags, Efer, EferFlags},
    xcontrol::{XCr0, XCr0Flags},
};

use crate::{
    RuntimeServices,
    arch::x86_64::{interrupt::apic::Apic, percpu::PerCpu, smp::Smp},
    device::pci::{PciInterruptBinder, PublishedPciHostBridge},
    sched::stack::CpuFallbackStack,
};

mod backtrace;
mod clock;
mod context;
mod gdt;
mod idt;
mod interrupt;
mod io;
mod memory;
mod percpu;
mod smp;
mod sync;
mod trap;

pub struct Machine;

impl init::MachineInitTrait for Machine {
    fn early_init() {
        io::init_early_serial();
    }

    fn early_writer() -> Option<&'static mut (dyn core::fmt::Write + Send)> {
        Some(io::early_serial_writer())
    }

    fn possible_cpu_num() -> usize {
        let boot = RuntimeServices::boot_info();
        let count = crate::device::acpi::early_cpu_topology(boot.rsdp, boot.physical_memory_offset)
            .map(|topology| topology.cpu_count())
            .unwrap_or(1)
            .max(1);
        log::info!("[x86_64] early possible_cpu_num={}", count);
        count
    }

    fn bsp_init() {
        log::info!("[x86_64] BSP init start");
        enable_cpu_features();
        PerCpu::install_runtime();
        // Phase 2.1: CPU features + per-cpu + GDT/IDT.
        log::info!("[x86_64] percpu init start");
        unsafe {
            PerCpu::init();
        }
        PerCpu::set_current_cpu_base();
        let _ = PerCpu::irq_stack_top();
        let _ = PerCpu::irq_nesting();
        log::info!("[x86_64] percpu init done");

        gdt::load();
        idt::init();
        CpuFallbackStack::install(0).expect("Failed to install BSP fallback stack");
        log::info!("[x86_64] BSP init done");
    }

    fn device_init() {
        log::info!("[x86_64] device init start");
        // Phase 2.3: APIC controller setup.
        let controller = Apic::init_lapic_bsp();
        let namespaces = RuntimeServices::global().namespaces();
        let device_owner = crate::interrupt::InterruptController::new(controller)
            .publish(
                namespaces.device_manager(),
                namespaces.irq_resource_manager(),
            )
            .expect("failed to publish APIC controller");
        let _ = RuntimeServices::global().install_interrupt_controller(controller);
        unsafe { device_owner.forget() };
        match crate::device::pci::PciEcamConfig::discover_from_firmware() {
            Ok(config) => {
                let summary = config.interrupt_summary();
                let config = Box::leak(Box::new(config));
                let binder = Box::leak(Box::new(PciInterruptBinder::new(controller, config)));
                let _ = RuntimeServices::global().install_pci_runtime(config, binder);
                match PublishedPciHostBridge::new(config).publish(
                    RuntimeServices::global().namespaces().device_manager(),
                    RuntimeServices::global().namespaces().resource_manager(),
                ) {
                    Ok(owner) => unsafe { owner.forget() },
                    Err(err) => {
                        log::warn!(
                            "[device/pci] failed to publish PCI host/function objects: {err:?}"
                        );
                    }
                }
                log::info!(
                    "[device/pci] ECAM ready: regions={} present_functions={} msi={} msix={}",
                    config.region_count(),
                    summary.present_functions,
                    summary.msi_functions,
                    summary.msix_functions
                );
            }
            Err(err) => {
                log::info!("[device/pci] ECAM unavailable: {:?}", err);
            }
        }
        clock::init_clock_sources();
        let _ = RuntimeServices::global().interrupt_controller().enable_ic();
        if let Err(err) = controller.selftest_local_ipi() {
            log::warn!("[x86_64] APIC local self-test failed: {:?}", err);
        }
        // TODO: Probe/register port-io resources.
        // TODO: Register ACPI resource objects into kernel namespace.
        log::info!("[x86_64] device init done");
    }

    fn smp_init() {
        log::info!("[x86_64/smp] bringup start");
        smp::smp_bringup();
        log::info!("[x86_64/smp] bringup done");
    }

    fn finalize_init() {
        Smp::signal_enter_kmain();
    }

    fn ap_init() {
        enable_cpu_features();
        PerCpu::install_runtime();
        PerCpu::set_current_cpu_base();
        let _ = PerCpu::irq_stack_top();
        let _ = PerCpu::irq_nesting();
        let cpu_id = PerCpu::id();
        let apic_id = PerCpu::lapic_id_of(cpu_id).unwrap_or(usize::MAX);
        gdt::load();
        idt::init();
        Apic::init_lapic_ap();
        if let Err(err) = RuntimeServices::global()
            .namespaces()
            .clock_source_manager()
            .setup_timer()
        {
            log::warn!(
                "[x86_64/smp cpu={} apic={}] timer ISR bind failed: {:?}",
                cpu_id,
                apic_id,
                err
            );
        }
        let _ = RuntimeServices::global().interrupt_controller().enable_ic();
        log::info!("[x86_64/smp cpu={} apic={}] online", cpu_id, apic_id);
    }
}

impl libakarin_machine_core::MachineTrait for Machine {
    type SimdContext = context::SimdContext;

    type TrapContext = context::TrapContext;

    type StackFrame = backtrace::StackFrame;

    type IrqGuard = ScopedGuard<sync::IrqGuard>;

    type IrqSaveGuard = ScopedGuard<sync::IrqSaveGuard>;

    type NoPreemptGuard = ScopedGuard<sync::NoPreemptGuard>;

    type PerCpu = PerCpu;

    type NoOpGuard = ScopedGuard<NoOp>;

    type InterruptController = Apic;

    type PortIo = io::X86PortIo;
}

impl SchedulerHelper for Machine {
    #[unsafe(naked)]
    unsafe extern "C" fn task_runner_trampoline() -> ! {
        naked_asm!(
            "push rbp",
            "mov rbp, rsp",
            "call {inner}",
            "ud2",
            inner = sym crate::sched::dispatch::scheduler_task_runner_inner,
        )
    }

    unsafe fn enter_idle_shell(stack_top: usize, entry: extern "C" fn() -> !) -> ! {
        let rsp = stack_top
            .checked_sub(core::mem::size_of::<usize>())
            .expect("fallback stack top must accommodate a sentinel return address");
        unsafe {
            (rsp as *mut usize).write(0);
            asm!(
                "mov rsp, {stack}",
                "xor rbp, rbp",
                "jmp {entry}",
                stack = in(reg) rsp,
                entry = in(reg) entry,
                options(noreturn)
            );
        }
    }

    unsafe fn invoke_local_reschedule() {
        unsafe {
            asm!(
                "int {vector}",
                vector = const interrupt::apic::consts::APIC_IPI_RESCHEDULE,
                options(nomem, nostack)
            );
        }
    }
}

fn enable_cpu_features() {
    let cpuid = CpuId::new();
    let has_fsgsbase = cpuid
        .get_extended_feature_info()
        .is_some_and(|f| f.has_fsgsbase());
    let has_syscall = cpuid
        .get_extended_processor_and_feature_identifiers()
        .is_some_and(|f| f.has_syscall_sysret());
    let Some(feature_info) = cpuid.get_feature_info() else {
        panic!("[x86_64] CPU feature leaf is unavailable, cannot continue");
    };
    let Some(extended_state) = cpuid.get_extended_state_info() else {
        panic!("[x86_64] CPU extended state leaf is unavailable, cannot continue");
    };

    if !has_syscall {
        panic!("[x86_64] CPU does not support SYSCALL/SYSRET, cannot continue");
    }

    if !has_fsgsbase {
        panic!("[x86_64] CPU does not support FSGSBASE, cannot continue");
    }

    if !feature_info.has_sse()
        || !feature_info.has_sse2()
        || !feature_info.has_xsave()
        || !feature_info.has_avx()
    {
        panic!("[x86_64] CPU does not support SSE/SSE2/XSAVE/AVX, cannot continue");
    }

    if !extended_state.has_xsaveopt()
        || !extended_state.xcr0_supports_legacy_x87()
        || !extended_state.xcr0_supports_sse_128()
        || !extended_state.xcr0_supports_avx_256()
    {
        panic!("[x86_64] CPU does not support XSAVEOPT-backed x87/SSE/AVX state, cannot continue");
    }

    let xsave_mask = (XCr0Flags::X87 | XCr0Flags::SSE | XCr0Flags::AVX).bits();
    let xsave_area_size = extended_state.xsave_area_size_enabled_features().max(512) as usize;

    unsafe {
        Cr4::update(|cr4| {
            cr4.insert(Cr4Flags::FSGSBASE);
            cr4.insert(Cr4Flags::OSFXSR);
            cr4.insert(Cr4Flags::OSXMMEXCPT_ENABLE);
            cr4.insert(Cr4Flags::OSXSAVE);
        });
        Efer::update(|f| {
            f.insert(EferFlags::NO_EXECUTE_ENABLE);
            f.insert(EferFlags::SYSTEM_CALL_EXTENSIONS);
        });
        Cr0::update(|f| {
            f.remove(Cr0Flags::WRITE_PROTECT);
            f.remove(Cr0Flags::EMULATE_COPROCESSOR);
            f.remove(Cr0Flags::TASK_SWITCHED);
            f.insert(Cr0Flags::MONITOR_COPROCESSOR);
            f.insert(Cr0Flags::NUMERIC_ERROR);
        });
        XCr0::write(XCr0Flags::X87 | XCr0Flags::SSE | XCr0Flags::AVX);
        asm!("fninit", options(nostack, preserves_flags));
    }
    context::install_simd_runtime(xsave_area_size, xsave_mask);
    log::debug!("[x86_64/cpu] CPU features enabled: FSGSBASE,NX");
}
