use libakarin_machine_core::{
    memory::FrameAllocatorTrait,
    sync::{NoOp, ScopedGuard},
};
use libakarin_object::Registry;
use libakarin_sync::spin::{Once, TryInitError};

use super::{futex::FutexManager, interrupt::InterruptRuntime, pci::PciRuntime};
use crate::{
    NamespaceSet,
    arch::interrupt::InterruptController,
    device::pci::{PciEcamConfig, PciInterruptBinder},
};

/// Process-wide runtime service registry installed during early boot.
pub struct RuntimeServices {
    registry: &'static Registry,
    frame_allocator: &'static dyn FrameAllocatorTrait,
    namespaces: &'static NamespaceSet,
    futex: FutexManager,
    interrupts: InterruptRuntime,
    pci: PciRuntime,
}

impl RuntimeServices {
    /// Create the installed runtime service registry.
    pub fn new(
        registry: &'static Registry,
        frame_allocator: &'static dyn FrameAllocatorTrait,
        namespaces: &'static NamespaceSet,
    ) -> Self {
        Self {
            registry,
            frame_allocator,
            namespaces,
            futex: FutexManager::new(),
            interrupts: InterruptRuntime::new(),
            pci: PciRuntime::new(),
        }
    }

    /// Return the global object registry.
    pub fn registry(&self) -> &'static Registry {
        self.registry
    }

    /// Return the active frame allocator service.
    pub fn frame_allocator(&self) -> &'static dyn FrameAllocatorTrait {
        self.frame_allocator
    }

    /// Return the canonical runtime namespace set.
    pub fn namespaces(&self) -> &'static NamespaceSet {
        self.namespaces
    }

    /// Return the runtime-owned futex wait/wake service.
    pub fn futex(&self) -> &FutexManager {
        &self.futex
    }

    /// Install runtime services after bootstrap subsystems are ready.
    pub fn install(services: RuntimeServices) -> &'static RuntimeServices {
        match RUNTIME_SERVICES.try_init(services) {
            Ok(()) | Err(TryInitError::AlreadyInitialized(_)) => RUNTIME_SERVICES.get(),
            Err(TryInitError::Initializing(_)) => {
                panic!("runtime services initialization in progress")
            }
        }
    }

    /// Return the installed runtime service registry.
    pub fn global() -> &'static RuntimeServices {
        RUNTIME_SERVICES.get()
    }

    /// Install boot information early so address translation can use it before
    /// the full runtime service registry is available.
    pub fn install_boot_info(info: *mut libakarin_boot_proto::BootInfo) {
        assert!(!info.is_null(), "boot info pointer must be non-null");
        match BOOT_INFO_PTR.try_init(info as usize) {
            Ok(()) | Err(TryInitError::AlreadyInitialized(_)) => {}
            Err(TryInitError::Initializing(_)) => {
                panic!("boot info initialization in progress")
            }
        }
    }

    /// Return the registered boot information.
    pub fn boot_info() -> &'static libakarin_boot_proto::BootInfo {
        let ptr = *BOOT_INFO_PTR.get() as *const libakarin_boot_proto::BootInfo;
        unsafe { &*ptr }
    }

    /// Install the active interrupt controller exactly once.
    pub fn install_interrupt_controller(
        &self,
        controller: &'static InterruptController,
    ) -> &'static InterruptController {
        self.interrupts.install_controller(controller)
    }

    /// Return the active interrupt controller.
    pub fn interrupt_controller(&self) -> &'static InterruptController {
        self.interrupts.controller()
    }

    /// Install the active PCI ECAM configuration access and interrupt binder.
    pub fn install_pci_runtime(
        &self,
        config: &'static PciEcamConfig,
        binder: &'static PciInterruptBinder<InterruptController, PciEcamConfig>,
    ) -> &'static PciInterruptBinder<InterruptController, PciEcamConfig> {
        self.pci.install(config, binder)
    }

    /// Return whether one PCI runtime has been installed.
    pub fn has_pci_runtime(&self) -> bool {
        self.pci.is_installed()
    }

    /// Return the installed PCI runtime service.
    pub fn pci(&self) -> &PciRuntime {
        &self.pci
    }

    /// Install the per-CPU reschedule-IPI hook.
    pub fn install_reschedule_ipi_hook(&self, hook: fn()) -> fn() {
        self.interrupts.install_reschedule_hook(hook)
    }

    /// Return the installed reschedule-IPI hook.
    pub fn reschedule_ipi_hook(&self) -> fn() {
        self.interrupts.reschedule_hook()
    }
}

static BOOT_INFO_PTR: Once<usize, ScopedGuard<NoOp>> = Once::new();
static RUNTIME_SERVICES: Once<RuntimeServices, ScopedGuard<NoOp>> = Once::new();
