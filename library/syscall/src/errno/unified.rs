//! Unified syscall failure model shared by kernel and user space.
//!
//! This module provides one typed error envelope that can carry all stable
//! syscall-visible failures and round-trip with `SyscallResult`.

use super::{
    FutexError, IpcError, IrqUnderlyingErrorCode, ObjectError, PciUnderlyingErrorCode,
    ProcessHandleInstallError, ProcessLoadError, ProcessSpawnError, ProcessVmarExtractError,
    ProcessWaitError, ServiceError, SyscallError, VmError,
};

/// Stable subsystem tag encoded in `SyscallResult.values[1]` when
/// `status == SYSCALL_STATUS_UNDERLYING_ERROR`.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnderlyingErrorKind {
    /// Generic syscall-family errors.
    Syscall = 1,
    /// IPC subsystem errors.
    Ipc = 2,
    /// VM subsystem errors.
    Vm = 3,
    /// Process image loading errors.
    ProcessLoad = 4,
    /// Process fixed-segment VMAR extraction errors.
    ProcessVmarExtract = 5,
    /// Process bootstrap handle-installation errors.
    ProcessHandleInstall = 6,
    /// Process initial-task spawn errors.
    ProcessSpawn = 7,
    /// Process wait errors.
    ProcessWait = 8,
    /// Futex subsystem errors.
    Futex = 9,
    /// IRQ subsystem errors.
    Irq = 10,
    /// PCI subsystem errors.
    Pci = 11,
    /// Userspace service-dispatch subsystem errors.
    Service = 12,
}

impl TryFrom<usize> for UnderlyingErrorKind {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Syscall),
            2 => Ok(Self::Ipc),
            3 => Ok(Self::Vm),
            4 => Ok(Self::ProcessLoad),
            5 => Ok(Self::ProcessVmarExtract),
            6 => Ok(Self::ProcessHandleInstall),
            7 => Ok(Self::ProcessSpawn),
            8 => Ok(Self::ProcessWait),
            9 => Ok(Self::Futex),
            10 => Ok(Self::Irq),
            11 => Ok(Self::Pci),
            _ => Err(()),
        }
    }
}

/// One decoded underlying-error payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnderlyingFailure {
    /// Generic syscall-family underlying error.
    Syscall(SyscallError),
    /// IPC-family underlying error.
    Ipc(IpcError),
    /// VM-family underlying error.
    Vm(VmError),
    /// Process image-loading underlying error.
    ProcessLoad(ProcessLoadError),
    /// Process VMAR-extraction underlying error.
    ProcessVmarExtract(ProcessVmarExtractError),
    /// Process handle-install underlying error.
    ProcessHandleInstall(ProcessHandleInstallError),
    /// Process initial-task-spawn underlying error.
    ProcessSpawn(ProcessSpawnError),
    /// Process wait underlying error.
    ProcessWait(ProcessWaitError),
    /// Futex underlying error.
    Futex(FutexError),
    /// IRQ underlying error.
    Irq(IrqUnderlyingErrorCode),
    /// PCI underlying error.
    Pci(PciUnderlyingErrorCode),
    /// Userspace service-dispatch underlying error.
    Service(ServiceError),
}

