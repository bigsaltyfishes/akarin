//! Shared service request / reply coordination.

use alloc::sync::Arc;

use hashbrown::HashMap;
use libakarin_sync::{
    asynchronous::Event,
    collections::IdAllocator,
    spin::{Once, SpinLock},
};
use log::warn;

use crate::{
    arch::guards::IrqSaveGuard,
    sched::{Scheduler, task::Task},
    service::frame::{ServiceDelivery, ServiceFrame},
};

/// Stable kernel-private identifier for one in-flight service call.
pub type ServiceCallId = u64;

fn service_call_ids() -> &'static IdAllocator<u64> {
    static IDS: Once<
        IdAllocator<u64>,
        libakarin_machine_core::sync::ScopedGuard<libakarin_machine_core::sync::NoOp>,
    > = Once::new();
    IDS.get_or_else(|| IdAllocator::new(1, 1))
}

/// Lifecycle states for one in-flight service call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceCallState {
    /// The request exists but has not yet been assigned to one waiting thread.
    Pending,
    /// One waiting thread has accepted and is currently executing the request.
    Running,
    /// The service thread replied and produced one terminal syscall result.
    Replied,
    /// The request was cancelled before a valid reply arrived.
    Cancelled,
}

/// One in-flight service request coordinated by the dispatcher.
pub struct ServiceCall {
    id: ServiceCallId,
    caller: Arc<Task>,
    request: ServiceFrame,
    state: SpinLock<ServiceCallState, IrqSaveGuard>,
    reply: SpinLock<Option<libakarin_syscall::SyscallResult>, IrqSaveGuard>,
    reply_event: Event,
    pub(crate) pending_link: libakarin_collections::intrusive::SingleLink,
}

impl ServiceCall {
    /// Create one new in-flight service request.
    pub fn new(caller: Arc<Task>, request: ServiceFrame) -> Arc<Self> {
        Arc::new(Self {
            id: service_call_ids().allocate(),
            caller,
            request,
            state: SpinLock::new(ServiceCallState::Pending),
            reply: SpinLock::new(None),
            reply_event: Event::new(),
            pending_link: libakarin_collections::intrusive::SingleLink::new(),
        })
    }

    /// Return the kernel-private call identifier.
    pub const fn id(&self) -> ServiceCallId {
        self.id
    }

    /// Return the raw request frame carried by this call.
    pub const fn request(&self) -> ServiceFrame {
        self.request
    }

    fn publish_caller_wakeup(&self) {
        let task_id = self.caller.id();
        let target_cpu = self.caller.sched_meta().cpu_id;
        if let Err(error) = Scheduler::queue_wakeup(target_cpu, task_id) {
            warn!(
                "[kernel/service] failed to queue caller wakeup for call {} task {} on cpu {}: \
                 {:?}",
                self.id, task_id, target_cpu, error
            );
        }
    }

    /// Mark the call as actively running on one selected service thread.
    pub fn mark_running(&self) {
        *self.state.lock() = ServiceCallState::Running;
    }

    /// Publish one terminal reply and wake the blocked caller.
    pub fn complete(&self, reply: libakarin_syscall::SyscallResult) {
        *self.reply.lock() = Some(reply);
        *self.state.lock() = ServiceCallState::Replied;
        self.reply_event.notify_all();
        self.publish_caller_wakeup();
    }

    /// Cancel this call with one terminal service failure reply.
    pub fn cancel(&self, error: libakarin_syscall::ServiceError) {
        *self.reply.lock() = Some(libakarin_syscall::SyscallResult::from(
            libakarin_syscall::SyscallFailure::Underlying(
                libakarin_syscall::UnderlyingFailure::Service(error),
            ),
        ));
        *self.state.lock() = ServiceCallState::Cancelled;
        self.reply_event.notify_all();
        self.publish_caller_wakeup();
    }

