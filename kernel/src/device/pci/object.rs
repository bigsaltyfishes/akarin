use alloc::{boxed::Box, string::String, vec::Vec};
use core::{convert::TryFrom, mem::MaybeUninit};

use async_trait::async_trait;
use libakarin_machine_core::interrupt::{InterruptControllerTrait, IrqError, MessageIrqBlockTrait};
use libakarin_object::{
    Capability, ControlPlane, Handle, ObjectError, ObjectSyscallContext, Payload, SyscallDispatch,
    WriteOperation,
};
use libakarin_sync::spin::SpinLock;
use libakarin_syscall::{
    IRQ_SESSION_ACK, IRQ_SESSION_QUERY, IRQ_SESSION_WAIT, ObjectLifecycleFlags,
    PCI_FUNCTION_INTERRUPT, PCI_FUNCTION_QUERY, PCI_HOST_QUERY, PCI_INTERRUPT_MODE_INTX,
    PCI_INTERRUPT_MODE_MSI, PCI_INTERRUPT_MODE_MSIX, PciFunctionMethod, PciHostMethod,
    PciInterruptBindArgs, PciInterruptModeKind, SYSCALL_STATUS_OK, SyscallFailure, SyscallResult,
    errno::PciUnderlyingErrorCode,
};

use super::{
    PciBarSummary, PciBdf, PciCapabilitySummary, PciConfigAccess, PciEcamConfig,
    PciInterruptAllocation, PciInterruptCapabilities, PciInterruptMode,
};
use crate::{
    RuntimeServices,
    arch::{
        guards::IrqSaveGuard,
        interrupt::{IrqMessageBlock, IrqSession},
    },
    device::manager::KernelDeviceManager,
    interrupt::SharedIrqSessionObject,
};

type ActivePciInterruptAllocation = PciInterruptAllocation<IrqMessageBlock, IrqSession>;

struct PciInterruptState {
    configuring: bool,
    active: Option<ActivePciInterruptAllocation>,
}

impl PciInterruptState {
    fn new() -> Self {
        Self {
            configuring: false,
            active: None,
        }
    }
}

struct PciSyscallCodec;

impl PciSyscallCodec {
    fn ok(values: [usize; 5]) -> SyscallResult {
        [
            SYSCALL_STATUS_OK,
            values[0],
            values[1],
            values[2],
            values[3],
            values[4],
        ]
        .into()
    }

    fn host_summary(function_count: usize, runtime_available: bool) -> SyscallResult {
        Self::ok([function_count, runtime_available as usize, 0, 0, 0])
    }

    fn function_info(info: PciFunctionInfo) -> SyscallResult {
        Self::ok([
            usize::from(info.bdf.segment),
            (usize::from(info.bdf.bus) << 16)
                | (usize::from(info.bdf.device) << 8)
                | usize::from(info.bdf.function),
            usize::from(info.vendor_id) | (usize::from(info.device_id) << 16),
            usize::from(info.revision_id)
                | (usize::from(info.prog_if) << 8)
                | (usize::from(info.subclass) << 16)
                | (usize::from(info.class_code) << 24),
            usize::from(info.header_type),
        ])
    }

    fn interrupt_modes(
        supported_modes: usize,
        current_mode: PciInterruptModeKind,
        max_msi_vectors: usize,
        max_msix_vectors: usize,
    ) -> SyscallResult {
        Self::ok([
            supported_modes,
            current_mode as usize,
            max_msi_vectors,
            max_msix_vectors,
            0,
        ])
    }

    fn bar_summary(summary: PciBarSummary) -> SyscallResult {
        Self::ok([
            summary.bar_count,
            summary.present_mask,
            summary.io_mask,
            summary.mmio64_mask,
            summary.prefetchable_mask,
        ])
    }

    fn capability_summary(summary: PciCapabilitySummary) -> SyscallResult {
        Self::ok([
            summary.capability_bits,
            summary.max_msi_vectors,
            summary.max_msix_vectors,
            0,
            0,
        ])
    }

