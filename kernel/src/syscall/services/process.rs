use alloc::string::String;

use libakarin_core::memory::{VmLayoutSegment, Vmo};
use libakarin_syscall::{
    INVALID_HANDLE_SLOT, ProcessHandleInstallError, ProcessInheritHandleArgs, ProcessLoadError,
    ProcessSpawnError, ProcessVmSegment, ProcessVmarExtractError, SYSCALL_STATUS_OK, SyscallArgs,
    SyscallResult,
};

use super::{SyscallEnvironment, SyscallUnderlyingError};
use crate::{
    RuntimeServices, UserSlice, sched::process::ProcessControl, syscall::abi::IntoSyscallResult,
};

/// Create one new process and return the caller-local slot containing its
/// anonymous bootstrap control handle.
pub async fn create(args: SyscallArgs) -> SyscallResult {
    let name = UserSlice::<u8>::new(args.args()[0], args.args()[1]);
    let flags = args.args()[2];
    if flags != 0 {
        return SyscallUnderlyingError::InvalidArgument.into_syscall_result();
    }

    let caller = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let bytes = match name.copy_to_vec(&caller) {
        Ok(bytes) => bytes,
        Err(_) => return SyscallUnderlyingError::Fault.into_syscall_result(),
    };
    let name = match String::from_utf8(bytes) {
        Ok(name) if !name.is_empty() => name,
        Ok(_) => return SyscallUnderlyingError::InvalidArgument.into_syscall_result(),
        Err(_) => return SyscallUnderlyingError::InvalidUtf8.into_syscall_result(),
    };

    let created = match RuntimeServices::global()
        .namespaces()
        .scheduler_manager()
        .create_process_control(&name, VmLayoutSegment::user_space_range())
    {
        Ok(created) => created,
        Err(err) => return err.into_syscall_result(),
    };

    let process_slot = caller.install_handle_auto(created);
    SyscallResult::new(SYSCALL_STATUS_OK, [process_slot as usize, 0, 0, 0, 0])
}

/// Load one Mach-O image into one bootstrap process and cache the rebased
/// image metadata.
pub async fn load(args: SyscallArgs) -> SyscallResult {
    let process_slot = args.args()[0] as u32;
    let image_slot = args.args()[1] as u32;
    let flags = args.args()[2];
    if flags != 0 || image_slot == INVALID_HANDLE_SLOT {
        return SyscallUnderlyingError::InvalidArgument.into_syscall_result();
    }

    let caller = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let image_handle = match caller.acquire_handle(image_slot) {
        Ok(handle) => handle,
        Err(err) => return err.into_syscall_result(),
    };
    let image_vmo = match image_handle.read_cp_with::<Vmo, _, _>(|vmo| vmo.share_vm()) {
        Ok(Ok(vmo)) => vmo,
        Ok(Err(_error)) => return ProcessLoadError::InvalidArgument.into_syscall_result(),
        Err(err) => return err.into_syscall_result(),
    };

    let process_handle = match caller.acquire_handle(process_slot) {
        Ok(handle) => handle,
        Err(err) => return err.into_syscall_result(),
    };
    let loaded = match process_handle
        .write_cp_with::<ProcessControl, _, _>(|process| process.load_image_vmo(&image_vmo, flags))
    {
        Ok(Ok(info)) => info,
        Ok(Err(err)) => return err.into_syscall_result(),
        Err(err) => return err.into_syscall_result(),
    };

    SyscallResult::new(
        SYSCALL_STATUS_OK,
        [
            loaded.entry_ip,
            loaded.image_base,
            loaded.image_end,
            loaded.slide,
            0,
        ],
    )
}

