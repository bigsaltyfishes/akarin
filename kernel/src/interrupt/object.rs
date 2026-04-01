use alloc::boxed::Box;
use core::{convert::TryFrom, ops::Deref};

use async_trait::async_trait;
use libakarin_machine_core::interrupt::{
    InterruptControllerInfo, InterruptControllerTrait, IrqDelivery, IrqError, IrqLineState,
    IrqLineTrait, IrqSessionState, IrqSessionTrait, IrqWaitFuture,
};
use libakarin_object::{
    Capability, ControlPlane, Handle, ObjectError, ObjectSyscallContext, Payload,
};
use libakarin_syscall::{
    IRQ_CTRL_FEATURES, IRQ_CTRL_QUERY, IRQ_CTRL_TOPOLOGY, IRQ_LINE_OPEN, IRQ_LINE_QUERY,
    IRQ_LINE_ROUTE, IRQ_SESSION_ACK, IRQ_SESSION_ENABLE, IRQ_SESSION_QUERY, IRQ_SESSION_WAIT,
    IrqAckDisposition, IrqControllerMethod, IrqLineMethod, IrqOpenFlags, IrqSessionMethod,
    IrqWaitFlags, SYSCALL_STATUS_OBJECT_ERROR, SYSCALL_STATUS_OK, SyscallDispatch, SyscallFailure,
    SyscallResult, errno::IrqUnderlyingErrorCode,
};

use crate::{
    arch::interrupt::{InterruptController as MachineInterruptController, IrqSession},
    device::manager::KernelDeviceManager,
    interrupt::IrqResourceManager,
};

pub struct IrqSyscallCodec;

impl IrqSyscallCodec {
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

    fn pack_controller_info(info: InterruptControllerInfo) -> SyscallResult {
        Self::ok([info.line_count, info.local_irq_base, info.cpu_count, 0, 0])
    }

    fn pack_controller_topology(info: InterruptControllerInfo) -> SyscallResult {
        Self::ok([info.cpu_count, 0, 0, 0, 0])
    }

    fn pack_controller_features(info: InterruptControllerInfo) -> SyscallResult {
        Self::ok([info.feature_bits, 0, 0, 0, 0])
    }

    fn pack_line_state(state: IrqLineState) -> SyscallResult {
        Self::ok([
            state.irq,
            state.flags.bits(),
            state.session_count,
            state.enabled_session_count,
            state.current_epoch as usize,
        ])
    }

    fn pack_session_state(state: IrqSessionState) -> SyscallResult {
        let state_bits = (state.enabled as usize)
            | ((state.closed as usize) << 1)
            | ((state.inflight_epoch.is_some() as usize) << 2);
        Self::ok([
            state.irq,
            state.session_id,
            state.flags.bits(),
            state_bits,
            state.pending_count,
        ])
    }

    pub fn ok_wait(delivery: IrqDelivery) -> SyscallResult {
        SyscallResult::new(
            SYSCALL_STATUS_OK,
            [
                delivery.epoch as usize,
                delivery.irq,
                delivery.pending_count,
                delivery.flags.bits(),
                0,
            ],
        )
    }

    pub fn ok_ack(pending_count: usize, epoch: u64) -> SyscallResult {
        SyscallResult::new(SYSCALL_STATUS_OK, [pending_count, epoch as usize, 0, 0, 0])
    }

    pub fn object_error(err: ObjectError) -> SyscallResult {
        SyscallResult::new(SYSCALL_STATUS_OBJECT_ERROR, [err.abi_code(), 0, 0, 0, 0])
    }

    pub fn underlying_error(err: IrqError) -> SyscallResult {
        let (code, detail) = Self::map_underlying_error(err);
        let mut words = SyscallResult::from(SyscallFailure::from(code)).to_words();
        words[3] = detail;
        words.into()
    }

    fn underlying_frame(err: IrqError) -> SyscallResult {
        Self::underlying_error(err)
    }