    fn enabled(count: usize, current_mode: PciInterruptModeKind) -> SyscallResult {
        Self::ok([count, current_mode as usize, 0, 0, 0])
    }

    fn disabled() -> SyscallResult {
        Self::ok([0, 0, 0, 0, 0])
    }

    fn underlying(error: PciInvokeError) -> SyscallResult {
        SyscallResult::from(SyscallFailure::from(error.code()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PciInvokeError {
    InvalidParameter,
    NotSupported,
    OutOfResources,
    Fault,
    BufferTooSmall,
    RuntimeUnavailable,
    Busy,
    ControllerFailure,
}

impl PciInvokeError {
    fn code(self) -> PciUnderlyingErrorCode {
        match self {
            Self::InvalidParameter => PciUnderlyingErrorCode::InvalidParameter,
            Self::NotSupported => PciUnderlyingErrorCode::NotSupported,
            Self::OutOfResources => PciUnderlyingErrorCode::OutOfResources,
            Self::Fault => PciUnderlyingErrorCode::Fault,
            Self::BufferTooSmall => PciUnderlyingErrorCode::BufferTooSmall,
            Self::RuntimeUnavailable => PciUnderlyingErrorCode::RuntimeUnavailable,
            Self::Busy => PciUnderlyingErrorCode::Busy,
            Self::ControllerFailure => PciUnderlyingErrorCode::ControllerFailure,
        }
    }
}

impl From<IrqError> for PciInvokeError {
    fn from(value: IrqError) -> Self {
        match value {
            IrqError::InvalidParameter => Self::InvalidParameter,
            IrqError::NotSupported => Self::NotSupported,
            IrqError::OutOfResources => Self::OutOfResources,
            IrqError::InvalidIrq(_)
            | IrqError::WouldBlock
            | IrqError::Closed
            | IrqError::DeadlineUnsupported => Self::ControllerFailure,
        }
    }
}

enum PciEnableError {
    Object(ObjectError),
    Underlying(PciInvokeError),
}

/// Stable summary for one discovered PCI function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciFunctionInfo {
    pub bdf: PciBdf,
    pub vendor_id: u16,
    pub device_id: u16,
    pub revision_id: u8,
    pub prog_if: u8,
    pub subclass: u8,
    pub class_code: u8,
    pub header_type: u8,
    pub interrupt_mode: PciInterruptMode,
}

impl PciFunctionInfo {
    fn from_config(config: &PciEcamConfig, bdf: PciBdf) -> Option<Self> {
        let vendor_id = config.read_u16(bdf, 0x00);
        if vendor_id == u16::MAX {
            return None;
        }

        let device_id = config.read_u16(bdf, 0x02);
        let revision_id = config.read_u8(bdf, 0x08);
        let prog_if = config.read_u8(bdf, 0x09);
        let subclass = config.read_u8(bdf, 0x0a);
        let class_code = config.read_u8(bdf, 0x0b);
        let header_type = config.read_u8(bdf, 0x0e);
        let interrupt_mode = PciInterruptCapabilities::new(config, bdf).interrupt_mode();
        Some(Self {
            bdf,
            vendor_id,
            device_id,
            revision_id,
            prog_if,
            subclass,
            class_code,
            header_type,
            interrupt_mode,
        })
    }

    /// Return one stable namespace key for this function.
    pub fn object_name(&self) -> String {
        self.bdf.object_name()
    }
}

/// Published PCI host object and its discovered function resources.
pub struct PublishedPciHostBridge<'a> {
    config: &'a PciEcamConfig,
}

impl<'a> PublishedPciHostBridge<'a> {
    /// Create one publisher over the installed ECAM configuration access.
    pub const fn new(config: &'a PciEcamConfig) -> Self {
        Self { config }
    }

    /// Publish the PCI host object under `/Kernel/Device/PCI` and every
    /// discovered PCI function under `/Resource/PCI/<BDF>`.
    pub fn publish(
        &self,
        device_manager: &KernelDeviceManager,
        resource_manager: &libakarin_object::ResourceManager,
    ) -> Result<Handle, ObjectError> {
        let functions = self.enumerate_functions();
        let resource_namespace = resource_manager.create_namespace("PCI")?;

        for info in &functions {
            let owner = resource_namespace.write_with(|ns: &dyn WriteOperation| {
                ns.add_child(
                    info.object_name(),
                    // PCI function resources are driver-facing control
                    // objects. Public lookups keep READ/WRITE/EXECUTE while
                    // ownership and agent authority stay inside the kernel.
                    Capability::ADMIN | Capability::AGENT,
                    Payload::new(PciFunction::new(*info)),
                )
            })??;
            owner.set_lifecycle_flags(ObjectLifecycleFlags::STICKY)?;
            owner.write_with(|object: &dyn WriteOperation| {
                object.set_public_interface_caps(PCI_FUNCTION_QUERY | PCI_FUNCTION_INTERRUPT)
            })?;
            unsafe { owner.forget() };
        }

        let host = device_manager
            .register_driver("PCI", PciHostBridge::new(resource_namespace, functions))?;
        host.write_with(|object: &dyn WriteOperation| {
            object.set_public_interface_caps(PCI_HOST_QUERY)
        })?;
        Ok(host)
    }

    fn enumerate_functions(&self) -> Vec<PciFunctionInfo> {
        let mut functions = Vec::new();
        self.config.for_each_present_function(|bdf| {
            if let Some(info) = PciFunctionInfo::from_config(self.config, bdf) {
                functions.push(info);
            }
        });
        functions
    }
}

/// `/Kernel/Device/PCI` host object.
pub struct PciHostBridge {
    _functions_namespace: Handle,
    functions: Vec<PciFunctionInfo>,
}

impl PciHostBridge {
    fn new(functions_namespace: Handle, functions: Vec<PciFunctionInfo>) -> Self {
        Self {
            _functions_namespace: functions_namespace,
            functions,
        }
    }

    /// Return the discovered PCI function count.
    fn function_count(&self) -> usize {
        self.functions.len()
    }

    /// Return one immutable snapshot over the discovered PCI functions.
    pub fn functions(&self) -> &[PciFunctionInfo] {
        &self.functions
    }

    fn query_summary_words(&self) -> SyscallResult {
        PciSyscallCodec::host_summary(
            self.function_count(),
            RuntimeServices::global().has_pci_runtime(),
        )
    }
}

/// Guard used for access modes that one PCI object does not expose.
pub struct PciUnsupportedGuard;

#[async_trait]
impl SyscallDispatch<ObjectSyscallContext> for PciUnsupportedGuard {
    async fn dispatch(
        &self,
        _caller: &ObjectSyscallContext,
        _method_id: usize,
        _arg1: usize,
        _arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        Err(ObjectError::InsufficientCapabilities)
    }
}

/// Read-side guard for the published PCI host object.
pub struct PciHostReadGuard<'a> {
    host: &'a PciHostBridge,
    interface_caps: u32,
}

impl PciHostReadGuard<'_> {
    fn require_caps(&self, caps: u32) -> Result<(), ObjectError> {
        if self.interface_caps == u32::MAX || (self.interface_caps & caps) == caps {
            Ok(())
        } else {
            Err(ObjectError::InsufficientCapabilities)
        }
    }

