//! Syscall dispatch table primitives.
//!
//! This module contains only the table object and lookup/registration
//! operations. Built-in wiring lives in `registry`.

use alloc::{boxed::Box, collections::BTreeMap, sync::Arc};
use core::{future::Future, pin::Pin};

use libakarin_object::{ControlPlane, SyscallDispatch};
use libakarin_sync::spin::SpinRwLock;
use libakarin_syscall::{Syscall, SyscallArgs, SyscallResult};

use crate::arch::guards::IrqSaveGuard;

/// Boxed future returned by one syscall handler.
pub type SyscallFuture = Pin<Box<dyn Future<Output = SyscallResult> + Send + 'static>>;

/// Type erased asynchronous syscall handler.
pub type SyscallHandler = Arc<dyn Fn(SyscallArgs) -> SyscallFuture + Send + Sync + 'static>;

/// Kernel-owned syscall dispatch table object.
pub struct SyscallTable {
    handlers: SpinRwLock<BTreeMap<usize, SyscallHandler>, IrqSaveGuard>,
}

impl SyscallTable {
    /// Create an empty syscall dispatch table.
    pub fn new() -> Self {
        Self {
            handlers: SpinRwLock::new(BTreeMap::new()),
        }
    }

    /// Register or replace one syscall handler.
    pub fn register_handler(&self, syscall: Syscall, handler: SyscallHandler) {
        self.handlers.write().insert(syscall as usize, handler);
    }

    /// Remove one syscall handler.
    #[allow(dead_code)]
    pub fn unregister_handler(&self, syscall: Syscall) -> Option<SyscallHandler> {
        self.handlers.write().remove(&(syscall as usize))
    }

    /// Return a cloned handler for one syscall id.
    pub fn handler(&self, syscall: Syscall) -> Option<SyscallHandler> {
        self.handlers.read().get(&(syscall as usize)).cloned()
    }
}

impl ControlPlane for SyscallTable {
    type ReadGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type WriteGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type AgentGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type AdminGuard<'a>
        = &'a Self
    where
        Self: 'a;

    fn read(&self, _interface_caps: u32) -> Self::ReadGuard<'_> {
        self
    }

    fn write(&self, _interface_caps: u32) -> Self::WriteGuard<'_> {
        self
    }

    fn execute(&self, _interface_caps: u32) -> Self::ExecuteGuard<'_> {
        self
    }

    fn agent(&self, _interface_caps: u32) -> Self::AgentGuard<'_> {
        self
    }

    fn admin(&self, _interface_caps: u32) -> Self::AdminGuard<'_> {
        self
    }
}

impl SyscallDispatch<libakarin_object::ObjectSyscallContext> for SyscallTable {}

impl ControlPlane for &'static SyscallTable {
    type ReadGuard<'a>
        = &'static SyscallTable
    where
        Self: 'a;
    type WriteGuard<'a>
        = &'static SyscallTable
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = &'static SyscallTable
    where
        Self: 'a;
    type AgentGuard<'a>
        = &'static SyscallTable
    where
        Self: 'a;
    type AdminGuard<'a>
        = &'static SyscallTable
    where
        Self: 'a;

    fn read(&self, _interface_caps: u32) -> Self::ReadGuard<'_> {
        *self
    }

    fn write(&self, _interface_caps: u32) -> Self::WriteGuard<'_> {
        *self
    }

    fn execute(&self, _interface_caps: u32) -> Self::ExecuteGuard<'_> {
        *self
    }

    fn agent(&self, _interface_caps: u32) -> Self::AgentGuard<'_> {
        *self
    }

    fn admin(&self, _interface_caps: u32) -> Self::AdminGuard<'_> {
        *self
    }
}
