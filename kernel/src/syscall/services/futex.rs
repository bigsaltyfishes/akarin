use libakarin_syscall::{SYSCALL_STATUS_OK, SyscallArgs, SyscallResult, errno::FutexError};

use super::SyscallEnvironment;
use crate::{RuntimeServices, syscall::abi::IntoSyscallResult};

pub async fn wait(args: SyscallArgs) -> SyscallResult {
    let user_addr = args.args()[0];
    let expected = args.args()[1] as u32;
    let timeout_ns = args.args()[2];
    if args.args()[3] != 0 {
        return FutexError::InvalidArgument.into_syscall_result();
    }
    if args.args()[4] != 0 {
        return FutexError::InvalidArgument.into_syscall_result();
    }

    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(error) => return error.into_syscall_result(),
    };

    match RuntimeServices::global()
        .futex()
        .wait(&process, user_addr, expected, timeout_ns)
        .await
    {
        Ok(()) => SyscallResult::new(SYSCALL_STATUS_OK, [0, 0, 0, 0, 0]),
        Err(error) => error.into_syscall_result(),
    }
}

pub async fn wake(args: SyscallArgs) -> SyscallResult {
    let user_addr = args.args()[0];
    let wake_count = args.args()[1];
    if args.args()[2] != 0 {
        return FutexError::InvalidArgument.into_syscall_result();
    }
    if args.args()[3] != 0 || args.args()[4] != 0 {
        return FutexError::InvalidArgument.into_syscall_result();
    }

    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(error) => return error.into_syscall_result(),
    };

    match RuntimeServices::global()
        .futex()
        .wake(&process, user_addr, wake_count)
    {
        Ok(woken) => SyscallResult::new(SYSCALL_STATUS_OK, [woken, 0, 0, 0, 0]),
        Err(error) => error.into_syscall_result(),
    }
}