    pub fn function_snapshot(&self) -> &[PciFunctionInfo] {
        self.host.functions()
    }
}

#[async_trait]
impl SyscallDispatch<ObjectSyscallContext> for PciHostReadGuard<'_> {
    async fn dispatch(
        &self,
        _caller: &ObjectSyscallContext,
        method_id: usize,
        _arg1: usize,
        _arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let Ok(method) = PciHostMethod::try_from(method_id) else {
            return Ok(PciSyscallCodec::underlying(
                PciInvokeError::InvalidParameter,
            ));
        };
        match method {
            PciHostMethod::QuerySummary => {
                self.require_caps(PCI_HOST_QUERY)?;
                Ok(self.host.query_summary_words())
            }
        }
    }
}

impl ControlPlane for PciHostBridge {
    type ReadGuard<'a>
        = PciHostReadGuard<'a>
    where
        Self: 'a;
    type WriteGuard<'a>
        = PciUnsupportedGuard
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = PciUnsupportedGuard
    where
        Self: 'a;
    type AgentGuard<'a>
        = PciUnsupportedGuard
    where
        Self: 'a;
    type AdminGuard<'a>
        = PciUnsupportedGuard
    where
        Self: 'a;

    fn read(&self, interface_caps: u32) -> Self::ReadGuard<'_> {
        PciHostReadGuard {
            host: self,
            interface_caps,
        }
    }

    fn write(&self, _interface_caps: u32) -> Self::WriteGuard<'_> {
        PciUnsupportedGuard
    }

    fn execute(&self, _interface_caps: u32) -> Self::ExecuteGuard<'_> {
        PciUnsupportedGuard
    }

    fn agent(&self, _interface_caps: u32) -> Self::AgentGuard<'_> {
        PciUnsupportedGuard
    }

    fn admin(&self, _interface_caps: u32) -> Self::AdminGuard<'_> {
        PciUnsupportedGuard
    }
}

