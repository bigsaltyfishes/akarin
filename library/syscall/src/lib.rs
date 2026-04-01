#![no_std]

extern crate alloc;

mod abi;
mod client;
mod dispatch;
pub mod errno;

/// Platform-neutral syscall ABI surface shared by kernel and user space.
pub mod common {
    pub use crate::{abi::*, errno::*};
}

/// Kernel-facing helpers for syscall dispatch and context management.
pub mod kernel {
    pub use crate::dispatch::*;
}

/// User-space helpers that wrap the raw syscall ABI with typed helpers.
pub mod user {
    pub use crate::client::*;
}

pub use common::*;
pub use kernel::{CpAccessMode, SyscallContext, SyscallDispatch, UserCopyError};
pub use user::{InvokeError, IpcClient, SyscallInvoker};
