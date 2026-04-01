use core::fmt::Debug;

use libakarin_object::ObjectError;
use libakarin_syscall::{
    SYSCALL_STATUS_OBJECT_ERROR, SyscallResult,
    errno::{
        FutexError, IpcError, IrqUnderlyingErrorCode, PciUnderlyingErrorCode,
        ProcessHandleInstallError, ProcessLoadError, ProcessSpawnError, ProcessVmarExtractError,
        ProcessWaitError, ServiceError, SyscallError, SyscallFailure, VmError,
    },
};

use crate::{
    error::ObjectOrUnderlyingError,
    sched::{SpawnError, process::SpawnTaskError},
};

/// Kernel-wide alias for generic syscall-family underlying errors.
pub type SyscallUnderlyingError = libakarin_syscall::SyscallError;

/// Convert one kernel-side error or frame into one syscall result frame.
pub trait IntoSyscallResult {
    fn into_syscall_result(self) -> SyscallResult;
}

impl IntoSyscallResult for ObjectError {
    fn into_syscall_result(self) -> SyscallResult {
        SyscallResult::new(SYSCALL_STATUS_OBJECT_ERROR, [self.abi_code(), 0, 0, 0, 0])
    }
}

macro_rules! impl_into_syscall_result_failure {
    ($ty:ty) => {
        impl IntoSyscallResult for $ty {
            fn into_syscall_result(self) -> SyscallResult {
                SyscallResult::from(SyscallFailure::from(self))
            }
        }
    };
}

impl_into_syscall_result_failure!(SyscallError);
impl_into_syscall_result_failure!(IpcError);
impl_into_syscall_result_failure!(VmError);
impl_into_syscall_result_failure!(ProcessLoadError);
impl_into_syscall_result_failure!(ProcessVmarExtractError);
impl_into_syscall_result_failure!(ProcessHandleInstallError);
impl_into_syscall_result_failure!(ProcessSpawnError);
impl_into_syscall_result_failure!(ProcessWaitError);
impl_into_syscall_result_failure!(FutexError);
impl_into_syscall_result_failure!(IrqUnderlyingErrorCode);
impl_into_syscall_result_failure!(PciUnderlyingErrorCode);
impl_into_syscall_result_failure!(ServiceError);

impl<E> IntoSyscallResult for ObjectOrUnderlyingError<E>
where
    E: Debug + IntoSyscallResult,
{
    fn into_syscall_result(self) -> SyscallResult {
        match self {
            ObjectOrUnderlyingError::Object(error) => error.into_syscall_result(),
            ObjectOrUnderlyingError::Underlying(error) => error.into_syscall_result(),
        }
    }
}

impl IntoSyscallResult for SpawnError {
    fn into_syscall_result(self) -> SyscallResult {
        match self {
            SpawnError::Object(error) => error.into_syscall_result(),
            SpawnError::Scheduler(SpawnTaskError::Object(error)) => error.into_syscall_result(),
            SpawnError::Scheduler(SpawnTaskError::Scheduler(_)) => {
                SyscallUnderlyingError::Internal.into_syscall_result()
            }
            SpawnError::OutOfMemory => SyscallUnderlyingError::OutOfMemory.into_syscall_result(),
        }
    }
}