#[async_trait]
impl SyscallDispatch<ObjectSyscallContext> for PciHostBridge {}

/// `/Resource/PCI/<BDF>` function object.
pub struct PciFunction {
    info: PciFunctionInfo,
    interrupts: SpinLock<PciInterruptState, IrqSaveGuard>,
}

impl PciFunction {
    fn new(info: PciFunctionInfo) -> Self {
        Self {
            info,
            interrupts: SpinLock::new(PciInterruptState::new()),
        }
    }

    fn query_info_words(&self) -> SyscallResult {
        PciSyscallCodec::function_info(self.info)
    }

    fn query_interrupt_modes_words(&self) -> Result<SyscallResult, PciInvokeError> {
        if !RuntimeServices::global().has_pci_runtime() {
            return Err(PciInvokeError::RuntimeUnavailable);
        }

        let caps = RuntimeServices::global()
            .pci()
            .interrupt_capabilities(self.info.bdf);
        let mut supported_modes = PCI_INTERRUPT_MODE_INTX;
        let mut max_msi_vectors = 0usize;
        let mut max_msix_vectors = 0usize;
        if let Some(msi) = caps.msi() {
            supported_modes |= PCI_INTERRUPT_MODE_MSI;
            max_msi_vectors = msi.max_messages;
        }
        if let Some(msix) = caps.msix() {
            supported_modes |= PCI_INTERRUPT_MODE_MSIX;
            max_msix_vectors = msix.table_size;
        }

        Ok(PciSyscallCodec::interrupt_modes(
            supported_modes,
            self.current_interrupt_mode(),
            max_msi_vectors,
            max_msix_vectors,
        ))
    }

    fn query_bar_summary_words(&self) -> Result<SyscallResult, PciInvokeError> {
        if !RuntimeServices::global().has_pci_runtime() {
            return Err(PciInvokeError::RuntimeUnavailable);
        }
        Ok(PciSyscallCodec::bar_summary(
            RuntimeServices::global().pci().bar_summary(self.info.bdf),
        ))
    }

    fn query_capability_summary_words(&self) -> Result<SyscallResult, PciInvokeError> {
        if !RuntimeServices::global().has_pci_runtime() {
            return Err(PciInvokeError::RuntimeUnavailable);
        }
        Ok(PciSyscallCodec::capability_summary(
            RuntimeServices::global()
                .pci()
                .capability_summary(self.info.bdf),
        ))
    }

