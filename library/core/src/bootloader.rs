use alloc::{string::ToString, sync::Arc};

use libakarin_boot_proto::{BootInfo, KernelSymtab, display::DisplayMode, memory::Arena};
use libakarin_machine_core::sync::{NoOp, ScopedGuard};
use libakarin_object::{
    Capability, ControlPlane, Handle, NameSpace, ObjectError, ObjectLifecycleFlags, Payload,
    SyscallDispatch, WriteOperation,
};
use libakarin_sync::spin::SpinLock;

/// Manager for objects under `/Kernel/Bootloader`.
///
/// Direct plane:
/// - publish boot resources
/// - consume one-shot boot resources
/// - read bootstrap-only firmware metadata
///
/// Capability plane:
/// - derive delegated handles
/// - discover published objects by relative path
pub struct BootloaderManager {
    namespace: Handle,
    bootstrap_program: SpinLock<Option<&'static [u8]>, ScopedGuard<NoOp>>,
    framebuffer: SpinLock<
        Option<Arc<SpinLock<Option<BootFramebufferData>, ScopedGuard<NoOp>>>>,
        ScopedGuard<NoOp>,
    >,
    firmware_info: SpinLock<Option<(Option<usize>, usize)>, ScopedGuard<NoOp>>,
}

impl BootloaderManager {
    /// Create a manager from a namespace handle with `WRITE` capability.
    pub fn new(namespace: Handle) -> Self {
        Self {
            namespace,
            bootstrap_program: SpinLock::new(None),
            framebuffer: SpinLock::new(None),
            firmware_info: SpinLock::new(None),
        }
    }

    /// Register a bootloader-provided resource object.
    pub fn register_resource<T>(&self, name: &str, payload: T) -> Result<Handle, ObjectError>
    where
        T: ControlPlane + Send + Sync + 'static,
        for<'a> T::ReadGuard<'a>: Send,
        for<'a> T::WriteGuard<'a>: Send,
        for<'a> T::ExecuteGuard<'a>: Send,
        for<'a> T::AgentGuard<'a>: Send,
        for<'a> T::AdminGuard<'a>: Send,
    {
        let handle = self.namespace.write_with(|ns: &dyn WriteOperation| {
            ns.add_child(
                name.to_string(),
                Capability::ADMIN | Capability::AGENT,
                Payload::new(payload),
            )
        })??;
        handle.set_lifecycle_flags(ObjectLifecycleFlags::STICKY)?;
        Ok(handle)
    }

    /// Create a child namespace under `/Kernel/Bootloader`.
    pub fn create_namespace(&self, name: &str) -> Result<Handle, ObjectError> {
        self.register_resource(name, NameSpace)
    }

    /// Lookup a published bootloader object through the discoverable namespace
    /// plane.
    pub fn lookup_handle(&self, path: &str) -> Result<Handle, ObjectError> {
        self.namespace.locate(path)
    }

    /// Remove one bootloader-published object through the manager-owned delete
    /// path.
    pub fn remove_published(&self, name: &str) -> Result<(), ObjectError> {
        self.namespace
            .write_with(|ns: &dyn WriteOperation| ns.remove_child(name, true).map(|_| ()))??;
        match name {
            "Framebuffer" => {
                *self.framebuffer.lock() = None;
            }
            "FirmwareInfo" => {
                *self.firmware_info.lock() = None;
            }
            _ => {}
        }
        Ok(())
    }

    /// Request a derived handle from the manager-owned namespace handle.
    pub fn request_handle(
        &self,
        capability: Capability,
        interface_caps: u32,
    ) -> Result<Handle, ObjectError> {
        self.namespace.derive_handle(capability, interface_caps)
    }

