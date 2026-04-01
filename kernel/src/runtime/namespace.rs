use alloc::string::ToString;

use libakarin_core::bootloader::BootloaderManager;
use libakarin_object::{
    Capability, Handle, NameSpace, ObjectError, ObjectLifecycleFlags, Payload, ResourceManager,
    WriteOperation,
};

use crate::{
    ClockSourceManager, ProcessManager, device::manager::KernelDeviceManager,
    interrupt::IrqResourceManager,
};

/// Canonical namespace handles and managers used by kernel runtime.
pub struct NamespaceSet {
    user_super: Handle,
    bootloader_manager: BootloaderManager,
    device_manager: KernelDeviceManager,
    clock_source_manager: ClockSourceManager,
    scheduler_manager: ProcessManager,
    resource_manager: ResourceManager,
    irq_resource_manager: IrqResourceManager,
    syscall_super: Handle,
}

impl NamespaceSet {
    /// Return the preserved `/User` namespace super handle.
    pub fn user_super(&self) -> &Handle {
        &self.user_super
    }

    /// Return the manager that owns `/Kernel/Device`.
    pub fn device_manager(&self) -> &KernelDeviceManager {
        &self.device_manager
    }

    /// Return the manager that owns `/Kernel/Bootloader`.
    pub fn bootloader_manager(&self) -> &BootloaderManager {
        &self.bootloader_manager
    }

    /// Return the manager that owns `/Kernel/ClockSource`.
    pub fn clock_source_manager(&self) -> &ClockSourceManager {
        &self.clock_source_manager
    }

    /// Return the manager that owns `/Kernel/Scheduler/Processes`.
    pub fn scheduler_manager(&self) -> &ProcessManager {
        &self.scheduler_manager
    }

    /// Return the manager that owns `/Resource`.
    pub fn resource_manager(&self) -> &ResourceManager {
        &self.resource_manager
    }

    /// Return the manager that owns `/Resource/IRQ`.
    pub fn irq_resource_manager(&self) -> &IrqResourceManager {
        &self.irq_resource_manager
    }

    /// Return the reserved `/Kernel/Syscall` super handle.
    pub fn syscall_super(&self) -> &Handle {
        &self.syscall_super
    }
}

/// Object responsible for constructing standard top-level namespaces.
///
/// This builder enforces capability flow: namespace creation is always done by
/// an object that owns a writable parent handle, instead of ad-hoc kernel code.
pub struct NamespaceBootstrap {
    root_super: Handle,
}

impl NamespaceBootstrap {
    /// Create a bootstrap object from the root namespace super handle.
    pub fn new(root_super: Handle) -> Self {
        Self { root_super }
    }

    fn create_namespace(parent: &Handle, name: &str) -> Result<Handle, ObjectError> {
        let handle = parent.write_with(|ns: &dyn WriteOperation| {
            ns.add_child(
                name.to_string(),
                Capability::AGENT | Capability::ADMIN,
                Payload::new(NameSpace),
            )
        })??;
        handle.set_lifecycle_flags(ObjectLifecycleFlags::STICKY)?;
        Ok(handle)
    }

    /// Build standard namespaces and return manager handles.
    ///
    /// Layout:
    /// - `/User`
    /// - `/Kernel/{Bootloader,Device,ClockSource,Runtime,Scheduler/Processes,
    ///   Syscall}`
    /// - `/Resource/{IRQ,...}`
    pub fn initialize(self) -> Result<NamespaceSet, ObjectError> {
        let user_super = Self::create_namespace(&self.root_super, "User")?;
        let kernel_super = Self::create_namespace(&self.root_super, "Kernel")?;
        let bootloader_super = Self::create_namespace(&kernel_super, "Bootloader")?;
        let device_super = Self::create_namespace(&kernel_super, "Device")?;
        let clock_super = Self::create_namespace(&kernel_super, "ClockSource")?;
        let runtime_super = Self::create_namespace(&kernel_super, "Runtime")?;
        let scheduler_super = Self::create_namespace(&kernel_super, "Scheduler")?;
        let processes_super = Self::create_namespace(&scheduler_super, "Processes")?;
        let syscall_super = Self::create_namespace(&kernel_super, "Syscall")?;
        let resource_super = Self::create_namespace(&self.root_super, "Resource")?;
        let irq_super = Self::create_namespace(&resource_super, "IRQ")?;

        // Construction-only parents are intentionally leaked to preserve the
        // namespace objects while preventing accidental over-privileged reuse.
        unsafe { kernel_super.forget() };
        unsafe { runtime_super.forget() };
        unsafe { scheduler_super.forget() };
        unsafe { self.root_super.forget() };

        Ok(NamespaceSet {
            user_super,
            bootloader_manager: BootloaderManager::new(bootloader_super),
            device_manager: KernelDeviceManager::new(device_super),
            clock_source_manager: ClockSourceManager::new(clock_super),
            scheduler_manager: ProcessManager::new(processes_super),
            resource_manager: ResourceManager::new(resource_super),
            irq_resource_manager: IrqResourceManager::new(irq_super),
            syscall_super,
        })
    }
}

/// Compatibility helper; prefer [`NamespaceBootstrap::initialize`].
pub fn init_standard_namespaces(root_super: &Handle) -> Result<NamespaceSet, ObjectError> {
    let root = root_super.derive_handle(Capability::WRITE | Capability::READ, 0)?;
    NamespaceBootstrap::new(root).initialize()
}