    fn map_underlying_error(err: IrqError) -> (IrqUnderlyingErrorCode, usize) {
        match err {
            IrqError::InvalidIrq(irq) => (IrqUnderlyingErrorCode::InvalidIrq, irq),
            IrqError::InvalidParameter => (IrqUnderlyingErrorCode::InvalidParameter, 0),
            IrqError::OutOfResources => (IrqUnderlyingErrorCode::OutOfResources, 0),
            IrqError::NotSupported => (IrqUnderlyingErrorCode::NotSupported, 0),
            IrqError::WouldBlock => (IrqUnderlyingErrorCode::WouldBlock, 0),
            IrqError::Closed => (IrqUnderlyingErrorCode::Closed, 0),
            IrqError::DeadlineUnsupported => (IrqUnderlyingErrorCode::DeadlineUnsupported, 0),
        }
    }
}

/// Control-plane guard used for modes that one IRQ object does not expose.
pub struct UnsupportedGuard;

#[async_trait]
impl SyscallDispatch<ObjectSyscallContext> for UnsupportedGuard {
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

/// Published wrapper that registers one machine interrupt controller and all
/// discoverable IRQ line resources into the kernel namespace hierarchy.
pub struct PublishedInterruptController<C: InterruptControllerTrait + 'static> {
    controller: &'static C,
}

impl<C: InterruptControllerTrait + 'static> PublishedInterruptController<C> {
    /// Create one published wrapper around a machine interrupt controller.
    pub fn new(controller: &'static C) -> Self {
        Self { controller }
    }

    /// Publish the controller device object and all discoverable line
    /// resources.
    pub fn publish(
        &self,
        device_manager: &KernelDeviceManager,
        irq_manager: &IrqResourceManager,
    ) -> Result<Handle, ObjectError>
    where
        C::Line: Send + Sync + 'static,
        C::Session: Send + Sync + 'static,
    {
        let device_owner = device_manager.register_driver(
            self.controller.controller_name(),
            InterruptControllerDevice::new(self.controller),
        )?;
        let info = self.controller.controller_info();
        for irq in 0..info.line_count {
            if !self.controller.is_valid_irq(irq) {
                continue;
            }
            let line = self.controller.line(irq).map_err(Self::map_object_error)?;
            let irq_owner = irq_manager.register_irq(irq, IrqLineResource::new(line))?;
            unsafe { irq_owner.forget() };
        }
        Ok(device_owner)
    }

    fn map_object_error(err: IrqError) -> ObjectError {
        match err {
            IrqError::InvalidIrq(_)
            | IrqError::InvalidParameter
            | IrqError::NotSupported
            | IrqError::WouldBlock
            | IrqError::DeadlineUnsupported => ObjectError::InvalidArgument,
            IrqError::OutOfResources | IrqError::Closed => ObjectError::ObjectDestroyed,
        }
    }
}

/// Stable device object for one machine interrupt controller.
pub struct InterruptControllerDevice<C: InterruptControllerTrait + 'static> {
    controller: &'static C,
}

impl<C: InterruptControllerTrait> InterruptControllerDevice<C> {
    fn new(controller: &'static C) -> Self {
        Self { controller }
    }

    fn query_info_words(&self) -> SyscallResult {
        IrqSyscallCodec::pack_controller_info(self.controller.controller_info())
    }

    fn query_topology_words(&self) -> SyscallResult {
        IrqSyscallCodec::pack_controller_topology(self.controller.controller_info())
    }

    fn query_features_words(&self) -> SyscallResult {
        IrqSyscallCodec::pack_controller_features(self.controller.controller_info())
    }
}

/// Read-side guard for one interrupt controller device object.
pub struct InterruptControllerReadGuard<'a, C: InterruptControllerTrait + 'static> {
    device: &'a InterruptControllerDevice<C>,
    interface_caps: u32,
}

