//! Syscall family service implementations.
//!
//! These modules contain syscall business logic grouped by subsystem.

pub(super) mod futex;
pub(super) mod ipc;
pub(super) mod irq;
pub(super) mod object;
pub(super) mod process;
pub(super) mod service;
pub(super) mod task;
pub(super) mod vm;

pub(super) use super::{
    ObjectMetadata, ProcessContext, SyscallEnvironment, SyscallUnderlyingError,
};
