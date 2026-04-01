//! General userspace service objects.

use alloc::{boxed::Box, sync::Arc};

use libakarin_collections::intrusive::LinkedList;
use libakarin_object::{
    ControlPlane, CpAccessMode, ObjectError, ObjectSyscallContext, SyscallDispatch,
};
use libakarin_sync::spin::SpinLock;
use libakarin_syscall::{ServiceError, SyscallResult};

use crate::{
    Process,
    arch::guards::IrqSaveGuard,
    sched::{Scheduler, task::Task},
    service::{ServiceCall, dispatcher, frame::ServiceFrame},
    syscall::IntoSyscallResult,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserObjectState {
    Active,
    Closed,
}

struct UserObjectInner {
    process: Arc<Process>,
    usr_ip: usize,
    lifecycle: UserObjectState,
    waiters: LinkedList<Task, libakarin_collections::intrusive::SingleLink>,
    pending_calls: LinkedList<ServiceCall, libakarin_collections::intrusive::SingleLink>,
}

/// One general userspace service object reached through `ObjectInvoke`.
#[derive(Clone)]
pub struct UserObject {
    inner: Arc<SpinLock<UserObjectInner, IrqSaveGuard>>,
}

impl UserObject {
    /// Create one userspace service object bound to `process` and `usr_ip`.
    pub fn new(process: Arc<Process>, usr_ip: usize) -> Self {
        Self {
            inner: Arc::new(SpinLock::new(UserObjectInner {
                process,
                usr_ip,
                lifecycle: UserObjectState::Active,
                waiters: LinkedList::new(),
                pending_calls: LinkedList::new(),
            })),
        }
    }

    /// Return the bound target process.
    pub fn process(&self) -> Arc<Process> {
        Arc::clone(&self.inner.lock().process)
    }

    /// Return the bound userspace service entry point.
    pub fn usr_ip(&self) -> usize {
        self.inner.lock().usr_ip
    }

    /// Queue one object request or deliver it immediately to one waiting
    /// thread.
    pub async fn invoke(
        &self,
        mode: CpAccessMode,
        interface_caps: u32,
        method_id: usize,
        arg0: usize,
        arg1: usize,
    ) -> Result<SyscallResult, ServiceError> {
        let frame = ServiceFrame::object_request(mode, interface_caps, method_id, arg0, arg1);
        let Some(caller) = Scheduler::current_task_ref() else {
            return Err(ServiceError::InvalidState);
        };
        let call = ServiceCall::new(caller, frame);
        self.queue_or_deliver_call(Arc::clone(&call))?;
        Ok(call.wait_for_reply().await)
    }

    /// Wait until one queued object request is delivered to `task`.
    pub async fn wait(&self, task: Arc<Task>) -> Result<(), ServiceError> {
        task.begin_service_wait(crate::service::ServiceWaitKind::Object)?;
        let pending_call = {
            let mut inner = self.inner.lock();
            if inner.lifecycle != UserObjectState::Active {
                task.abort_service_wait(crate::service::ServiceWaitKind::Object);
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
                .wait_for_service_delivery(crate::service::ServiceWaitKind::Object)
                .await;
        };
        match dispatcher().deliver_to_waiter(&task, pending_call, self.usr_ip()) {
            Ok(()) => Ok(()),
            Err(err) => {
                task.abort_service_wait(crate::service::ServiceWaitKind::Object);
                Err(err)
            }
        }
    }

    fn queue_or_deliver_call(&self, call: Arc<ServiceCall>) -> Result<(), ServiceError> {
        loop {
            let waiter = {
                let mut inner = self.inner.lock();
                if inner.lifecycle != UserObjectState::Active {
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
                // One queued waiter may have been cancelled or already
                // completed another request before this call reached it.
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

/// One control-plane guard that forwards one invoke request into the userspace
/// service dispatcher.
pub struct UserObjectGuard<'a> {
    object: &'a UserObject,
    mode: CpAccessMode,
    interface_caps: u32,
}

impl<'a> UserObjectGuard<'a> {
    /// Return the wrapped userspace object runtime.
    pub fn object(&self) -> &'a UserObject {
        self.object
    }
}

#[async_trait::async_trait]
impl SyscallDispatch<ObjectSyscallContext> for UserObjectGuard<'_> {
    async fn dispatch(
        &self,
        _caller: &ObjectSyscallContext,
        method_id: usize,
        arg1: usize,
        arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        match self
            .object
            .invoke(self.mode, self.interface_caps, method_id, arg1, arg2)
            .await
        {
            Ok(result) => Ok(result),
            Err(error) => Ok(error.into_syscall_result()),
        }
    }
}

impl ControlPlane for UserObject {
    type ReadGuard<'a>
        = UserObjectGuard<'a>
    where
        Self: 'a;
    type WriteGuard<'a>
        = UserObjectGuard<'a>
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = UserObjectGuard<'a>
    where
        Self: 'a;
    type AgentGuard<'a>
        = UserObjectGuard<'a>
    where
        Self: 'a;
    type AdminGuard<'a>
        = UserObjectGuard<'a>
    where
        Self: 'a;

    fn read(&self, interface_caps: u32) -> Self::ReadGuard<'_> {
        UserObjectGuard {
            object: self,
            mode: CpAccessMode::Read,
            interface_caps,
        }
    }

    fn write(&self, interface_caps: u32) -> Self::WriteGuard<'_> {
        UserObjectGuard {
            object: self,
            mode: CpAccessMode::Write,
            interface_caps,
        }
    }

    fn execute(&self, interface_caps: u32) -> Self::ExecuteGuard<'_> {
        UserObjectGuard {
            object: self,
            mode: CpAccessMode::Execute,
            interface_caps,
        }
    }

    fn agent(&self, interface_caps: u32) -> Self::AgentGuard<'_> {
        UserObjectGuard {
            object: self,
            mode: CpAccessMode::Agent,
            interface_caps,
        }
    }

    fn admin(&self, interface_caps: u32) -> Self::AdminGuard<'_> {
        UserObjectGuard {
            object: self,
            mode: CpAccessMode::Admin,
            interface_caps,
        }
    }
}
