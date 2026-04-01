use alloc::{boxed::Box, string::ToString};
use core::convert::TryFrom;

use libakarin_machine_core::sync::{NoOp, ScopedGuard};
use libakarin_object::{
    Capability, Handle, ObjectError, ObjectLifecycleFlags, Payload, WriteOperation,
};
use libakarin_sync::spin::{Once, TryInitError};
use libakarin_syscall::{SyscallArgs, SyscallError, SyscallResult};

use super::{Syscall, SyscallHandler, SyscallTable, registry};
use crate::{
    RuntimeServices,
    service::{SyscallRegister, SyscallRegisterScope, syscall_handler_runtime},
    syscall::abi::IntoSyscallResult,
};

/// Kernel syscall runtime.
///
/// Direct plane:
/// - own the typed syscall dispatch table used by kernel registration and
///   dispatch
///
/// Capability plane:
/// - publish `/Kernel/Syscall/Table`
/// - derive delegated handles to the published table object
pub struct SyscallRuntime {
    table: Once<&'static SyscallTable, ScopedGuard<NoOp>>,
    table_owner: Once<Handle, ScopedGuard<NoOp>>,
    register_owner: Once<Handle, ScopedGuard<NoOp>>,
}

impl SyscallRuntime {
    pub const fn new() -> Self {
        Self {
            table: Once::new(),
            table_owner: Once::new(),
            register_owner: Once::new(),
        }
    }

    pub fn init(&self) -> Result<&Handle, ObjectError> {
        if self.table_owner.is_initialized() {
            return Ok(self.table_owner.get());
        }
        let table: &'static SyscallTable = Box::leak(Box::new(SyscallTable::new()));
        let owner = RuntimeServices::global()
            .namespaces()
            .syscall_super()
            .write_with(|ns: &dyn WriteOperation| {
                ns.add_child(
                    "Table".to_string(),
                    Capability::ADMIN | Capability::AGENT,
                    Payload::new(table),
                )
            })??;
        owner.set_lifecycle_flags(ObjectLifecycleFlags::STICKY)?;
        let register = RuntimeServices::global()
            .namespaces()
            .syscall_super()
            .write_with(|ns: &dyn WriteOperation| {
                ns.add_child(
                    "Register".to_string(),
                    Capability::ADMIN | Capability::AGENT,
                    Payload::new(SyscallRegister::new(SyscallRegisterScope::Kernel)),
                )
            })??;
        register.set_lifecycle_flags(ObjectLifecycleFlags::STICKY)?;
        match self.table.try_init(table) {
            Ok(()) | Err(TryInitError::AlreadyInitialized(_)) => {}
            Err(TryInitError::Initializing(_)) => {
                panic!("syscall table initialization in progress")
            }
        }
        match self.table_owner.try_init(owner) {
            Ok(()) | Err(TryInitError::AlreadyInitialized(_)) => {}
            Err(TryInitError::Initializing(_)) => {
                panic!("syscall table initialization in progress")
            }
        }
        match self.register_owner.try_init(register) {
            Ok(()) | Err(TryInitError::AlreadyInitialized(_)) => {}
            Err(TryInitError::Initializing(_)) => {
                panic!("syscall register initialization in progress")
            }
        }
        registry::install_builtin_handlers(self.table())?;
        log::info!("[kernel/syscall] published syscall table at /Kernel/Syscall/Table");
        Ok(self.table_owner.get())
    }

    /// Return the private direct syscall table reference used by kernel
    /// registration and dispatch.
    pub fn table(&self) -> &'static SyscallTable {
        self.table.get()
    }

    pub fn table_owner(&self) -> &Handle {
        self.table_owner.get()
    }

    /// Return the kernel-owned syscall-register admin handle.
    pub fn register_owner(&self) -> &Handle {
        self.register_owner.get()
    }

    /// Derive one delegated handle for the published syscall table object.
    pub fn request_handle(
        &self,
        capability: Capability,
        interface_caps: u32,
    ) -> Result<Handle, ObjectError> {
        self.table_owner().derive_handle(capability, interface_caps)
    }

    /// Derive one delegated handle for the published syscall-register object.
    pub fn request_register_handle(
        &self,
        capability: Capability,
        interface_caps: u32,
    ) -> Result<Handle, ObjectError> {
        self.register_owner()
            .derive_handle(capability, interface_caps)
    }

    pub fn register_handler(
        &self,
        syscall: Syscall,
        handler: SyscallHandler,
    ) -> Result<(), ObjectError> {
        self.table().register_handler(syscall, handler);
        Ok(())
    }

    pub async fn dispatch(&self, args: SyscallArgs) -> SyscallResult {
        let Ok(syscall) = Syscall::try_from(args.method_id()) else {
            return SyscallError::InvalidArgument.into_syscall_result();
        };
        if let Some(handler) = syscall_handler_runtime().handler_for(syscall) {
            return match handler.submit_request(args).await {
                Ok(reply) => reply,
                Err(error) => error.into_syscall_result(),
            };
        }
        let handler = self.table().handler(syscall);
        match handler {
            Some(handler) => handler(args).await,
            None => SyscallError::NotImplemented.into_syscall_result(),
        }
    }
}
