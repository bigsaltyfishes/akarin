use libakarin_core::clock::time::Duration;
use libakarin_object::{Capability, ObjectError};
use libakarin_syscall::{
    INVALID_HANDLE_SLOT, ProcessObjectPhase, SYSCALL_STATUS_OK, SyscallArgs, SyscallResult,
};

use super::{SyscallEnvironment, SyscallUnderlyingError};
use crate::{
    Scheduler, SpawnError, sched::process::ProcessControl, syscall::abi::IntoSyscallResult,
};

pub async fn yield_now(_args: SyscallArgs) -> SyscallResult {
    Scheduler::yield_now().await;
    SyscallResult::new(SYSCALL_STATUS_OK, [0, 0, 0, 0, 0])
}

pub async fn sleep(args: SyscallArgs) -> SyscallResult {
    let duration = Duration::from_nanos(args.args()[0] as u64);
    if duration.as_nanos() == 0 {
        return yield_now(args).await;
    }

    Scheduler::sleep(duration).await;
    SyscallResult::new(SYSCALL_STATUS_OK, [0, 0, 0, 0, 0])
}

pub async fn create(args: SyscallArgs) -> SyscallResult {
    let process_slot = args.args()[0] as u32;
    let entry = args.args()[1];
    let user_sp = args.args()[2];
    let tls_base = args.args()[3];
    let flags = args.args()[4];
    if flags != 0 || entry == 0 {
        return SyscallUnderlyingError::InvalidArgument.into_syscall_result();
    }

    let caller = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let process_handle = match caller.acquire_handle(process_slot) {
        Ok(handle) => handle,
        Err(err) => return err.into_syscall_result(),
    };
    if !process_handle.capabilities().contains(Capability::ADMIN) {
        return ObjectError::InsufficientCapabilities.into_syscall_result();
    }

    let state = match process_handle.read_cp_with::<ProcessControl, _, _>(|process| {
        Ok::<_, ObjectError>((
            process.creating_only(),
            process.supervisor_only(),
            process.object_phase(),
        ))
    }) {
        Ok(Ok(state)) => state,
        Ok(Err(err)) => return err.into_syscall_result(),
        Err(err) => return err.into_syscall_result(),
    };

    let (creating_only, supervisor_only, phase) = state;
    if creating_only || supervisor_only || phase == ProcessObjectPhase::Terminating {
        return ObjectError::InsufficientCapabilities.into_syscall_result();
    }

    let (task_id, task_handle) = match process_handle.admin_cp_with::<ProcessControl, _, _>(
        |process| -> Result<_, SpawnError> { process.spawn_user_task(entry, user_sp, tls_base) },
    ) {
        Ok(Ok(result)) => result,
        Ok(Err(err)) => return err.into_syscall_result(),
        Err(err) => return err.into_syscall_result(),
    };
    let task_slot = caller.install_handle_auto(task_handle);
    SyscallResult::new(
        SYSCALL_STATUS_OK,
        [
            task_slot as usize,
            INVALID_HANDLE_SLOT as usize,
            task_id as usize,
            0,
            0,
        ],
    )
}

pub async fn exit(args: SyscallArgs) -> SyscallResult {
    SyscallResult::new(SYSCALL_STATUS_OK, [args.args()[0], 0, 0, 0, 0])
}
