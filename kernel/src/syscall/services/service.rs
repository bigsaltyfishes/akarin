use alloc::{string::String, sync::Arc};

use libakarin_core::memory::VmFlags;
use libakarin_machine_core::memory::VirtAddr;
use libakarin_object::{Capability, ObjectError, Payload, WriteOperation};
use libakarin_syscall::{
    FutexError, IpcError, IrqUnderlyingErrorCode, PciUnderlyingErrorCode,
    ProcessHandleInstallError, ProcessLoadError, ProcessSpawnError, ProcessVmarExtractError,
    ProcessWaitError, SYSCALL_STATUS_OK, ServiceError, Syscall, SyscallArgs, SyscallError,
    SyscallFailure, SyscallResult, UnderlyingErrorKind, UnderlyingFailure, VmError,
};

use super::SyscallEnvironment;
use crate::{
    Scheduler,
    sched::process::ProcessControl,
    service::{
        PagerObject, PagerRegister, PagerRegisterScope, SyscallHandlerObject, SyscallRegister,
        SyscallRegisterScope, UserObject, syscall_handler_runtime,
    },
    syscall::{abi::IntoSyscallResult, user_ptr::UserSlice},
};

fn decode_underlying_failure(kind: usize, code: usize) -> Result<UnderlyingFailure, ServiceError> {
    match UnderlyingErrorKind::try_from(kind) {
        Ok(UnderlyingErrorKind::Syscall) => SyscallError::try_from(code)
            .map(UnderlyingFailure::Syscall)
            .map_err(|_| ServiceError::InvalidArgument),
        Ok(UnderlyingErrorKind::Ipc) => IpcError::try_from(code)
            .map(UnderlyingFailure::Ipc)
            .map_err(|_| ServiceError::InvalidArgument),
        Ok(UnderlyingErrorKind::Vm) => VmError::try_from(code)
            .map(UnderlyingFailure::Vm)
            .map_err(|_| ServiceError::InvalidArgument),
        Ok(UnderlyingErrorKind::ProcessLoad) => ProcessLoadError::try_from(code)
            .map(UnderlyingFailure::ProcessLoad)
            .map_err(|_| ServiceError::InvalidArgument),
        Ok(UnderlyingErrorKind::ProcessVmarExtract) => ProcessVmarExtractError::try_from(code)
            .map(UnderlyingFailure::ProcessVmarExtract)
            .map_err(|_| ServiceError::InvalidArgument),
        Ok(UnderlyingErrorKind::ProcessHandleInstall) => ProcessHandleInstallError::try_from(code)
            .map(UnderlyingFailure::ProcessHandleInstall)
            .map_err(|_| ServiceError::InvalidArgument),
        Ok(UnderlyingErrorKind::ProcessSpawn) => ProcessSpawnError::try_from(code)
            .map(UnderlyingFailure::ProcessSpawn)
            .map_err(|_| ServiceError::InvalidArgument),
        Ok(UnderlyingErrorKind::ProcessWait) => ProcessWaitError::try_from(code)
            .map(UnderlyingFailure::ProcessWait)
            .map_err(|_| ServiceError::InvalidArgument),
        Ok(UnderlyingErrorKind::Futex) => FutexError::try_from(code)
            .map(UnderlyingFailure::Futex)
            .map_err(|_| ServiceError::InvalidArgument),
        Ok(UnderlyingErrorKind::Irq) => IrqUnderlyingErrorCode::try_from(code)
            .map(UnderlyingFailure::Irq)
            .map_err(|_| ServiceError::InvalidArgument),
        Ok(UnderlyingErrorKind::Pci) => PciUnderlyingErrorCode::try_from(code)
            .map(UnderlyingFailure::Pci)
            .map_err(|_| ServiceError::InvalidArgument),
        Ok(UnderlyingErrorKind::Service) => ServiceError::try_from(code)
            .map(UnderlyingFailure::Service)
            .map_err(|_| ServiceError::InvalidArgument),
        Err(()) => Err(ServiceError::InvalidArgument),
    }
}

