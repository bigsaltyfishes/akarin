//! Userspace pager-object definitions.

use alloc::{boxed::Box, sync::Arc};

use hashbrown::HashMap;
use libakarin_collections::intrusive::LinkedList;
use libakarin_core::memory::{VmarMapping, Vmo, VmoPagePurpose};
use libakarin_machine_core::{
    memory::{AddressSpaceTrait, VirtAddr, paging::MMUFlags},
    sync::{NoOp, ScopedGuard},
};
use libakarin_object::{ControlPlane, ObjectError, ObjectSyscallContext, SyscallDispatch};
use libakarin_sync::{
    asynchronous::Event,
    spin::{Once, SpinLock},
};
use libakarin_syscall::{ServiceError, SyscallFailure, SyscallResult, UnderlyingFailure};

use crate::{
    Process,
    arch::{Machine, guards::IrqSaveGuard},
    sched::{Scheduler, task::Task},
    service::{ServiceCall, dispatcher, frame::ServiceFrame},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PagerObjectState {
    Active,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PagerRequestKey {
    vmo_id: u64,
    page_offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PagerRequestState {
    Missing,
    InFlight,
    Ready,
    Failed(ServiceError),
}

struct PagerRequestSlot {
    state: SpinLock<PagerRequestState, IrqSaveGuard>,
    event: Event,
}

impl PagerRequestSlot {
    fn new() -> Self {
        Self {
            state: SpinLock::new(PagerRequestState::Missing),
            event: Event::new(),
        }
    }

    async fn wait_until_ready(&self) -> Result<(), ServiceError> {
        loop {
            let listener = {
                let state = self.state.lock();
                match *state {
                    PagerRequestState::Ready => return Ok(()),
                    PagerRequestState::Failed(error) => return Err(error),
                    PagerRequestState::Missing | PagerRequestState::InFlight => self.event.listen(),
                }
            };

            match *self.state.lock() {
                PagerRequestState::Ready => return Ok(()),
                PagerRequestState::Failed(error) => return Err(error),
                PagerRequestState::Missing | PagerRequestState::InFlight => {}
            }
            listener.await;
        }
    }

    fn publish(&self, state: PagerRequestState) {
        *self.state.lock() = state;
        self.event.notify_all();
    }
}

#[derive(Clone)]
struct PagerBinding {
    pager: PagerObject,
}

/// Kernel-global pager binding and request coordination runtime.
pub struct PagerRuntime {
    bindings: SpinLock<HashMap<u64, PagerBinding>, IrqSaveGuard>,
    requests: SpinLock<HashMap<PagerRequestKey, Arc<PagerRequestSlot>>, IrqSaveGuard>,
}

impl PagerRuntime {
    /// Create one empty pager runtime with no bound paged `VMO`s.
    pub fn new() -> Self {
        Self {
            bindings: SpinLock::new(HashMap::new()),
            requests: SpinLock::new(HashMap::new()),
        }
    }

    /// Bind `vmo` to `pager` and mark it as pager-backed with `backing_cookie`.
    pub fn bind_vmo(
        &self,
        vmo: &Vmo,
        pager: PagerObject,
        backing_cookie: usize,
    ) -> Result<(), ServiceError> {
        if !vmo.bind_pager(backing_cookie) {
            return Err(ServiceError::InvalidArgument);
        }

        let mut bindings = self.bindings.lock();
        if bindings.contains_key(&vmo.id()) {
            return Err(ServiceError::InvalidState);
        }
        bindings.insert(vmo.id(), PagerBinding { pager });
        Ok(())
    }

    /// Resolve one pager-backed fault against `mapping`.
    pub async fn resolve_mapping_fault(
        &self,
        mapping: &VmarMapping,
        addr: VirtAddr,
        access: MMUFlags,
        purpose: VmoPagePurpose,
    ) -> Result<(), ServiceError> {
        let page_size = mapping.vmo.page_size();
        let page_addr = VirtAddr::new(addr.as_usize() & !(page_size - 1));
        let page_offset = mapping
            .vmo_offset_for_addr(page_addr)
            .ok_or(ServiceError::InvalidArgument)?;
        let binding = self
            .bindings
            .lock()
            .get(&mapping.vmo.id())
            .cloned()
            .ok_or(ServiceError::ServiceUnavailable)?;
        let cookie = mapping
            .vmo
            .pager_cookie()
            .ok_or(ServiceError::InvalidState)?;
        let key = PagerRequestKey {
            vmo_id: mapping.vmo.id(),
            page_offset,
        };
        let slot = {
            let mut requests = self.requests.lock();
            Arc::clone(
                requests
                    .entry(key)
                    .or_insert_with(|| Arc::new(PagerRequestSlot::new())),
            )
        };
        {
            let mut state = slot.state.lock();
            match *state {
                PagerRequestState::Ready => return Ok(()),
                PagerRequestState::Failed(error) => return Err(error),
                PagerRequestState::InFlight => return slot.wait_until_ready().await,
                PagerRequestState::Missing => {
                    *state = PagerRequestState::InFlight;
                }
            }
        }

        let result = self
            .fetch_page_from_pager(
                &binding,
                &mapping.vmo,
                addr.as_usize(),
                access.bits() as usize,
                page_offset,
                purpose,
                cookie,
            )
            .await;
        match result {
            Ok(()) => {
                slot.publish(PagerRequestState::Ready);
                Ok(())
            }
            Err(error) => {
                slot.publish(PagerRequestState::Failed(error));
                Err(error)
            }
        }
    }

    async fn fetch_page_from_pager(
        &self,
        binding: &PagerBinding,
        target_vmo: &Vmo,
        fault_addr: usize,
        access_flags: usize,
        page_offset: usize,
        purpose: VmoPagePurpose,
        cookie: usize,
    ) -> Result<(), ServiceError> {
        let reply = binding
            .pager
            .submit_fault(
                fault_addr,
                access_flags,
                cookie,
                page_offset / target_vmo.page_size(),
                0,
            )
            .await?;
        if reply.is_ok() {
            return binding
                .pager
                .install_reply_page(target_vmo, page_offset, purpose, reply);
        }

        match reply.failure() {
            Some(SyscallFailure::Underlying(UnderlyingFailure::Service(error))) => Err(error),
            Some(_) => Err(ServiceError::ProtocolViolation),
            None => Err(ServiceError::InvalidState),
        }
    }
}

/// Scope carried by one pager-register capability object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PagerRegisterScope {
    /// One kernel-global pager registration permit.
    Kernel,
}

/// Special registration object consumed by `PagerCreate`.
pub struct PagerRegister {
    scope: PagerRegisterScope,
}

impl PagerRegister {
    /// Create one new pager register object.
    pub const fn new(scope: PagerRegisterScope) -> Self {
        Self { scope }
    }

    /// Return the scope attached to this register object.
    pub const fn scope(&self) -> PagerRegisterScope {
        self.scope
    }
}

struct PagerObjectInner {
    process: Arc<Process>,
    usr_ip: usize,
    lifecycle: PagerObjectState,
    waiters: LinkedList<Task, libakarin_collections::intrusive::SingleLink>,
    pending_calls: LinkedList<ServiceCall, libakarin_collections::intrusive::SingleLink>,
}

/// Special service object used by future pager-backed `VMO` faults.
#[derive(Clone)]
pub struct PagerObject {
    inner: Arc<SpinLock<PagerObjectInner, IrqSaveGuard>>,
}

impl PagerObject {
    /// Create one pager object bound to `process` and `usr_ip`.
    pub fn new(process: Arc<Process>, usr_ip: usize) -> Self {
        Self {
            inner: Arc::new(SpinLock::new(PagerObjectInner {
                process,
                usr_ip,
                lifecycle: PagerObjectState::Active,
                waiters: LinkedList::new(),
                pending_calls: LinkedList::new(),
            })),
        }
    }

    /// Return the target process that owns this pager object.
    pub fn process(&self) -> Arc<Process> {
        Arc::clone(&self.inner.lock().process)
    }

    /// Return the pager userspace entry point.
    pub fn usr_ip(&self) -> usize {
        self.inner.lock().usr_ip
    }

    /// Bind `vmo` to this pager object for future pager-backed faults.
    pub fn bind_vmo(&self, vmo: &Vmo, backing_cookie: usize) -> Result<(), ServiceError> {
        pager_runtime().bind_vmo(vmo, self.clone(), backing_cookie)
    }

    /// Queue one pager fault request or deliver it immediately to one waiting
    /// pager thread.
    pub async fn submit_fault(
        &self,
        fault_addr: usize,
        access_flags: usize,
        backing_cookie: usize,
        page_offset: usize,
        request_flags: usize,
    ) -> Result<SyscallResult, ServiceError> {
        let frame = ServiceFrame::pager_fault_request(
            fault_addr,
            access_flags,
            backing_cookie,
            page_offset,
            request_flags,
        );
        let Some(caller) = Scheduler::current_task_ref() else {
            return Err(ServiceError::InvalidState);
        };
        let call = ServiceCall::new(caller, frame);
        self.queue_or_deliver_call(Arc::clone(&call))?;
        Ok(call.wait_for_reply().await)
    }

    /// Wait until one queued pager request is delivered to `task`.
    pub async fn wait(&self, task: Arc<Task>) -> Result<(), ServiceError> {
        task.begin_service_wait(crate::service::ServiceWaitKind::Pager)?;
        let pending_call = {
            let mut inner = self.inner.lock();
            if inner.lifecycle != PagerObjectState::Active {
                task.abort_service_wait(crate::service::ServiceWaitKind::Pager);
                return Err(ServiceError::InvalidState);
            }
            match unsafe { inner.pending_calls.pop_front() } {
                Some(call_ptr) => Some(Self::take_call_from_ptr(call_ptr)),
                None => {
                    unsafe {
                        let waiter_ptr = Self::task_ptr(&task);
                        Arc::increment_strong_count(waiter_ptr.cast_const());
                        inner.waiters.push_back(waiter_ptr);
                    }
                    None
                }
            }
        };
        let Some(pending_call) = pending_call else {
            return task
                .wait_for_service_delivery(crate::service::ServiceWaitKind::Pager)
                .await;
        };
        match dispatcher().deliver_to_waiter(&task, pending_call, self.usr_ip()) {
            Ok(()) => Ok(()),
            Err(err) => {
                task.abort_service_wait(crate::service::ServiceWaitKind::Pager);
                Err(err)
            }
        }
    }

    fn queue_or_deliver_call(&self, call: Arc<ServiceCall>) -> Result<(), ServiceError> {
        loop {
            let waiter = {
                let mut inner = self.inner.lock();
                if inner.lifecycle != PagerObjectState::Active {
                    return Err(ServiceError::InvalidState);
                }
                match unsafe { inner.waiters.pop_front() } {
                    Some(waiter_ptr) => Some(Self::take_waiter_from_ptr(waiter_ptr)),
                    None => {
                        unsafe {
                            let call_ptr = Self::call_ptr(&call);
                            Arc::increment_strong_count(call_ptr.cast_const());
                            inner.pending_calls.push_back(call_ptr);
                        }
                        return Ok(());
                    }
                }
            };
            let Some(waiter) = waiter else {
                continue;
            };
            match dispatcher().deliver_to_waiter(&waiter, Arc::clone(&call), self.usr_ip()) {
                Ok(()) => return Ok(()),
                Err(ServiceError::InvalidState) => continue,
                Err(err) => return Err(err),
            }
        }
    }

    fn task_ptr(task: &Arc<Task>) -> *mut Task {
        Arc::as_ptr(task) as *mut Task
    }

    fn take_waiter_from_ptr(task_ptr: *mut Task) -> Arc<Task> {
        unsafe { Arc::from_raw(task_ptr.cast_const()) }
    }

    fn call_ptr(call: &Arc<ServiceCall>) -> *mut ServiceCall {
        Arc::as_ptr(call) as *mut ServiceCall
    }

    fn take_call_from_ptr(call_ptr: *mut ServiceCall) -> Arc<ServiceCall> {
        let raw = call_ptr.cast_const();
        unsafe { Arc::from_raw(raw) }
    }

    fn install_reply_page(
        &self,
        target_vmo: &Vmo,
        page_offset: usize,
        purpose: VmoPagePurpose,
        reply: SyscallResult,
    ) -> Result<(), ServiceError> {
        let source_slot =
            u32::try_from(reply.values[0]).map_err(|_| ServiceError::ProtocolViolation)?;
        let source_offset = reply.values[1];
        let source_vmo = self.resolve_source_vmo(source_slot)?;
        let page_size = target_vmo.page_size();
        if !page_offset.is_multiple_of(page_size)
            || !source_offset.is_multiple_of(page_size)
            || source_vmo.page_size() != page_size
        {
            return Err(ServiceError::ProtocolViolation);
        }

        let source_page = source_vmo
            .page_at(source_offset)
            .ok_or(ServiceError::ProtocolViolation)?;
        let allocator = crate::RuntimeServices::global().frame_allocator();
        target_vmo
            .commit_range::<Machine>(page_offset, page_size, purpose, allocator)
            .map_err(|_| ServiceError::ServiceUnavailable)?;
        let target_page = target_vmo
            .page_at(page_offset)
            .ok_or(ServiceError::ServiceUnavailable)?;
        unsafe {
            Machine::copy_phys(source_page.phys, target_page.phys, page_size);
        }
        Ok(())
    }

    fn resolve_source_vmo(&self, slot: u32) -> Result<Vmo, ServiceError> {
        let handle = self
            .process()
            .acquire_handle(slot)
            .map_err(|_| ServiceError::ProtocolViolation)?;
        handle
            .read_cp_with::<Vmo, _, _>(|vmo| vmo.share())
            .map_err(|_| ServiceError::ProtocolViolation)?
            .map(|vmo| vmo.as_ref().clone())
            .map_err(|_| ServiceError::ProtocolViolation)
    }
}

/// Capability-denied guard for unsupported pager-object control-plane entry.
pub struct PagerUnsupportedGuard;

#[async_trait::async_trait]
impl SyscallDispatch<ObjectSyscallContext> for PagerUnsupportedGuard {
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

/// Admin guard used by special pager-object syscalls.
pub struct PagerObjectAdminGuard<'a> {
    object: &'a PagerObject,
}

impl<'a> PagerObjectAdminGuard<'a> {
    /// Return the wrapped pager object runtime.
    pub fn object(&self) -> &'a PagerObject {
        self.object
    }
}

#[async_trait::async_trait]
impl SyscallDispatch<ObjectSyscallContext> for PagerObjectAdminGuard<'_> {
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

impl ControlPlane for PagerObject {
    type ReadGuard<'a>
        = PagerUnsupportedGuard
    where
        Self: 'a;
    type WriteGuard<'a>
        = PagerUnsupportedGuard
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = PagerUnsupportedGuard
    where
        Self: 'a;
    type AgentGuard<'a>
        = PagerUnsupportedGuard
    where
        Self: 'a;
    type AdminGuard<'a>
        = PagerObjectAdminGuard<'a>
    where
        Self: 'a;

    fn read(&self, _interface_caps: u32) -> Self::ReadGuard<'_> {
        PagerUnsupportedGuard
    }

    fn write(&self, _interface_caps: u32) -> Self::WriteGuard<'_> {
        PagerUnsupportedGuard
    }

    fn execute(&self, _interface_caps: u32) -> Self::ExecuteGuard<'_> {
        PagerUnsupportedGuard
    }

    fn agent(&self, _interface_caps: u32) -> Self::AgentGuard<'_> {
        PagerUnsupportedGuard
    }

    fn admin(&self, _interface_caps: u32) -> Self::AdminGuard<'_> {
        PagerObjectAdminGuard { object: self }
    }
}