    /// Return the immutable bootstrap program image used for the first
    /// userspace process.
    pub fn bootstrap_program_bytes(&self) -> Result<&'static [u8], ObjectError> {
        let image = self.bootstrap_program.lock();
        match image.as_ref().copied() {
            Some(program) => Ok(program),
            None => Err(ObjectError::ObjectNotFound),
        }
    }

    /// Transfer the bootloader-provided framebuffer handoff payload.
    pub fn take_framebuffer(&self) -> Result<Option<BootFramebufferData>, ObjectError> {
        let framebuffer = self.framebuffer.lock().as_ref().cloned();
        match framebuffer {
            Some(framebuffer) => Ok(framebuffer.lock().take()),
            None => Err(ObjectError::ObjectNotFound),
        }
    }

    /// Return firmware metadata needed by early drivers.
    pub fn firmware_info(&self) -> Result<(Option<usize>, usize), ObjectError> {
        let firmware = self.firmware_info.lock();
        match firmware.as_ref().copied() {
            Some(info) => Ok(info),
            None => Err(ObjectError::ObjectNotFound),
        }
    }

    /// Publish bootloader-provided resources into `/Kernel/Bootloader`.
    pub fn publish_boot_info_resources(
        &self,
        boot_info: &'static mut BootInfo,
    ) -> Result<(), ObjectError> {
        let image_info = BootloaderImageInfo::from_boot_info(boot_info);
        let image_super = self.register_resource("ImageInfo", image_info)?;
        unsafe { image_super.forget() };

        let firmware_info = BootloaderFirmwareInfo::from_boot_info(boot_info);
        let firmware_super = self.register_resource("FirmwareInfo", firmware_info)?;
        *self.firmware_info.lock() = Some((boot_info.rsdp, boot_info.physical_memory_offset));
        unsafe { firmware_super.forget() };

        if let Some((buffer, mode)) = boot_info.framebuffer.take() {
            let framebuffer = Arc::new(SpinLock::new(Some(BootFramebufferData { buffer, mode })));
            let framebuffer_super = self.register_resource(
                "Framebuffer",
                BootloaderFramebuffer::new(framebuffer.clone()),
            )?;
            *self.framebuffer.lock() = Some(framebuffer);
            unsafe { framebuffer_super.forget() };
        }

        if let Some(image) = boot_info.bootstrap.take() {
            let programs_super = self.create_namespace("Programs")?;
            let bootstrap_super = programs_super.write_with(|ns: &dyn WriteOperation| {
                ns.add_child(
                    "bootstrap".to_string(),
                    Capability::ADMIN | Capability::AGENT,
                    Payload::new(BootloaderProgramImage::new(image)),
                )
            })??;
            *self.bootstrap_program.lock() = Some(image);
            bootstrap_super.set_lifecycle_flags(ObjectLifecycleFlags::STICKY)?;
            unsafe { bootstrap_super.forget() };
            unsafe { programs_super.forget() };
        }

        Ok(())
    }
}

/// Immutable kernel image metadata handed off by the bootloader.
pub struct BootloaderImageInfo {
    slide: usize,
    symtab: Option<KernelSymtab>,
    mapped_sections: Option<&'static [(&'static str, Arena)]>,
}

impl BootloaderImageInfo {
    /// Build image metadata from BootInfo.
    pub fn from_boot_info(boot_info: &BootInfo) -> Self {
        let image_info = boot_info.image_info.as_ref();
        Self {
            slide: image_info.map(|info| info.slide).unwrap_or(0),
            symtab: image_info.and_then(|info| {
                info.symtab.as_ref().map(|symtab| KernelSymtab {
                    sym_addr: symtab.sym_addr,
                    num_syms: symtab.num_syms,
                    str_addr: symtab.str_addr,
                    str_size: symtab.str_size,
                })
            }),
            mapped_sections: image_info.and_then(|info| info.mapped_sections),
        }
    }

    /// Return the kernel image slide.
    pub fn slide(&self) -> usize {
        self.slide
    }

    /// Return the bootloader-provided symbol table metadata.
    pub fn symtab(&self) -> Option<&KernelSymtab> {
        self.symtab.as_ref()
    }

    /// Return the sections mapped by the bootloader.
    pub fn mapped_sections(&self) -> Option<&'static [(&'static str, Arena)]> {
        self.mapped_sections
    }
}

impl ControlPlane for BootloaderImageInfo {
    type ReadGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type WriteGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type AgentGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type AdminGuard<'a>
        = &'a Self
    where
        Self: 'a;

    fn read(&self, _interface_caps: u32) -> Self::ReadGuard<'_> {
        self
    }

    fn write(&self, _interface_caps: u32) -> Self::WriteGuard<'_> {
        self
    }

    fn execute(&self, _interface_caps: u32) -> Self::ExecuteGuard<'_> {
        self
    }

    fn agent(&self, _interface_caps: u32) -> Self::AgentGuard<'_> {
        self
    }

    fn admin(&self, _interface_caps: u32) -> Self::AdminGuard<'_> {
        self
    }
}

impl SyscallDispatch<libakarin_object::ObjectSyscallContext> for BootloaderImageInfo {}

/// Firmware/environment metadata handed off by the bootloader.
pub struct BootloaderFirmwareInfo {
    rsdp: Option<usize>,
    physical_memory_offset: usize,
}

impl BootloaderFirmwareInfo {
    /// Build firmware metadata from BootInfo.
    pub fn from_boot_info(boot_info: &BootInfo) -> Self {
        Self {
            rsdp: boot_info.rsdp,
            physical_memory_offset: boot_info.physical_memory_offset,
        }
    }