fn resolve_supervisor_process(
    caller: &Arc<crate::Process>,
    process_slot: u32,
) -> Result<Arc<crate::Process>, SyscallResult> {
    let process_handle = match caller.acquire_handle(process_slot) {
        Ok(handle) => handle,
        Err(err) => return Err(err.into_syscall_result()),
    };
    match process_handle.admin_cp_with::<ProcessControl, _, _>(|process| {
        if !process.supervisor_only() {
            return Err(ObjectError::InsufficientCapabilities);
        }
        Ok::<_, ObjectError>(Arc::clone(process.process()))
    }) {
        Ok(Ok(process)) => Ok(process),
        Ok(Err(err)) | Err(err) => Err(err.into_syscall_result()),
    }
}

fn validate_service_usr_ip(
    process: &Arc<crate::Process>,
    usr_ip: usize,
) -> Result<(), ServiceError> {
    if usr_ip == 0 {
        return Err(ServiceError::InvalidArgument);
    }
    let addr = VirtAddr::new(usr_ip);
    let Some(end_addr) = usr_ip.checked_add(1).map(VirtAddr::new) else {
        return Err(ServiceError::InvalidArgument);
    };
    let Some(range) = libakarin_core::memory::VmRange::new(addr, end_addr) else {
        return Err(ServiceError::InvalidArgument);
    };
    let required = VmFlags::EXECUTE | VmFlags::USER;
    if process.validate_user_range(range, required).is_err() {
        return Err(ServiceError::InvalidArgument);
    }
    Ok(())
}

/// Create one general userspace service object and install it into the caller.
pub async fn user_object_create(args: SyscallArgs) -> SyscallResult {
    let parent_slot = args.args()[0] as u32;
    let process_slot = args.args()[1] as u32;
    let name_ptr = args.args()[2];
    let name_len = args.args()[3];
    let usr_ip = args.args()[4];
    if usr_ip == 0 {
        return ServiceError::InvalidArgument.into_syscall_result();
    }

    let caller = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let target_process = match resolve_supervisor_process(&caller, process_slot) {
        Ok(process) => process,
        Err(result) => return result,
    };
    if let Err(err) = validate_service_usr_ip(&target_process, usr_ip) {
        return err.into_syscall_result();
    }

    let name_bytes = match UserSlice::<u8>::new(name_ptr, name_len).copy_to_vec(&caller) {
        Ok(bytes) => bytes,
        Err(_) => return ServiceError::InvalidArgument.into_syscall_result(),
    };
    let name = match String::from_utf8(name_bytes) {
        Ok(name) => name,
        Err(_) => return ServiceError::InvalidArgument.into_syscall_result(),
    };
    let parent_handle = match caller.acquire_handle(parent_slot) {
        Ok(handle) => handle,
        Err(err) => return err.into_syscall_result(),
    };
    let object = UserObject::new(target_process, usr_ip);
    let handle = match parent_handle.write_with(|parent: &dyn WriteOperation| {
        parent.add_child(
            name,
            Capability::ADMIN | Capability::AGENT,
            Payload::new(object),
        )
    }) {
        Ok(Ok(handle)) => handle,
        Ok(Err(err)) => return err.into_syscall_result(),
        Err(err) => return err.into_syscall_result(),
    };
    match handle.write_with(|child: &dyn WriteOperation| {
        child.set_public_interface_caps(u32::MAX);
        Ok::<(), ObjectError>(())
    }) {
        Ok(Ok(())) => {}
        Ok(Err(err)) | Err(err) => return err.into_syscall_result(),
    }
    let slot = caller.install_handle_auto(handle);
    SyscallResult::new(SYSCALL_STATUS_OK, [slot as usize, 0, 0, 0, 0])
}

