//! Userspace syscall-handler object definitions.

use alloc::{boxed::Box, sync::Arc};

use hashbrown::HashMap;
use libakarin_collections::intrusive::LinkedList;
use libakarin_machine_core::sync::{NoOp, ScopedGuard};
use libakarin_object::{ControlPlane, ObjectError, ObjectSyscallContext, SyscallDispatch};
use libakarin_sync::spin::{Once, SpinLock};
use libakarin_syscall::{ServiceError, Syscall, SyscallArgs, SyscallResult};

use crate::{
    Process,
    arch::guards::IrqSaveGuard,
    sched::{Scheduler, task::Task},
    service::{ServiceCall, dispatcher, frame::ServiceFrame},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyscallHandlerObjectState {
    Active,
    Closed,
}

/// Scope carried by one syscall-handler register capability object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyscallRegisterScope {
    /// Allow registration of one kernel-global forwarded syscall handler.
    Kernel,
}

/// Special registration object consumed by `SyscallHandlerCreate`.
pub struct SyscallRegister {
    scope: SyscallRegisterScope,
}

impl SyscallRegister {
    /// Create one new syscall-handler register object.
    pub const fn new(scope: SyscallRegisterScope) -> Self {
        Self { scope }
    }

    /// Return the registration scope attached to this object.
    pub const fn scope(&self) -> SyscallRegisterScope {
        self.scope
    }
}

struct SyscallHandlerObjectInner {
    process: Arc<Process>,
    usr_ip: usize,
    syscall_nr: Syscall,
    lifecycle: SyscallHandlerObjectState,
    waiters: LinkedList<Task, libakarin_collections::intrusive::SingleLink>,
    pending_calls: LinkedList<ServiceCall, libakarin_collections::intrusive::SingleLink>,
}

/// Special service object used to forward selected syscalls into userspace.
#[derive(Clone)]
pub struct SyscallHandlerObject {
    inner: Arc<SpinLock<SyscallHandlerObjectInner, IrqSaveGuard>>,
}

impl SyscallHandlerObject {
    /// Create one syscall-handler object bound to `process`, `usr_ip`, and one
    /// forwarded syscall number.
    pub fn new(process: Arc<Process>, usr_ip: usize, syscall_nr: Syscall) -> Self {
        Self {
            inner: Arc::new(SpinLock::new(SyscallHandlerObjectInner {
                process,
                usr_ip,
                syscall_nr,
                lifecycle: SyscallHandlerObjectState::Active,
                waiters: LinkedList::new(),
                pending_calls: LinkedList::new(),
            })),
        }
    }

    /// Return the target process that owns this handler object.
    pub fn process(&self) -> Arc<Process> {
        Arc::clone(&self.inner.lock().process)
    }

    /// Return the userspace handler entry point.
    pub fn usr_ip(&self) -> usize {
        self.inner.lock().usr_ip
    }

    /// Return the syscall number forwarded into this handler.
    pub fn syscall_nr(&self) -> Syscall {
        self.inner.lock().syscall_nr
    }

    /// Queue one forwarded syscall request or deliver it immediately to one
    /// waiting handler thread.
    pub async fn submit_request(&self, args: SyscallArgs) -> Result<SyscallResult, ServiceError> {
        let frame = ServiceFrame::syscall_request(args.method_id, *args.args());
        let Some(caller) = Scheduler::current_task_ref() else {
            return Err(ServiceError::InvalidState);
        };
        let call = ServiceCall::new(caller, frame);
        self.queue_or_deliver_call(Arc::clone(&call))?;
        Ok(call.wait_for_reply().await)
    }