impl<C: InterruptControllerTrait> InterruptControllerReadGuard<'_, C> {
    fn require_caps(&self, caps: u32) -> Result<(), ObjectError> {
        if self.interface_caps == u32::MAX || (self.interface_caps & caps) == caps {
            Ok(())
        } else {
            Err(ObjectError::InsufficientCapabilities)
        }
    }
}

impl<C: InterruptControllerTrait> Deref for InterruptControllerReadGuard<'_, C> {
    type Target = InterruptControllerDevice<C>;

    fn deref(&self) -> &Self::Target {
        self.device
    }
}

#[async_trait]
impl<C: InterruptControllerTrait> SyscallDispatch<ObjectSyscallContext>
    for InterruptControllerReadGuard<'_, C>
{
    async fn dispatch(
        &self,
        _caller: &ObjectSyscallContext,
        method_id: usize,
        _arg1: usize,
        _arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let Ok(method) = IrqControllerMethod::try_from(method_id) else {
            return Ok(IrqSyscallCodec::underlying_frame(
                IrqError::InvalidParameter,
            ));
        };
        match method {
            IrqControllerMethod::QueryInfo => {
                self.require_caps(IRQ_CTRL_QUERY)?;
                Ok(self.query_info_words())
            }
            IrqControllerMethod::QueryTopology => {
                self.require_caps(IRQ_CTRL_TOPOLOGY)?;
                Ok(self.query_topology_words())
            }
            IrqControllerMethod::QueryFeatures => {
                self.require_caps(IRQ_CTRL_FEATURES)?;
                Ok(self.query_features_words())
            }
        }
    }
}

/// Admin-side guard for one interrupt controller device object.
pub struct InterruptControllerAdminGuard<'a, C: InterruptControllerTrait + 'static> {
    device: &'a InterruptControllerDevice<C>,
    interface_caps: u32,
}

impl<C: InterruptControllerTrait> Deref for InterruptControllerAdminGuard<'_, C> {
    type Target = InterruptControllerDevice<C>;

    fn deref(&self) -> &Self::Target {
        self.device
    }
}

#[async_trait]
impl<C: InterruptControllerTrait> SyscallDispatch<ObjectSyscallContext>
    for InterruptControllerAdminGuard<'_, C>
{
    async fn dispatch(
        &self,
        caller: &ObjectSyscallContext,
        method_id: usize,
        arg1: usize,
        arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        InterruptControllerReadGuard {
            device: self.device,
            interface_caps: self.interface_caps,
        }
        .dispatch(caller, method_id, arg1, arg2)
        .await
    }
}

impl<C: InterruptControllerTrait + 'static> ControlPlane for InterruptControllerDevice<C> {
    type ReadGuard<'a>
        = InterruptControllerReadGuard<'a, C>
    where
        C: 'a;
    type WriteGuard<'a>
        = UnsupportedGuard
    where
        C: 'a;
    type ExecuteGuard<'a>
        = UnsupportedGuard
    where
        C: 'a;
    type AgentGuard<'a>
        = UnsupportedGuard
    where
        C: 'a;
    type AdminGuard<'a>
        = InterruptControllerAdminGuard<'a, C>
    where
        C: 'a;

    fn read(&self, interface_caps: u32) -> Self::ReadGuard<'_> {
        InterruptControllerReadGuard {
            device: self,
            interface_caps,
        }
    }

    fn write(&self, _interface_caps: u32) -> Self::WriteGuard<'_> {
        UnsupportedGuard
    }

    fn execute(&self, _interface_caps: u32) -> Self::ExecuteGuard<'_> {
        UnsupportedGuard
    }

    fn agent(&self, _interface_caps: u32) -> Self::AgentGuard<'_> {
        UnsupportedGuard
    }

    fn admin(&self, interface_caps: u32) -> Self::AdminGuard<'_> {
        InterruptControllerAdminGuard {
            device: self,
            interface_caps,
        }
    }
}

/// Discoverable IRQ line resource object under `/Resource/IRQ/{irq}`.
pub struct IrqLineResource<L: IrqLineTrait + 'static> {
    line: L,
}