/// Create one special pager object and consume the supplied pager-register
/// handle.
pub async fn pager_create(args: SyscallArgs) -> SyscallResult {
    let register_slot = args.args()[0] as u32;
    let process_slot = args.args()[1] as u32;
    let usr_ip = args.args()[2];
    let flags = args.args()[3];
    if flags != 0 {
        return ServiceError::InvalidArgument.into_syscall_result();
    }

    let caller = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let target_process = match resolve_supervisor_process(&caller, process_slot) {
        Ok(process) => process,
        Err(result) => return result,
    };
    if let Err(err) = validate_service_usr_ip(&target_process, usr_ip) {
        return err.into_syscall_result();
    }

    let register_handle = match caller.take_handle(register_slot) {
        Ok(handle) => handle,
        Err(err) => return err.into_syscall_result(),
    };
    let register_scope = match register_handle.admin_cp_with::<PagerRegister, _, _>(|guard| {
        Ok::<_, ObjectError>(guard.register().scope())
    }) {
        Ok(Ok(scope)) => scope,
        Ok(Err(err)) | Err(err) => {
            let _ = caller.install_handle(register_slot, register_handle);
            return err.into_syscall_result();
        }
    };
    if !matches!(register_scope, PagerRegisterScope::Kernel) {
        let _ = caller.install_handle(register_slot, register_handle);
        return ServiceError::InvalidState.into_syscall_result();
    }

    let pager_handle = libakarin_object::Handle::new_anonymous(
        Payload::new(PagerObject::new(target_process, usr_ip)),
        Capability::empty(),
    );
    let slot = caller.install_handle_auto(pager_handle);
    SyscallResult::new(SYSCALL_STATUS_OK, [slot as usize, 0, 0, 0, 0])
}

/// Block the current thread until one `UserObject` request is assigned to it.
pub async fn wait_object_request(args: SyscallArgs) -> SyscallResult {
    let object_slot = args.args()[0] as u32;
    let caller = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let handle = match caller.acquire_handle(object_slot) {
        Ok(handle) => handle,
        Err(err) => return err.into_syscall_result(),
    };
    let object = match handle.execute_cp_with::<UserObject, _, _>(|guard| guard.object().clone()) {
        Ok(object) => object,
        Err(err) => return err.into_syscall_result(),
    };
    if object.process().pid() != caller.pid() {
        return ServiceError::InvalidState.into_syscall_result();
    }

    let Some(task) = Scheduler::current_task_ref() else {
        return ServiceError::InvalidState.into_syscall_result();
    };
    match object.wait(task).await {
        Ok(()) => SyscallResult::new(SYSCALL_STATUS_OK, [0, 0, 0, 0, 0]),
        Err(err) => err.into_syscall_result(),
    }
}

/// Block the current thread until one pager request is assigned to it.
pub async fn wait_pager_request(args: SyscallArgs) -> SyscallResult {
    let pager_slot = args.args()[0] as u32;
    let caller = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let handle = match caller.acquire_handle(pager_slot) {
        Ok(handle) => handle,
        Err(err) => return err.into_syscall_result(),
    };
    let pager = match handle.admin_cp_with::<PagerObject, _, _>(|guard| guard.object().clone()) {
        Ok(pager) => pager,
        Err(err) => return err.into_syscall_result(),
    };
    if pager.process().pid() != caller.pid() {
        return ServiceError::InvalidState.into_syscall_result();
    }

    let Some(task) = Scheduler::current_task_ref() else {
        return ServiceError::InvalidState.into_syscall_result();
    };
    match pager.wait(task).await {
        Ok(()) => SyscallResult::new(SYSCALL_STATUS_OK, [0, 0, 0, 0, 0]),
        Err(err) => err.into_syscall_result(),
    }
}

/// Create one special syscall-handler object and consume the supplied
/// syscall-register handle.
pub async fn syscall_handler_create(args: SyscallArgs) -> SyscallResult {
    let register_slot = args.args()[0] as u32;
    let process_slot = args.args()[1] as u32;
    let usr_ip = args.args()[2];
    let syscall_nr = match Syscall::try_from(args.args()[3]) {
        Ok(syscall) => syscall,
        Err(()) => return ServiceError::InvalidArgument.into_syscall_result(),
    };
    let flags = args.args()[4];
    if flags != 0 {
        return ServiceError::InvalidArgument.into_syscall_result();
    }

    let caller = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let target_process = match resolve_supervisor_process(&caller, process_slot) {
        Ok(process) => process,
        Err(result) => return result,
    };
    if let Err(err) = validate_service_usr_ip(&target_process, usr_ip) {
        return err.into_syscall_result();
    }

    let register_handle = match caller.take_handle(register_slot) {
        Ok(handle) => handle,
        Err(err) => return err.into_syscall_result(),
    };
    let register_scope = match register_handle.admin_cp_with::<SyscallRegister, _, _>(|guard| {
        Ok::<_, ObjectError>(guard.register().scope())
    }) {
        Ok(Ok(scope)) => scope,
        Ok(Err(err)) | Err(err) => {
            let _ = caller.install_handle(register_slot, register_handle);
            return err.into_syscall_result();
        }
    };
    if !matches!(register_scope, SyscallRegisterScope::Kernel) {
        let _ = caller.install_handle(register_slot, register_handle);
        return ServiceError::InvalidState.into_syscall_result();
    }

    let handler = SyscallHandlerObject::new(target_process, usr_ip, syscall_nr);
    if let Err(err) = syscall_handler_runtime().bind_handler(handler.clone()) {
        let _ = caller.install_handle(register_slot, register_handle);
        return err.into_syscall_result();
    }

    let handler_handle =
        libakarin_object::Handle::new_anonymous(Payload::new(handler), Capability::empty());
    let slot = caller.install_handle_auto(handler_handle);
    SyscallResult::new(SYSCALL_STATUS_OK, [slot as usize, 0, 0, 0, 0])
}