    /// Wait until one terminal reply has been published for this call.
    pub async fn wait_for_reply(&self) -> libakarin_syscall::SyscallResult {
        loop {
            let listener = {
                let reply = self.reply.lock();
                if let Some(frame) = *reply {
                    return frame;
                }
                self.reply_event.listen()
            };
            if let Some(frame) = *self.reply.lock() {
                return frame;
            }
            listener.await;
        }
    }
}

impl Drop for ServiceCall {
    fn drop(&mut self) {
        service_call_ids().recycle(self.id);
    }
}

impl
    libakarin_collections::intrusive::ElememtOf<
        ServiceCall,
        libakarin_collections::intrusive::SingleLink,
    > for ServiceCall
{
    fn link(node: &ServiceCall) -> &libakarin_collections::intrusive::SingleLink {
        &node.pending_link
    }

    fn link_mut(node: &mut ServiceCall) -> &mut libakarin_collections::intrusive::SingleLink {
        &mut node.pending_link
    }

    fn element(link: &libakarin_collections::intrusive::SingleLink) -> &ServiceCall {
        let offset = core::mem::offset_of!(ServiceCall, pending_link);
        unsafe {
            &*((link as *const libakarin_collections::intrusive::SingleLink).byte_sub(offset)
                as *const ServiceCall)
        }
    }

    fn element_mut(link: &mut libakarin_collections::intrusive::SingleLink) -> &mut ServiceCall {
        let offset = core::mem::offset_of!(ServiceCall, pending_link);
        unsafe {
            &mut *((link as *mut libakarin_collections::intrusive::SingleLink).byte_sub(offset)
                as *mut ServiceCall)
        }
    }
}

/// Shared kernel-global dispatcher that coordinates service calls, waiter
/// activation, and reply restoration.
pub struct ServiceDispatcher {
    inflight: SpinLock<HashMap<ServiceCallId, Arc<ServiceCall>>, IrqSaveGuard>,
}

impl ServiceDispatcher {
    /// Create one empty service dispatcher.
    pub fn new() -> Self {
        Self {
            inflight: SpinLock::new(HashMap::new()),
        }
    }

    /// Publish one new in-flight call under dispatcher ownership.
    pub fn register_call(&self, call: Arc<ServiceCall>) {
        self.inflight.lock().insert(call.id(), call);
    }

    /// Associate `call` with `task` and arm one pending service delivery.
    pub fn deliver_to_waiter(
        &self,
        task: &Arc<Task>,
        call: Arc<ServiceCall>,
        usr_ip: usize,
    ) -> Result<(), libakarin_syscall::ServiceError> {
        let delivery = ServiceDelivery::new(usr_ip, call.request());
        self.register_call(Arc::clone(&call));
        if let Err(err) = task.activate_service_call(call.id(), delivery) {
            self.inflight.lock().remove(&call.id());
            return Err(err);
        }
        let task_id = task.id();
        let target_cpu = task.sched_meta().cpu_id;
        if Scheduler::queue_wakeup(target_cpu, task_id).is_err() {
            self.inflight.lock().remove(&call.id());
            task.cancel_active_service_call(call.id());
            return Err(libakarin_syscall::ServiceError::ServiceUnavailable);
        }
        call.mark_running();
        Ok(())
    }

    /// Complete the current active call bound to `task`.
    pub fn complete_current_call(
        &self,
        task: &Arc<Task>,
        reply: libakarin_syscall::SyscallResult,
    ) -> Result<(), libakarin_syscall::ServiceError> {
        let call_id = task.take_active_service_call()?;
        let call = self
            .inflight
            .lock()
            .remove(&call_id)
            .ok_or(libakarin_syscall::ServiceError::InvalidState)?;
        call.complete(reply);
        Ok(())
    }

    /// Cancel one in-flight call after its servicing thread faults or exits.
    pub fn cancel_call(
        &self,
        call_id: ServiceCallId,
        error: libakarin_syscall::ServiceError,
    ) -> Result<(), libakarin_syscall::ServiceError> {
        let Some(call) = self.inflight.lock().remove(&call_id) else {
            return Err(libakarin_syscall::ServiceError::InvalidState);
        };
        call.cancel(error);
        Ok(())
    }
}