impl<L: IrqLineTrait + 'static> IrqLineResource<L> {
    fn new(line: L) -> Self {
        Self { line }
    }

    fn query_state_words(&self) -> Result<SyscallResult, IrqError> {
        self.line.state().map(IrqSyscallCodec::pack_line_state)
    }

    fn open_session_object(
        &self,
        flags: IrqOpenFlags,
    ) -> Result<SharedIrqSessionObject<L::Session>, IrqError> {
        self.line
            .open_session(flags)
            .map(SharedIrqSessionObject::new)
    }

    fn set_destination(&self, cpu_id: usize) -> Result<(), IrqError> {
        self.line.set_destination(cpu_id)
    }
}

/// Read-side guard for one IRQ line resource object.
pub struct IrqLineReadGuard<'a, L: IrqLineTrait + 'static> {
    line: &'a IrqLineResource<L>,
    interface_caps: u32,
}

impl<L: IrqLineTrait> IrqLineReadGuard<'_, L> {
    fn require_caps(&self, caps: u32) -> Result<(), ObjectError> {
        if self.interface_caps == u32::MAX || (self.interface_caps & caps) == caps {
            Ok(())
        } else {
            Err(ObjectError::InsufficientCapabilities)
        }
    }
}

impl<L: IrqLineTrait> Deref for IrqLineReadGuard<'_, L> {
    type Target = IrqLineResource<L>;

    fn deref(&self) -> &Self::Target {
        self.line
    }
}

#[async_trait]
impl<L> SyscallDispatch<ObjectSyscallContext> for IrqLineReadGuard<'_, L>
where
    L: IrqLineTrait + Send + Sync,
    L::Session: Send + Sync + 'static,
{
    async fn dispatch(
        &self,
        _caller: &ObjectSyscallContext,
        method_id: usize,
        _arg1: usize,
        _arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let Ok(method) = IrqLineMethod::try_from(method_id) else {
            return Ok(IrqSyscallCodec::underlying_frame(
                IrqError::InvalidParameter,
            ));
        };
        match method {
            IrqLineMethod::QueryState => {
                self.require_caps(IRQ_LINE_QUERY)?;
                Ok(match self.query_state_words() {
                    Ok(frame) => frame,
                    Err(err) => IrqSyscallCodec::underlying_frame(err),
                })
            }
            _ => Err(ObjectError::InsufficientCapabilities),
        }
    }
}

/// Agent-side guard used to open shared IRQ sessions on one line.
pub struct IrqLineAgentGuard<'a, L: IrqLineTrait + 'static> {
    line: &'a IrqLineResource<L>,
    interface_caps: u32,
}

impl<L: IrqLineTrait> IrqLineAgentGuard<'_, L> {
    fn require_caps(&self, caps: u32) -> Result<(), ObjectError> {
        if self.interface_caps == u32::MAX || (self.interface_caps & caps) == caps {
            Ok(())
        } else {
            Err(ObjectError::InsufficientCapabilities)
        }
    }
}

impl<L: IrqLineTrait> Deref for IrqLineAgentGuard<'_, L> {
    type Target = IrqLineResource<L>;

    fn deref(&self) -> &Self::Target {
        self.line
    }
}