    fn current_interrupt_mode(&self) -> PciInterruptModeKind {
        match self
            .interrupts
            .lock()
            .active
            .as_ref()
            .map(|allocation| allocation.block.kind())
        {
            Some(libakarin_machine_core::interrupt::MessageIrqKind::Msi) => {
                PciInterruptModeKind::Msi
            }
            Some(libakarin_machine_core::interrupt::MessageIrqKind::Msix) => {
                PciInterruptModeKind::Msix
            }
            None => PciInterruptModeKind::Intx,
        }
    }

    fn begin_reconfigure(&self) -> Result<(), PciInvokeError> {
        let mut state = self.interrupts.lock();
        if state.configuring {
            return Err(PciInvokeError::Busy);
        }
        state.configuring = true;
        Ok(())
    }

    fn finish_reconfigure(&self, new_allocation: Option<ActivePciInterruptAllocation>) {
        let mut state = self.interrupts.lock();
        state.active = new_allocation;
        state.configuring = false;
    }

    fn cancel_reconfigure(&self) {
        self.interrupts.lock().configuring = false;
    }

    fn take_active_allocation(&self) -> Option<ActivePciInterruptAllocation> {
        self.interrupts.lock().active.take()
    }

    fn deactivate_allocation(&self, allocation: ActivePciInterruptAllocation) {
        allocation.block.close_sessions();
        drop(allocation);
    }
}

/// Read-side guard for one PCI function resource object.
pub struct PciFunctionReadGuard<'a> {
    function: &'a PciFunction,
    interface_caps: u32,
}

impl PciFunctionReadGuard<'_> {
    fn require_caps(&self, caps: u32) -> Result<(), ObjectError> {
        if self.interface_caps == u32::MAX || (self.interface_caps & caps) == caps {
            Ok(())
        } else {
            Err(ObjectError::InsufficientCapabilities)
        }
    }
}

#[async_trait]
impl SyscallDispatch<ObjectSyscallContext> for PciFunctionReadGuard<'_> {
    async fn dispatch(
        &self,
        _caller: &ObjectSyscallContext,
        method_id: usize,
        _arg1: usize,
        _arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let Ok(method) = PciFunctionMethod::try_from(method_id) else {
            return Ok(PciSyscallCodec::underlying(
                PciInvokeError::InvalidParameter,
            ));
        };
        match method {
            PciFunctionMethod::QueryInfo => {
                self.require_caps(PCI_FUNCTION_QUERY)?;
                Ok(self.function.query_info_words())
            }
            PciFunctionMethod::QueryInterruptModes => {
                self.require_caps(PCI_FUNCTION_QUERY)?;
                match self.function.query_interrupt_modes_words() {
                    Ok(frame) => Ok(frame),
                    Err(error) => Ok(PciSyscallCodec::underlying(error)),
                }
            }
            PciFunctionMethod::QueryBars => {
                self.require_caps(PCI_FUNCTION_QUERY)?;
                match self.function.query_bar_summary_words() {
                    Ok(frame) => Ok(frame),
                    Err(error) => Ok(PciSyscallCodec::underlying(error)),
                }
            }
            PciFunctionMethod::QueryCapabilities => {
                self.require_caps(PCI_FUNCTION_QUERY)?;
                match self.function.query_capability_summary_words() {
                    Ok(frame) => Ok(frame),
                    Err(error) => Ok(PciSyscallCodec::underlying(error)),
                }
            }
            _ => Err(ObjectError::InsufficientCapabilities),
        }
    }
}

/// Execute-side guard for one PCI function resource object.
pub struct PciFunctionExecuteGuard<'a> {
    function: &'a PciFunction,
    interface_caps: u32,
}