/// Block the current thread until one forwarded syscall request is assigned to
/// it.
pub async fn wait_syscall_request(args: SyscallArgs) -> SyscallResult {
    let handler_slot = args.args()[0] as u32;
    let caller = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let handle = match caller.acquire_handle(handler_slot) {
        Ok(handle) => handle,
        Err(err) => return err.into_syscall_result(),
    };
    let handler = match handle.admin_cp_with::<SyscallHandlerObject, _, _>(|guard| {
        Ok::<_, ObjectError>(guard.object().clone())
    }) {
        Ok(Ok(handler)) => handler,
        Ok(Err(err)) | Err(err) => return err.into_syscall_result(),
    };
    if handler.process().pid() != caller.pid() {
        return ServiceError::InvalidState.into_syscall_result();
    }

    let Some(task) = Scheduler::current_task_ref() else {
        return ServiceError::InvalidState.into_syscall_result();
    };
    match handler.wait(task).await {
        Ok(()) => SyscallResult::new(SYSCALL_STATUS_OK, [0, 0, 0, 0, 0]),
        Err(err) => err.into_syscall_result(),
    }
}

/// Reply to the current active service request with one successful payload.
pub async fn reply_ok(args: SyscallArgs) -> SyscallResult {
    let Some(task) = Scheduler::current_task_ref() else {
        return ServiceError::InvalidState.into_syscall_result();
    };
    let reply = SyscallResult::new(SYSCALL_STATUS_OK, *args.args());
    match crate::service::dispatcher().complete_current_call(&task, reply) {
        Ok(()) => SyscallResult::new(SYSCALL_STATUS_OK, [0, 0, 0, 0, 0]),
        Err(err) => err.into_syscall_result(),
    }
}

/// Reply to the current active service request with one object error.
pub async fn reply_object_error(args: SyscallArgs) -> SyscallResult {
    let Some(error) = libakarin_syscall::ObjectError::from_abi_code(args.args()[0]) else {
        return ServiceError::InvalidArgument.into_syscall_result();
    };
    let Some(task) = Scheduler::current_task_ref() else {
        return ServiceError::InvalidState.into_syscall_result();
    };
    let reply = SyscallResult::from(SyscallFailure::Object(error));
    match crate::service::dispatcher().complete_current_call(&task, reply) {
        Ok(()) => SyscallResult::new(SYSCALL_STATUS_OK, [0, 0, 0, 0, 0]),
        Err(err) => err.into_syscall_result(),
    }
}

/// Reply to the current active service request with one underlying error.
pub async fn reply_underlying(args: SyscallArgs) -> SyscallResult {
    let kind = args.args()[0];
    let code = args.args()[1];
    let failure = match decode_underlying_failure(kind, code) {
        Ok(failure) => failure,
        Err(err) => return err.into_syscall_result(),
    };
    let Some(task) = Scheduler::current_task_ref() else {
        return ServiceError::InvalidState.into_syscall_result();
    };
    let reply = SyscallResult::from(SyscallFailure::Underlying(failure));
    match crate::service::dispatcher().complete_current_call(&task, reply) {
        Ok(()) => SyscallResult::new(SYSCALL_STATUS_OK, [0, 0, 0, 0, 0]),
        Err(err) => err.into_syscall_result(),
    }
}
