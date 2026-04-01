//! Userspace service-object dispatch core.
//!
//! This subsystem owns the shared request/reply machinery used by:
//! - general userspace objects reached through `ObjectInvoke`;
//! - future userspace pager objects;
//! - future userspace syscall-handler objects.

mod dispatcher;
mod frame;
mod object;
mod pager;
mod syscall;

pub use dispatcher::{ServiceCall, ServiceCallId, ServiceDispatcher};
pub use frame::{ServiceDelivery, ServiceFrame, ServiceRole, ServiceWaitKind};
use libakarin_machine_core::sync::{NoOp, ScopedGuard};
use libakarin_sync::spin::Once;
pub use object::UserObject;
pub use pager::{PagerObject, PagerRegister, PagerRegisterScope, pager_runtime};
pub use syscall::{
    SyscallHandlerObject, SyscallRegister, SyscallRegisterScope,
    syscall_handler_runtime,
};

/// Return the kernel-global userspace service dispatcher.
pub fn dispatcher() -> &'static ServiceDispatcher {
    static DISPATCHER: Once<ServiceDispatcher, ScopedGuard<NoOp>> = Once::new();
    DISPATCHER.get_or_else(ServiceDispatcher::new)
}