impl PciFunctionExecuteGuard<'_> {
    fn require_interrupt_caps(&self) -> Result<(), ObjectError> {
        if self.interface_caps == u32::MAX
            || (self.interface_caps & PCI_FUNCTION_INTERRUPT) == PCI_FUNCTION_INTERRUPT
        {
            Ok(())
        } else {
            Err(ObjectError::InsufficientCapabilities)
        }
    }

    fn read_bind_args(
        &self,
        caller: &ObjectSyscallContext,
        address: usize,
    ) -> Result<PciInterruptBindArgs, PciInvokeError> {
        let mut value = MaybeUninit::<PciInterruptBindArgs>::uninit();
        let bytes = unsafe {
            core::slice::from_raw_parts_mut(
                value.as_mut_ptr().cast::<u8>(),
                core::mem::size_of::<PciInterruptBindArgs>(),
            )
        };
        caller
            .copy_from_user(address, bytes)
            .map_err(|_| PciInvokeError::Fault)?;
        Ok(unsafe { value.assume_init() })
    }

    fn install_sessions(
        &self,
        caller: &ObjectSyscallContext,
        allocation: &mut ActivePciInterruptAllocation,
    ) -> Result<Vec<u32>, ObjectError> {
        let mut slots = Vec::with_capacity(allocation.sessions.len());
        for session in allocation.sessions.drain(..) {
            match caller.create_anonymous_object(
                Payload::new(SharedIrqSessionObject::new(session)),
                Capability::SEND | Capability::READ | Capability::WRITE | Capability::EXECUTE,
                IRQ_SESSION_QUERY | IRQ_SESSION_WAIT | IRQ_SESSION_ACK,
            ) {
                Ok(slot) => slots.push(slot),
                Err(error) => {
                    self.destroy_sessions(caller, &slots);
                    return Err(error);
                }
            }
        }
        Ok(slots)
    }

    fn destroy_sessions(&self, caller: &ObjectSyscallContext, slots: &[u32]) {
        for slot in slots.iter().copied() {
            let _ = caller.destroy_anonymous_object(slot);
        }
    }

    fn write_slots(
        &self,
        caller: &ObjectSyscallContext,
        slots_ptr: usize,
        slots: &[u32],
    ) -> Result<(), PciInvokeError> {
        let bytes = unsafe {
            core::slice::from_raw_parts(slots.as_ptr().cast::<u8>(), core::mem::size_of_val(slots))
        };
        caller
            .copy_to_user(slots_ptr, bytes)
            .map_err(|_| PciInvokeError::Fault)
    }

    fn abort_allocation(
        &self,
        caller: &ObjectSyscallContext,
        slots: &[u32],
        allocation: ActivePciInterruptAllocation,
    ) {
        self.destroy_sessions(caller, slots);
        if RuntimeServices::global().has_pci_runtime() {
            let _ = RuntimeServices::global()
                .pci()
                .disable_interrupts(self.function.info.bdf);
        }
        self.function.deactivate_allocation(allocation);
        self.function.finish_reconfigure(None);
    }

    fn finish_disable(&self) -> Result<SyscallResult, PciInvokeError> {
        if !RuntimeServices::global().has_pci_runtime() {
            self.function.cancel_reconfigure();
            return Err(PciInvokeError::RuntimeUnavailable);
        }
        let old = self.function.take_active_allocation();
        if let Err(error) = RuntimeServices::global()
            .pci()
            .disable_interrupts(self.function.info.bdf)
        {
            if let Some(allocation) = old {
                self.function.finish_reconfigure(Some(allocation));
            } else {
                self.function.cancel_reconfigure();
            }
            self.function.cancel_reconfigure();
            return Err(PciInvokeError::from(error));
        }
        if let Some(allocation) = old {
            self.function.deactivate_allocation(allocation);
        }
        self.function.finish_reconfigure(None);
        Ok(PciSyscallCodec::disabled())
    }

    fn enable_msi(
        &self,
        caller: &ObjectSyscallContext,
        bind_args: PciInterruptBindArgs,
    ) -> Result<SyscallResult, PciEnableError> {
        if bind_args.count == 0 {
            return Err(PciEnableError::Underlying(PciInvokeError::InvalidParameter));
        }
        if bind_args.slots_len < bind_args.count {
            return Err(PciEnableError::Underlying(PciInvokeError::BufferTooSmall));
        }
        if !RuntimeServices::global().has_pci_runtime() {
            return Err(PciEnableError::Underlying(
                PciInvokeError::RuntimeUnavailable,
            ));
        }

        let cpu_hint =
            (bind_args.cpu_hint != PciInterruptBindArgs::NO_CPU_HINT).then_some(bind_args.cpu_hint);
        let old = self.function.take_active_allocation();
        if let Some(previous) = old {
            let _ = RuntimeServices::global()
                .pci()
                .disable_interrupts(self.function.info.bdf);
            self.function.deactivate_allocation(previous);
        }

        let mut allocation = RuntimeServices::global()
            .pci()
            .allocate_msi(self.function.info.bdf, bind_args.count, cpu_hint)
            .map_err(PciInvokeError::from)
            .map_err(PciEnableError::Underlying)?;
        let slots = match self.install_sessions(caller, &mut allocation) {
            Ok(slots) => slots,
            Err(error) => {
                self.abort_allocation(caller, &[], allocation);
                return Err(PciEnableError::Object(error));
            }
        };
        if let Err(error) = self.write_slots(caller, bind_args.slots_ptr, &slots) {
            self.abort_allocation(caller, &slots, allocation);
            return Err(PciEnableError::Underlying(error));
        }

        self.function.finish_reconfigure(Some(allocation));
        Ok(PciSyscallCodec::enabled(
            slots.len(),
            PciInterruptModeKind::Msi,
        ))
    }

    fn enable_msix(
        &self,
        caller: &ObjectSyscallContext,
        bind_args: PciInterruptBindArgs,
    ) -> Result<SyscallResult, PciEnableError> {
        if bind_args.count == 0 {
            return Err(PciEnableError::Underlying(PciInvokeError::InvalidParameter));
        }
        if bind_args.slots_len < bind_args.count {
            return Err(PciEnableError::Underlying(PciInvokeError::BufferTooSmall));
        }
        if !RuntimeServices::global().has_pci_runtime() {
            return Err(PciEnableError::Underlying(
                PciInvokeError::RuntimeUnavailable,
            ));
        }

        let cpu_hint =
            (bind_args.cpu_hint != PciInterruptBindArgs::NO_CPU_HINT).then_some(bind_args.cpu_hint);
        let old = self.function.take_active_allocation();
        if let Some(previous) = old {
            let _ = RuntimeServices::global()
                .pci()
                .disable_interrupts(self.function.info.bdf);
            self.function.deactivate_allocation(previous);
        }

        let mut allocation = RuntimeServices::global()
            .pci()
            .allocate_msix(self.function.info.bdf, bind_args.count, cpu_hint)
            .map_err(PciInvokeError::from)
            .map_err(PciEnableError::Underlying)?;
        let slots = match self.install_sessions(caller, &mut allocation) {
            Ok(slots) => slots,
            Err(error) => {
                self.abort_allocation(caller, &[], allocation);
                return Err(PciEnableError::Object(error));
            }
        };
        if let Err(error) = self.write_slots(caller, bind_args.slots_ptr, &slots) {
            self.abort_allocation(caller, &slots, allocation);
            return Err(PciEnableError::Underlying(error));
        }

        self.function.finish_reconfigure(Some(allocation));
        Ok(PciSyscallCodec::enabled(
            slots.len(),
            PciInterruptModeKind::Msix,
        ))
    }
}