#[async_trait]
impl<L> SyscallDispatch<ObjectSyscallContext> for IrqLineAgentGuard<'_, L>
where
    L: IrqLineTrait + Send + Sync,
    L::Session: Send + Sync + 'static,
{
    async fn dispatch(
        &self,
        caller: &ObjectSyscallContext,
        method_id: usize,
        arg1: usize,
        _arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let Ok(method) = IrqLineMethod::try_from(method_id) else {
            return Ok(IrqSyscallCodec::underlying_frame(
                IrqError::InvalidParameter,
            ));
        };
        match method {
            IrqLineMethod::OpenSession => {
                self.require_caps(IRQ_LINE_OPEN)?;
                let Some(mut flags) = IrqOpenFlags::from_bits(arg1) else {
                    return Ok(IrqSyscallCodec::underlying_frame(
                        IrqError::InvalidParameter,
                    ));
                };
                if flags.bits() == 0 {
                    flags |= IrqOpenFlags::SHARED;
                }
                let session = match self.open_session_object(flags) {
                    Ok(session) => session,
                    Err(err) => return Ok(IrqSyscallCodec::underlying_frame(err)),
                };
                let slot = caller.create_anonymous_object(
                    Payload::new(session),
                    Capability::SEND | Capability::READ | Capability::WRITE | Capability::EXECUTE,
                    IRQ_SESSION_QUERY | IRQ_SESSION_WAIT | IRQ_SESSION_ACK,
                )?;
                Ok(IrqSyscallCodec::ok([slot as usize, 0, 0, 0, 0]))
            }
            _ => Err(ObjectError::InsufficientCapabilities),
        }
    }
}

/// Admin-side guard for one IRQ line resource object.
pub struct IrqLineAdminGuard<'a, L: IrqLineTrait + 'static> {
    line: &'a IrqLineResource<L>,
    interface_caps: u32,
}

impl<L: IrqLineTrait> IrqLineAdminGuard<'_, L> {
    fn require_caps(&self, caps: u32) -> Result<(), ObjectError> {
        if self.interface_caps == u32::MAX || (self.interface_caps & caps) == caps {
            Ok(())
        } else {
            Err(ObjectError::InsufficientCapabilities)
        }
    }
}

impl<L: IrqLineTrait> Deref for IrqLineAdminGuard<'_, L> {
    type Target = IrqLineResource<L>;

    fn deref(&self) -> &Self::Target {
        self.line
    }
}

#[async_trait]
impl<L> SyscallDispatch<ObjectSyscallContext> for IrqLineAdminGuard<'_, L>
where
    L: IrqLineTrait + Send + Sync,
    L::Session: Send + Sync + 'static,
{
    async fn dispatch(
        &self,
        caller: &ObjectSyscallContext,
        method_id: usize,
        arg1: usize,
        arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let Ok(method) = IrqLineMethod::try_from(method_id) else {
            return Ok(IrqSyscallCodec::underlying_frame(
                IrqError::InvalidParameter,
            ));
        };
        match method {
            IrqLineMethod::QueryState => {
                self.require_caps(IRQ_LINE_QUERY)?;
                Ok(match self.query_state_words() {
                    Ok(frame) => frame,
                    Err(err) => IrqSyscallCodec::underlying_frame(err),
                })
            }
            IrqLineMethod::OpenSession => {
                self.require_caps(IRQ_LINE_OPEN)?;
                IrqLineAgentGuard {
                    line: self.line,
                    interface_caps: self.interface_caps,
                }
                .dispatch(caller, method_id, arg1, arg2)
                .await
            }
            IrqLineMethod::SetDestination => {
                self.require_caps(IRQ_LINE_ROUTE)?;
                if let Err(err) = self.set_destination(arg1) {
                    return Ok(IrqSyscallCodec::underlying_frame(err));
                }
                Ok(IrqSyscallCodec::ok([0, 0, 0, 0, 0]))
            }
        }
    }
}

