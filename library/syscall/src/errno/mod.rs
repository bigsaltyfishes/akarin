//! Stable syscall-visible errno definitions.
//!
//! This module is the single authority for all errors that may cross the
//! syscall ABI through the `UnderlyingError` channel, plus the stable
//! capability/object authorization error surface used by the
//! `ObjectError` channel.

mod common;
mod futex;
mod ipc;
mod irq;
mod object;
mod pci;
mod process;
mod service;
mod unified;
mod vm;

pub use common::SyscallError;
pub use futex::FutexError;
pub use ipc::IpcError;
pub use irq::IrqUnderlyingErrorCode;
pub use object::ObjectError;
pub use pci::PciUnderlyingErrorCode;
pub use process::{
    ProcessHandleInstallError, ProcessLoadError, ProcessSpawnError, ProcessVmarExtractError,
    ProcessWaitError,
};
pub use service::ServiceError;
pub use unified::{SyscallFailure, UnderlyingErrorKind, UnderlyingFailure};
pub use vm::VmError;