#[async_trait]
impl SyscallDispatch<ObjectSyscallContext> for PciFunctionExecuteGuard<'_> {
    async fn dispatch(
        &self,
        caller: &ObjectSyscallContext,
        method_id: usize,
        arg1: usize,
        _arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let Ok(method) = PciFunctionMethod::try_from(method_id) else {
            return Ok(PciSyscallCodec::underlying(
                PciInvokeError::InvalidParameter,
            ));
        };
        match method {
            PciFunctionMethod::EnableMsi => {
                self.require_interrupt_caps()?;
                if let Err(error) = self.function.begin_reconfigure() {
                    return Ok(PciSyscallCodec::underlying(error));
                }
                let args = match self.read_bind_args(caller, arg1) {
                    Ok(args) => args,
                    Err(error) => {
                        self.function.cancel_reconfigure();
                        return Ok(PciSyscallCodec::underlying(error));
                    }
                };
                match self.enable_msi(caller, args) {
                    Ok(frame) => Ok(frame),
                    Err(PciEnableError::Object(error)) => Ok([
                        libakarin_syscall::SYSCALL_STATUS_OBJECT_ERROR,
                        error.abi_code(),
                        0,
                        0,
                        0,
                        0,
                    ]
                    .into()),
                    Err(PciEnableError::Underlying(error)) => {
                        self.function.cancel_reconfigure();
                        Ok(PciSyscallCodec::underlying(error))
                    }
                }
            }
            PciFunctionMethod::EnableMsix => {
                self.require_interrupt_caps()?;
                if let Err(error) = self.function.begin_reconfigure() {
                    return Ok(PciSyscallCodec::underlying(error));
                }
                let args = match self.read_bind_args(caller, arg1) {
                    Ok(args) => args,
                    Err(error) => {
                        self.function.cancel_reconfigure();
                        return Ok(PciSyscallCodec::underlying(error));
                    }
                };
                match self.enable_msix(caller, args) {
                    Ok(frame) => Ok(frame),
                    Err(PciEnableError::Object(error)) => Ok([
                        libakarin_syscall::SYSCALL_STATUS_OBJECT_ERROR,
                        error.abi_code(),
                        0,
                        0,
                        0,
                        0,
                    ]
                    .into()),
                    Err(PciEnableError::Underlying(error)) => {
                        self.function.cancel_reconfigure();
                        Ok(PciSyscallCodec::underlying(error))
                    }
                }
            }
            PciFunctionMethod::DisableInterrupts => {
                self.require_interrupt_caps()?;
                if let Err(error) = self.function.begin_reconfigure() {
                    return Ok(PciSyscallCodec::underlying(error));
                }
                match self.finish_disable() {
                    Ok(frame) => Ok(frame),
                    Err(error) => Ok(PciSyscallCodec::underlying(error)),
                }
            }
            _ => Err(ObjectError::InsufficientCapabilities),
        }
    }
}

