//! Kernel syscall subsystem façade.
//!
//! Layering:
//! - `dispatch`: typed syscall table storage and handler lookup.
//! - `registry`: built-in syscall registration wiring.
//! - `runtime`: lifecycle, publication, and dispatch entry.
//! - `services`: syscall family implementations.

use libakarin_object::{Capability, Handle, ObjectError};
use libakarin_syscall::{SyscallArgs, SyscallResult};

mod abi;
mod context;
mod dispatch;
mod registry;
mod runtime;
mod services;
pub mod user_ptr;
pub use abi::{IntoSyscallResult, SyscallUnderlyingError};
pub use context::{ProcessContext, SyscallEnvironment};
pub use dispatch::{SyscallHandler, SyscallTable};
pub use libakarin_syscall::{ObjectMetadata, Syscall};
use runtime::SyscallRuntime;
pub use user_ptr::{UserPtr, UserPtrError, UserSlice};

static SYSCALL_RUNTIME: SyscallRuntime = SyscallRuntime::new();

// Syscall services are organized under `syscall/services/*`.

/// Publish the syscall table object under `/Kernel/Syscall/Table`.
pub fn init() -> Result<&'static Handle, ObjectError> {
    SYSCALL_RUNTIME.init()
}

/// Return the kernel-owned syscall table admin handle.
#[allow(dead_code)]
pub fn table_owner() -> &'static Handle {
    SYSCALL_RUNTIME.table_owner()
}

/// Return the kernel-owned syscall-register admin handle.
#[allow(dead_code)]
pub fn register_owner() -> &'static Handle {
    SYSCALL_RUNTIME.register_owner()
}

/// Request a derived syscall table handle for delegation.
#[allow(dead_code)]
pub fn request_handle(capability: Capability, interface_caps: u32) -> Result<Handle, ObjectError> {
    SYSCALL_RUNTIME.request_handle(capability, interface_caps)
}

/// Request a derived syscall-register handle for delegation.
#[allow(dead_code)]
pub fn request_register_handle(
    capability: Capability,
    interface_caps: u32,
) -> Result<Handle, ObjectError> {
    SYSCALL_RUNTIME.request_register_handle(capability, interface_caps)
}

/// Register one built-in syscall handler.
#[allow(dead_code)]
pub fn register_handler(syscall: Syscall, handler: SyscallHandler) -> Result<(), ObjectError> {
    SYSCALL_RUNTIME.register_handler(syscall, handler)
}

/// Dispatch one syscall frame asynchronously.
pub async fn dispatch(args: SyscallArgs) -> SyscallResult {
    SYSCALL_RUNTIME.dispatch(args).await
}