impl UnderlyingFailure {
    /// Return the encoded kind/code/detail tuple used by `SyscallResult`.
    pub const fn to_parts(self) -> (usize, usize, [usize; 3]) {
        match self {
            Self::Syscall(error) => (
                UnderlyingErrorKind::Syscall as usize,
                error as usize,
                [0, 0, 0],
            ),
            Self::Ipc(error) => (UnderlyingErrorKind::Ipc as usize, error as usize, [0, 0, 0]),
            Self::Vm(error) => (UnderlyingErrorKind::Vm as usize, error as usize, [0, 0, 0]),
            Self::ProcessLoad(error) => (
                UnderlyingErrorKind::ProcessLoad as usize,
                error as usize,
                [0, 0, 0],
            ),
            Self::ProcessVmarExtract(error) => (
                UnderlyingErrorKind::ProcessVmarExtract as usize,
                error as usize,
                [0, 0, 0],
            ),
            Self::ProcessHandleInstall(error) => (
                UnderlyingErrorKind::ProcessHandleInstall as usize,
                error as usize,
                [0, 0, 0],
            ),
            Self::ProcessSpawn(error) => (
                UnderlyingErrorKind::ProcessSpawn as usize,
                error as usize,
                [0, 0, 0],
            ),
            Self::ProcessWait(error) => (
                UnderlyingErrorKind::ProcessWait as usize,
                error as usize,
                [0, 0, 0],
            ),
            Self::Futex(error) => (
                UnderlyingErrorKind::Futex as usize,
                error as usize,
                [0, 0, 0],
            ),
            Self::Irq(error) => (UnderlyingErrorKind::Irq as usize, error as usize, [0, 0, 0]),
            Self::Pci(error) => (UnderlyingErrorKind::Pci as usize, error as usize, [0, 0, 0]),
            Self::Service(error) => (
                UnderlyingErrorKind::Service as usize,
                error as usize,
                [0, 0, 0],
            ),
        }
    }

    /// Decode one encoded underlying payload into one typed failure.
    pub fn from_parts(code: usize, kind: usize, _detail: [usize; 3]) -> Self {
        match UnderlyingErrorKind::try_from(kind) {
            Ok(UnderlyingErrorKind::Syscall) => SyscallError::try_from(code)
                .map(Self::Syscall)
                .expect("invalid SyscallError code"),
            Ok(UnderlyingErrorKind::Ipc) => IpcError::try_from(code)
                .map(Self::Ipc)
                .expect("invalid IpcError code"),
            Ok(UnderlyingErrorKind::Vm) => VmError::try_from(code)
                .map(Self::Vm)
                .expect("invalid VmError code"),
            Ok(UnderlyingErrorKind::ProcessLoad) => ProcessLoadError::try_from(code)
                .map(Self::ProcessLoad)
                .expect("invalid ProcessLoadError code"),
            Ok(UnderlyingErrorKind::ProcessVmarExtract) => ProcessVmarExtractError::try_from(code)
                .map(Self::ProcessVmarExtract)
                .expect("invalid ProcessVmarExtractError code"),
            Ok(UnderlyingErrorKind::ProcessHandleInstall) => {
                ProcessHandleInstallError::try_from(code)
                    .map(Self::ProcessHandleInstall)
                    .expect("invalid ProcessHandleInstallError code")
            }
            Ok(UnderlyingErrorKind::ProcessSpawn) => ProcessSpawnError::try_from(code)
                .map(Self::ProcessSpawn)
                .expect("invalid ProcessSpawnError code"),
            Ok(UnderlyingErrorKind::ProcessWait) => ProcessWaitError::try_from(code)
                .map(Self::ProcessWait)
                .expect("invalid ProcessWaitError code"),
            Ok(UnderlyingErrorKind::Futex) => FutexError::try_from(code)
                .map(Self::Futex)
                .expect("invalid FutexError code"),
            Ok(UnderlyingErrorKind::Irq) => IrqUnderlyingErrorCode::try_from(code)
                .map(Self::Irq)
                .expect("invalid IrqUnderlyingErrorCode code"),
            Ok(UnderlyingErrorKind::Pci) => PciUnderlyingErrorCode::try_from(code)
                .map(Self::Pci)
                .expect("invalid PciUnderlyingErrorCode code"),
            Ok(UnderlyingErrorKind::Service) => ServiceError::try_from(code)
                .map(Self::Service)
                .expect("invalid ServiceError code"),
            Err(()) => panic!("invalid UnderlyingErrorKind: {}", kind),
        }
    }
}

impl From<SyscallError> for UnderlyingFailure {
    fn from(error: SyscallError) -> Self {
        Self::Syscall(error)
    }
}

impl From<IpcError> for UnderlyingFailure {
    fn from(error: IpcError) -> Self {
        Self::Ipc(error)
    }
}

impl From<VmError> for UnderlyingFailure {
    fn from(error: VmError) -> Self {
        Self::Vm(error)
    }
}