impl<L> ControlPlane for IrqLineResource<L>
where
    L: IrqLineTrait + Send + Sync + 'static,
    L::Session: Send + Sync + 'static,
{
    type ReadGuard<'a>
        = IrqLineReadGuard<'a, L>
    where
        L: 'a;
    type WriteGuard<'a>
        = UnsupportedGuard
    where
        L: 'a;
    type ExecuteGuard<'a>
        = UnsupportedGuard
    where
        L: 'a;
    type AgentGuard<'a>
        = IrqLineAgentGuard<'a, L>
    where
        L: 'a;
    type AdminGuard<'a>
        = IrqLineAdminGuard<'a, L>
    where
        L: 'a;

    fn read(&self, interface_caps: u32) -> Self::ReadGuard<'_> {
        IrqLineReadGuard {
            line: self,
            interface_caps,
        }
    }

    fn write(&self, _interface_caps: u32) -> Self::WriteGuard<'_> {
        UnsupportedGuard
    }

    fn execute(&self, _interface_caps: u32) -> Self::ExecuteGuard<'_> {
        UnsupportedGuard
    }

    fn agent(&self, interface_caps: u32) -> Self::AgentGuard<'_> {
        IrqLineAgentGuard {
            line: self,
            interface_caps,
        }
    }

    fn admin(&self, interface_caps: u32) -> Self::AdminGuard<'_> {
        IrqLineAdminGuard {
            line: self,
            interface_caps,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IrqSessionGuardMode {
    Read,
    Write,
    Execute,
    Agent,
    Admin,
}

impl IrqSessionGuardMode {
    fn allows_method(self, method: IrqSessionMethod) -> bool {
        match self {
            Self::Admin => true,
            Self::Read => matches!(method, IrqSessionMethod::QueryState),
            Self::Write => false,
            Self::Execute => false,
            Self::Agent => false,
        }
    }

    fn allows_wait(self) -> bool {
        matches!(self, Self::Execute | Self::Admin)
    }

    fn allows_ack(self) -> bool {
        matches!(self, Self::Write | Self::Admin)
    }
}

/// Generic user-visible IRQ session object backed by one machine session.
pub struct SharedIrqSessionObject<S: IrqSessionTrait> {
    session: S,
}

impl<S: IrqSessionTrait> SharedIrqSessionObject<S> {
    pub fn new(session: S) -> Self {
        Self { session }
    }

    fn query_state_words(&self) -> SyscallResult {
        IrqSyscallCodec::pack_session_state(self.session.state())
    }

    pub fn wait_future(&self, deadline: usize, flags: IrqWaitFlags) -> IrqWaitFuture {
        self.session.wait(deadline, flags)
    }

    pub fn ack_epoch(&self, epoch: u64, disposition: IrqAckDisposition) -> Result<usize, IrqError> {
        self.session.ack(epoch, disposition)
    }

    fn set_enabled(&self, enabled: bool) {
        self.session.set_enabled(enabled);
    }

    fn close(&self) {
        self.session.close();
    }
}

impl<S: IrqSessionTrait> Drop for SharedIrqSessionObject<S> {
    fn drop(&mut self) {
        self.session.close();
    }
}

/// Mode-constrained guard for one shared IRQ session object.
pub struct IrqSessionGuard<'a, S: IrqSessionTrait + 'static> {
    session: &'a SharedIrqSessionObject<S>,
    interface_caps: u32,
    mode: IrqSessionGuardMode,
}

impl<S: IrqSessionTrait> IrqSessionGuard<'_, S> {
    fn require_caps(&self, caps: u32) -> Result<(), ObjectError> {
        if self.interface_caps == u32::MAX || (self.interface_caps & caps) == caps {
            Ok(())
        } else {
            Err(ObjectError::InsufficientCapabilities)
        }
    }

    fn require_method(&self, method: IrqSessionMethod, caps: u32) -> Result<(), ObjectError> {
        if !self.mode.allows_method(method) {
            return Err(ObjectError::InsufficientCapabilities);
        }
        self.require_caps(caps)
    }

    fn require_wait(&self) -> Result<(), ObjectError> {
        if !self.mode.allows_wait() {
            return Err(ObjectError::InsufficientCapabilities);
        }
        self.require_caps(IRQ_SESSION_WAIT)
    }

    fn require_ack(&self) -> Result<(), ObjectError> {
        if !self.mode.allows_ack() {
            return Err(ObjectError::InsufficientCapabilities);
        }
        self.require_caps(IRQ_SESSION_ACK)
    }

    pub fn wait_future(
        &self,
        deadline: usize,
        flags: IrqWaitFlags,
    ) -> Result<IrqWaitFuture, ObjectError> {
        self.require_wait()?;
        Ok(self.session.wait_future(deadline, flags))
    }

    pub fn ack_epoch(
        &self,
        epoch: u64,
        disposition: IrqAckDisposition,
    ) -> Result<Result<usize, IrqError>, ObjectError> {
        self.require_ack()?;
        Ok(self.session.ack_epoch(epoch, disposition))
    }
}