impl ControlPlane for PciFunction {
    type ReadGuard<'a>
        = PciFunctionReadGuard<'a>
    where
        Self: 'a;
    type WriteGuard<'a>
        = PciUnsupportedGuard
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = PciFunctionExecuteGuard<'a>
    where
        Self: 'a;
    type AgentGuard<'a>
        = PciUnsupportedGuard
    where
        Self: 'a;
    type AdminGuard<'a>
        = PciUnsupportedGuard
    where
        Self: 'a;

    fn read(&self, interface_caps: u32) -> Self::ReadGuard<'_> {
        PciFunctionReadGuard {
            function: self,
            interface_caps,
        }
    }

    fn write(&self, _interface_caps: u32) -> Self::WriteGuard<'_> {
        PciUnsupportedGuard
    }

    fn execute(&self, interface_caps: u32) -> Self::ExecuteGuard<'_> {
        PciFunctionExecuteGuard {
            function: self,
            interface_caps,
        }
    }

    fn agent(&self, _interface_caps: u32) -> Self::AgentGuard<'_> {
        PciUnsupportedGuard
    }

    fn admin(&self, _interface_caps: u32) -> Self::AdminGuard<'_> {
        PciUnsupportedGuard
    }
}

#[async_trait]
impl SyscallDispatch<ObjectSyscallContext> for PciFunction {}

impl Drop for PciFunction {
    fn drop(&mut self) {
        let mut state = self.interrupts.lock();
        let allocation = state.active.take();
        state.configuring = false;
        drop(state);

        if let Some(allocation) = allocation {
            if RuntimeServices::global().has_pci_runtime() {
                let _ = RuntimeServices::global()
                    .pci()
                    .disable_interrupts(self.info.bdf);
            }
            self.deactivate_allocation(allocation);
        }
    }
}