impl From<ProcessLoadError> for UnderlyingFailure {
    fn from(error: ProcessLoadError) -> Self {
        Self::ProcessLoad(error)
    }
}

impl From<ProcessVmarExtractError> for UnderlyingFailure {
    fn from(error: ProcessVmarExtractError) -> Self {
        Self::ProcessVmarExtract(error)
    }
}

impl From<ProcessHandleInstallError> for UnderlyingFailure {
    fn from(error: ProcessHandleInstallError) -> Self {
        Self::ProcessHandleInstall(error)
    }
}

impl From<ProcessSpawnError> for UnderlyingFailure {
    fn from(error: ProcessSpawnError) -> Self {
        Self::ProcessSpawn(error)
    }
}

impl From<ProcessWaitError> for UnderlyingFailure {
    fn from(error: ProcessWaitError) -> Self {
        Self::ProcessWait(error)
    }
}

impl From<FutexError> for UnderlyingFailure {
    fn from(error: FutexError) -> Self {
        Self::Futex(error)
    }
}

impl From<IrqUnderlyingErrorCode> for UnderlyingFailure {
    fn from(error: IrqUnderlyingErrorCode) -> Self {
        Self::Irq(error)
    }
}

impl From<PciUnderlyingErrorCode> for UnderlyingFailure {
    fn from(error: PciUnderlyingErrorCode) -> Self {
        Self::Pci(error)
    }
}

impl From<ServiceError> for UnderlyingFailure {
    fn from(error: ServiceError) -> Self {
        Self::Service(error)
    }
}

/// One decoded failed syscall result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyscallFailure {
    /// One object/capability authorization failure.
    Object(ObjectError),
    /// One subsystem-defined underlying failure.
    Underlying(UnderlyingFailure),
}

impl From<ObjectError> for SyscallFailure {
    fn from(error: ObjectError) -> Self {
        Self::Object(error)
    }
}

impl<T> From<T> for SyscallFailure
where
    T: Into<UnderlyingFailure>,
{
    fn from(error: T) -> Self {
        Self::Underlying(error.into())
    }
}

impl TryFrom<SyscallFailure> for ObjectError {
    type Error = SyscallFailure;

    fn try_from(value: SyscallFailure) -> Result<Self, Self::Error> {
        match value {
            SyscallFailure::Object(error) => Ok(error),
            other => Err(other),
        }
    }
}

macro_rules! impl_try_from_failure_underlying {
    ($ty:ty, $variant:path) => {
        impl TryFrom<SyscallFailure> for $ty {
            type Error = SyscallFailure;

            fn try_from(value: SyscallFailure) -> Result<Self, Self::Error> {
                match value {
                    SyscallFailure::Underlying($variant(error)) => Ok(error),
                    other => Err(other),
                }
            }
        }
    };
}

impl_try_from_failure_underlying!(SyscallError, UnderlyingFailure::Syscall);
impl_try_from_failure_underlying!(IpcError, UnderlyingFailure::Ipc);
impl_try_from_failure_underlying!(VmError, UnderlyingFailure::Vm);
impl_try_from_failure_underlying!(ProcessLoadError, UnderlyingFailure::ProcessLoad);
impl_try_from_failure_underlying!(
    ProcessVmarExtractError,
    UnderlyingFailure::ProcessVmarExtract
);
impl_try_from_failure_underlying!(
    ProcessHandleInstallError,
    UnderlyingFailure::ProcessHandleInstall
);
impl_try_from_failure_underlying!(ProcessSpawnError, UnderlyingFailure::ProcessSpawn);
impl_try_from_failure_underlying!(ProcessWaitError, UnderlyingFailure::ProcessWait);
impl_try_from_failure_underlying!(FutexError, UnderlyingFailure::Futex);
impl_try_from_failure_underlying!(IrqUnderlyingErrorCode, UnderlyingFailure::Irq);
impl_try_from_failure_underlying!(PciUnderlyingErrorCode, UnderlyingFailure::Pci);
impl_try_from_failure_underlying!(ServiceError, UnderlyingFailure::Service);