/// Admin guard used to validate one consumed pager-register handle.
pub struct PagerRegisterAdminGuard<'a> {
    register: &'a PagerRegister,
}

impl<'a> PagerRegisterAdminGuard<'a> {
    /// Return the wrapped pager-register runtime.
    pub fn register(&self) -> &'a PagerRegister {
        self.register
    }
}

#[async_trait::async_trait]
impl SyscallDispatch<ObjectSyscallContext> for PagerRegisterAdminGuard<'_> {
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

impl ControlPlane for PagerRegister {
    type ReadGuard<'a>
        = PagerUnsupportedGuard
    where
        Self: 'a;
    type WriteGuard<'a>
        = PagerUnsupportedGuard
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = PagerUnsupportedGuard
    where
        Self: 'a;
    type AgentGuard<'a>
        = PagerUnsupportedGuard
    where
        Self: 'a;
    type AdminGuard<'a>
        = PagerRegisterAdminGuard<'a>
    where
        Self: 'a;

    fn read(&self, _interface_caps: u32) -> Self::ReadGuard<'_> {
        PagerUnsupportedGuard
    }

    fn write(&self, _interface_caps: u32) -> Self::WriteGuard<'_> {
        PagerUnsupportedGuard
    }

    fn execute(&self, _interface_caps: u32) -> Self::ExecuteGuard<'_> {
        PagerUnsupportedGuard
    }

    fn agent(&self, _interface_caps: u32) -> Self::AgentGuard<'_> {
        PagerUnsupportedGuard
    }

    fn admin(&self, _interface_caps: u32) -> Self::AdminGuard<'_> {
        PagerRegisterAdminGuard { register: self }
    }
}

/// Return the kernel-global pager runtime.
pub fn pager_runtime() -> &'static PagerRuntime {
    static RUNTIME: Once<PagerRuntime, ScopedGuard<NoOp>> = Once::new();
    RUNTIME.get_or_else(PagerRuntime::new)
}