impl<S: IrqSessionTrait> Deref for IrqSessionGuard<'_, S> {
    type Target = SharedIrqSessionObject<S>;

    fn deref(&self) -> &Self::Target {
        self.session
    }
}

#[async_trait]
impl<S> SyscallDispatch<ObjectSyscallContext> for IrqSessionGuard<'_, S>
where
    S: IrqSessionTrait + Send + Sync,
{
    async fn dispatch(
        &self,
        _caller: &ObjectSyscallContext,
        method_id: usize,
        arg1: usize,
        _arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let Ok(method) = IrqSessionMethod::try_from(method_id) else {
            return Ok(IrqSyscallCodec::underlying_frame(
                IrqError::InvalidParameter,
            ));
        };
        match method {
            IrqSessionMethod::QueryState => {
                self.require_method(IrqSessionMethod::QueryState, IRQ_SESSION_QUERY)?;
                Ok(self.query_state_words())
            }
            IrqSessionMethod::SetEnabled => {
                self.require_method(IrqSessionMethod::SetEnabled, IRQ_SESSION_ENABLE)?;
                self.set_enabled(arg1 != 0);
                Ok(IrqSyscallCodec::ok([0, 0, 0, 0, 0]))
            }
            IrqSessionMethod::Close => {
                self.require_method(IrqSessionMethod::Close, IRQ_SESSION_ENABLE)?;
                self.close();
                Ok(IrqSyscallCodec::ok([0, 0, 0, 0, 0]))
            }
        }
    }
}

impl<S> ControlPlane for SharedIrqSessionObject<S>
where
    S: IrqSessionTrait + Send + Sync + 'static,
{
    type ReadGuard<'a>
        = IrqSessionGuard<'a, S>
    where
        S: 'a;
    type WriteGuard<'a>
        = IrqSessionGuard<'a, S>
    where
        S: 'a;
    type ExecuteGuard<'a>
        = IrqSessionGuard<'a, S>
    where
        S: 'a;
    type AgentGuard<'a>
        = IrqSessionGuard<'a, S>
    where
        S: 'a;
    type AdminGuard<'a>
        = IrqSessionGuard<'a, S>
    where
        S: 'a;

    fn read(&self, interface_caps: u32) -> Self::ReadGuard<'_> {
        IrqSessionGuard {
            session: self,
            interface_caps,
            mode: IrqSessionGuardMode::Read,
        }
    }

    fn write(&self, interface_caps: u32) -> Self::WriteGuard<'_> {
        IrqSessionGuard {
            session: self,
            interface_caps,
            mode: IrqSessionGuardMode::Write,
        }
    }

    fn execute(&self, interface_caps: u32) -> Self::ExecuteGuard<'_> {
        IrqSessionGuard {
            session: self,
            interface_caps,
            mode: IrqSessionGuardMode::Execute,
        }
    }

    fn agent(&self, interface_caps: u32) -> Self::AgentGuard<'_> {
        IrqSessionGuard {
            session: self,
            interface_caps,
            mode: IrqSessionGuardMode::Agent,
        }
    }

    fn admin(&self, interface_caps: u32) -> Self::AdminGuard<'_> {
        IrqSessionGuard {
            session: self,
            interface_caps,
            mode: IrqSessionGuardMode::Admin,
        }
    }
}

/// Concrete published-controller type for the active machine.
pub type InterruptController = PublishedInterruptController<MachineInterruptController>;

/// Concrete shared IRQ session object type for the active machine.
pub type IrqSessionObject = SharedIrqSessionObject<IrqSession>;