/// Extract one fixed-segment VMAR handle from one bootstrap process.
pub async fn vmar_extract(args: SyscallArgs) -> SyscallResult {
    let process_slot = args.args()[0] as u32;
    let segment = match ProcessVmSegment::try_from(args.args()[1]) {
        Ok(segment) => segment,
        Err(()) => return ProcessVmarExtractError::InvalidArgument.into_syscall_result(),
    };

    let caller = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let process_handle = match caller.acquire_handle(process_slot) {
        Ok(handle) => handle,
        Err(err) => return err.into_syscall_result(),
    };
    let handle = match process_handle.write_cp_with::<ProcessControl, _, _>(|process| {
        process.derive_segment_vmar_handle(segment)
    }) {
        Ok(Ok(handle)) => handle,
        Ok(Err(err)) => return err.into_syscall_result(),
        Err(err) => return err.into_syscall_result(),
    };
    let slot = caller.install_handle_auto(handle);
    SyscallResult::new(SYSCALL_STATUS_OK, [slot as usize, 0, 0, 0, 0])
}

/// Install one delegated handle into one bootstrap process.
pub async fn handle_install(args: SyscallArgs) -> SyscallResult {
    let process_slot = args.args()[0] as u32;
    let source_slot = args.args()[1] as u32;
    let capability_bits = args.args()[2];
    let interface_caps = match u32::try_from(args.args()[3]) {
        Ok(caps) => caps,
        Err(_) => return ProcessHandleInstallError::InvalidArgument.into_syscall_result(),
    };
    let target_slot = match u32::try_from(args.args()[4]) {
        Ok(slot) => slot,
        Err(_) => return ProcessHandleInstallError::InvalidArgument.into_syscall_result(),
    };
    let caller = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let process_handle = match caller.acquire_handle(process_slot) {
        Ok(handle) => handle,
        Err(err) => return err.into_syscall_result(),
    };
    let installed = match process_handle.write_cp_with::<ProcessControl, _, _>(|process| {
        process.inherit_handle_from_caller(
            &crate::syscall::ProcessContext::new(caller.clone()),
            ProcessInheritHandleArgs {
                source_slot,
                capability_bits: capability_bits as u32,
                interface_caps,
                target_slot,
            },
        )
    }) {
        Ok(Ok(slot)) => slot,
        Ok(Err(err)) => return err.into_syscall_result(),
        Err(err) => return err.into_syscall_result(),
    };
    SyscallResult::new(SYSCALL_STATUS_OK, [installed as usize, 0, 0, 0, 0])
}

/// Validate and start the first userspace task for one bootstrap process.
pub async fn task_spawn(args: SyscallArgs) -> SyscallResult {
    let process_slot = args.args()[0] as u32;
    let entry = args.args()[1];
    let user_sp = args.args()[2];
    let tls_base = args.args()[3];
    let flags = args.args()[4];
    if flags != 0 {
        return SyscallUnderlyingError::InvalidArgument.into_syscall_result();
    }

    let caller = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let creating_handle = match caller.take_handle(process_slot) {
        Ok(handle) => handle,
        Err(err) => return err.into_syscall_result(),
    };

    let committed = creating_handle.admin_cp_with::<ProcessControl, _, _>(
        |process| -> Result<_, ProcessSpawnError> {
            if !process.creating_only() {
                return Err(ProcessSpawnError::InvalidState);
            }
            process
                .load_control()
                .spawn_initial_task(entry, user_sp, tls_base)
        },
    );

    let (task_id, supervisor_handle) = match committed {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            caller.install_handle(process_slot, creating_handle);
            return error.into_syscall_result();
        }
        Err(err) => {
            caller.install_handle(process_slot, creating_handle);
            return err.into_syscall_result();
        }
    };

    caller.install_handle(process_slot, supervisor_handle);
    SyscallResult::new(
        SYSCALL_STATUS_OK,
        [process_slot as usize, task_id as usize, 0, 0, 0],
    )
}

/// Exit the current process with one caller-supplied code.
pub async fn exit(args: SyscallArgs) -> SyscallResult {
    SyscallResult::new(SYSCALL_STATUS_OK, [args.args()[0], 0, 0, 0, 0])
}