    /// Return the firmware RSDP, if present.
    pub fn rsdp(&self) -> Option<usize> {
        self.rsdp
    }

    /// Return the higher-half direct-map physical memory offset.
    pub fn physical_memory_offset(&self) -> usize {
        self.physical_memory_offset
    }
}

impl ControlPlane for BootloaderFirmwareInfo {
    type ReadGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type WriteGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type AgentGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type AdminGuard<'a>
        = &'a Self
    where
        Self: 'a;

    fn read(&self, _interface_caps: u32) -> Self::ReadGuard<'_> {
        self
    }

    fn write(&self, _interface_caps: u32) -> Self::WriteGuard<'_> {
        self
    }

    fn execute(&self, _interface_caps: u32) -> Self::ExecuteGuard<'_> {
        self
    }

    fn agent(&self, _interface_caps: u32) -> Self::AgentGuard<'_> {
        self
    }

    fn admin(&self, _interface_caps: u32) -> Self::AdminGuard<'_> {
        self
    }
}

impl SyscallDispatch<libakarin_object::ObjectSyscallContext> for BootloaderFirmwareInfo {}

/// Bootloader-provided framebuffer payload transferred once into a driver.
pub struct BootFramebufferData {
    pub buffer: &'static mut [u8],
    pub mode: DisplayMode,
}

/// One-shot framebuffer handoff object under `/Kernel/Bootloader/Framebuffer`.
pub struct BootloaderFramebuffer {
    framebuffer: Arc<SpinLock<Option<BootFramebufferData>, ScopedGuard<NoOp>>>,
}

impl BootloaderFramebuffer {
    /// Create a new bootloader framebuffer handoff object.
    pub fn new(framebuffer: Arc<SpinLock<Option<BootFramebufferData>, ScopedGuard<NoOp>>>) -> Self {
        Self { framebuffer }
    }

    /// Transfer framebuffer ownership to the caller.
    pub fn take(&self) -> Option<BootFramebufferData> {
        self.framebuffer.lock().take()
    }

    /// Return whether the framebuffer has not yet been claimed.
    pub fn is_available(&self) -> bool {
        self.framebuffer.lock().is_some()
    }
}

impl ControlPlane for BootloaderFramebuffer {
    type ReadGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type WriteGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type AgentGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type AdminGuard<'a>
        = &'a Self
    where
        Self: 'a;

    fn read(&self, _interface_caps: u32) -> Self::ReadGuard<'_> {
        self
    }

    fn write(&self, _interface_caps: u32) -> Self::WriteGuard<'_> {
        self
    }

    fn execute(&self, _interface_caps: u32) -> Self::ExecuteGuard<'_> {
        self
    }

    fn agent(&self, _interface_caps: u32) -> Self::AgentGuard<'_> {
        self
    }

    fn admin(&self, _interface_caps: u32) -> Self::AdminGuard<'_> {
        self
    }
}

impl SyscallDispatch<libakarin_object::ObjectSyscallContext> for BootloaderFramebuffer {}

/// Immutable bootstrap userspace image published by the bootloader.
pub struct BootloaderProgramImage {
    image: &'static [u8],
}

impl BootloaderProgramImage {
    /// Create one immutable program-image resource.
    pub const fn new(image: &'static [u8]) -> Self {
        Self { image }
    }

    /// Return the raw image bytes.
    pub fn bytes(&self) -> &'static [u8] {
        self.image
    }

    /// Return the image size in bytes.
    pub fn len(&self) -> usize {
        self.image.len()
    }

    /// Return whether the image is empty.
    pub fn is_empty(&self) -> bool {
        self.image.is_empty()
    }
}

impl ControlPlane for BootloaderProgramImage {
    type ReadGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type WriteGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type AgentGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type AdminGuard<'a>
        = &'a Self
    where
        Self: 'a;

    fn read(&self, _interface_caps: u32) -> Self::ReadGuard<'_> {
        self
    }

    fn write(&self, _interface_caps: u32) -> Self::WriteGuard<'_> {
        self
    }

    fn execute(&self, _interface_caps: u32) -> Self::ExecuteGuard<'_> {
        self
    }

    fn agent(&self, _interface_caps: u32) -> Self::AgentGuard<'_> {
        self
    }

    fn admin(&self, _interface_caps: u32) -> Self::AdminGuard<'_> {
        self
    }
}

impl SyscallDispatch<libakarin_object::ObjectSyscallContext> for BootloaderProgramImage {}