    /// Wait until one queued forwarded syscall request is delivered to `task`.
    pub async fn wait(&self, task: Arc<Task>) -> Result<(), ServiceError> {
        task.begin_service_wait(crate::service::ServiceWaitKind::SyscallHandler)?;
        let pending_call = {
            let mut inner = self.inner.lock();
            if inner.lifecycle != SyscallHandlerObjectState::Active {
                task.abort_service_wait(crate::service::ServiceWaitKind::SyscallHandler);
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
                .wait_for_service_delivery(crate::service::ServiceWaitKind::SyscallHandler)
                .await;
        };
        match dispatcher().deliver_to_waiter(&task, pending_call, self.usr_ip()) {
            Ok(()) => Ok(()),
            Err(err) => {
                task.abort_service_wait(crate::service::ServiceWaitKind::SyscallHandler);
                Err(err)
            }
        }
    }

    fn queue_or_deliver_call(&self, call: Arc<ServiceCall>) -> Result<(), ServiceError> {
        loop {
            let waiter = {
                let mut inner = self.inner.lock();
                if inner.lifecycle != SyscallHandlerObjectState::Active {
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
                // One queued waiter may have been cancelled or reassigned
                // before this request reached it.
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
}

/// Kernel-global forwarded-syscall registry.
pub struct SyscallHandlerRuntime {
    handlers: SpinLock<HashMap<usize, SyscallHandlerObject>, IrqSaveGuard>,
}

impl SyscallHandlerRuntime {
    /// Create one empty runtime with no registered forwarded syscalls.
    pub fn new() -> Self {
        Self {
            handlers: SpinLock::new(HashMap::new()),
        }
    }

    /// Register `handler` as the unique userspace handler for its syscall
    /// number.
    pub fn bind_handler(&self, handler: SyscallHandlerObject) -> Result<(), ServiceError> {
        let syscall_nr = handler.syscall_nr();
        if !self.is_forwardable_syscall(syscall_nr) {
            return Err(ServiceError::InvalidArgument);
        }

        let mut handlers = self.handlers.lock();
        if handlers.contains_key(&(syscall_nr as usize)) {
            return Err(ServiceError::InvalidState);
        }
        handlers.insert(syscall_nr as usize, handler);
        Ok(())
    }

    /// Return the currently registered handler for `syscall`, if one exists.
    pub fn handler_for(&self, syscall: Syscall) -> Option<SyscallHandlerObject> {
        self.handlers.lock().get(&(syscall as usize)).cloned()
    }

    fn is_forwardable_syscall(&self, syscall: Syscall) -> bool {
        !matches!(
            syscall,
            Syscall::UserObjectCreate
                | Syscall::WaitObjectRequest
                | Syscall::ServiceReplyOk
                | Syscall::ServiceReplyObjectError
                | Syscall::ServiceReplyUnderlying
                | Syscall::PagerCreate
                | Syscall::WaitPagerRequest
                | Syscall::SyscallHandlerCreate
                | Syscall::WaitSyscallRequest
        )
    }
}

/// Guard returned for unsupported control-plane access modes on special
/// syscall-handler objects and registers.
pub struct SyscallUnsupportedGuard;

#[async_trait::async_trait]
impl SyscallDispatch<ObjectSyscallContext> for SyscallUnsupportedGuard {
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

/// Admin guard used by special syscall-handler object syscalls.
pub struct SyscallHandlerObjectAdminGuard<'a> {
    object: &'a SyscallHandlerObject,
}

impl<'a> SyscallHandlerObjectAdminGuard<'a> {
    /// Return the wrapped syscall-handler object runtime.
    pub fn object(&self) -> &'a SyscallHandlerObject {
        self.object
    }
}

#[async_trait::async_trait]
impl SyscallDispatch<ObjectSyscallContext> for SyscallHandlerObjectAdminGuard<'_> {
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

impl ControlPlane for SyscallHandlerObject {
    type ReadGuard<'a>
        = SyscallUnsupportedGuard
    where
        Self: 'a;
    type WriteGuard<'a>
        = SyscallUnsupportedGuard
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = SyscallUnsupportedGuard
    where
        Self: 'a;
    type AgentGuard<'a>
        = SyscallUnsupportedGuard
    where
        Self: 'a;
    type AdminGuard<'a>
        = SyscallHandlerObjectAdminGuard<'a>
    where
        Self: 'a;

    fn read(&self, _interface_caps: u32) -> Self::ReadGuard<'_> {
        SyscallUnsupportedGuard
    }

    fn write(&self, _interface_caps: u32) -> Self::WriteGuard<'_> {
        SyscallUnsupportedGuard
    }

    fn execute(&self, _interface_caps: u32) -> Self::ExecuteGuard<'_> {
        SyscallUnsupportedGuard
    }

    fn agent(&self, _interface_caps: u32) -> Self::AgentGuard<'_> {
        SyscallUnsupportedGuard
    }

    fn admin(&self, _interface_caps: u32) -> Self::AdminGuard<'_> {
        SyscallHandlerObjectAdminGuard { object: self }
    }
}

/// Admin guard used to validate one consumed syscall-handler register.
pub struct SyscallRegisterAdminGuard<'a> {
    register: &'a SyscallRegister,
}

impl<'a> SyscallRegisterAdminGuard<'a> {
    /// Return the wrapped syscall-handler register runtime.
    pub fn register(&self) -> &'a SyscallRegister {
        self.register
    }
}

#[async_trait::async_trait]
impl SyscallDispatch<ObjectSyscallContext> for SyscallRegisterAdminGuard<'_> {
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

impl ControlPlane for SyscallRegister {
    type ReadGuard<'a>
        = SyscallUnsupportedGuard
    where
        Self: 'a;
    type WriteGuard<'a>
        = SyscallUnsupportedGuard
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = SyscallUnsupportedGuard
    where
        Self: 'a;
    type AgentGuard<'a>
        = SyscallUnsupportedGuard
    where
        Self: 'a;
    type AdminGuard<'a>
        = SyscallRegisterAdminGuard<'a>
    where
        Self: 'a;

    fn read(&self, _interface_caps: u32) -> Self::ReadGuard<'_> {
        SyscallUnsupportedGuard
    }

    fn write(&self, _interface_caps: u32) -> Self::WriteGuard<'_> {
        SyscallUnsupportedGuard
    }

    fn execute(&self, _interface_caps: u32) -> Self::ExecuteGuard<'_> {
        SyscallUnsupportedGuard
    }

    fn agent(&self, _interface_caps: u32) -> Self::AgentGuard<'_> {
        SyscallUnsupportedGuard
    }

    fn admin(&self, _interface_caps: u32) -> Self::AdminGuard<'_> {
        SyscallRegisterAdminGuard { register: self }
    }
}

/// Return the kernel-global forwarded-syscall runtime.
pub fn syscall_handler_runtime() -> &'static SyscallHandlerRuntime {
    static RUNTIME: Once<SyscallHandlerRuntime, ScopedGuard<NoOp>> = Once::new();
    RUNTIME.get_or_else(SyscallHandlerRuntime::new)
}
