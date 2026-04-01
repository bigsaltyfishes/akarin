//! Process lifecycle syscall underlying errors.

/// Underlying error codes returned by `ProcessLoad`.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessLoadError {
    /// One syscall argument or decoded load request was invalid.
    InvalidArgument = 1,
    /// The target process is not in a valid bootstrap-load state.
    InvalidState = 2,
    /// The requested bootstrap image is unavailable to the kernel.
    ImageUnavailable = 3,
    /// The supplied executable image is malformed or cannot be linked.
    InvalidImage = 4,
    /// The kernel could not map or prepare the process image layout.
    MappingFailed = 5,
}

impl TryFrom<usize> for ProcessLoadError {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::InvalidArgument),
            2 => Ok(Self::InvalidState),
            3 => Ok(Self::ImageUnavailable),
            4 => Ok(Self::InvalidImage),
            5 => Ok(Self::MappingFailed),
            _ => Err(()),
        }
    }
}

/// Underlying error codes returned by `ProcessVmarExtract`.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessVmarExtractError {
    /// One syscall argument or decoded extract request was invalid.
    InvalidArgument = 1,
    /// The target process is not in a valid bootstrap state for segment
    /// extraction.
    InvalidState = 2,
}

impl TryFrom<usize> for ProcessVmarExtractError {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::InvalidArgument),
            2 => Ok(Self::InvalidState),
            _ => Err(()),
        }
    }
}

/// Underlying error codes returned by `ProcessHandleInstall`.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessHandleInstallError {
    /// One syscall argument or decoded install request was invalid.
    InvalidArgument = 1,
    /// The target process is not in a valid bootstrap state for handle
    /// installation.
    InvalidState = 2,
}

impl TryFrom<usize> for ProcessHandleInstallError {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::InvalidArgument),
            2 => Ok(Self::InvalidState),
            _ => Err(()),
        }
    }
}

/// Underlying error codes returned by `ProcessTaskSpawn`.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessSpawnError {
    /// One syscall argument or decoded spawn request was invalid.
    InvalidArgument = 1,
    /// The target process is not in a valid bootstrap state for first-task
    /// spawn.
    InvalidState = 2,
    /// The requested instruction pointer is not a valid executable userspace
    /// address.
    InvalidInstructionPointer = 3,
    /// The requested stack pointer is not a valid mapped userspace stack
    /// address.
    InvalidStackPointer = 4,
    /// The requested TLS base is not a valid userspace address.
    InvalidTlsBase = 5,
    /// The kernel could not allocate enough resources to publish the task.
    OutOfMemory = 6,
    /// The scheduler or kernel hit one unexpected internal condition.
    Internal = 7,
}

impl TryFrom<usize> for ProcessSpawnError {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::InvalidArgument),
            2 => Ok(Self::InvalidState),
            3 => Ok(Self::InvalidInstructionPointer),
            4 => Ok(Self::InvalidStackPointer),
            5 => Ok(Self::InvalidTlsBase),
            6 => Ok(Self::OutOfMemory),
            7 => Ok(Self::Internal),
            _ => Err(()),
        }
    }
}

/// Underlying error codes returned by process wait operations.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessWaitError {
    /// One syscall argument or decoded wait request was invalid.
    InvalidArgument = 1,
    /// The target handle is not in a valid state for waiting.
    InvalidState = 2,
    /// The target process has not exited yet and the caller requested one
    /// non-blocking wait.
    WouldBlock = 3,
    /// The supplied timeout expired before the process exited.
    TimedOut = 4,
}

impl TryFrom<usize> for ProcessWaitError {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::InvalidArgument),
            2 => Ok(Self::InvalidState),
            3 => Ok(Self::WouldBlock),
            4 => Ok(Self::TimedOut),
            _ => Err(()),
        }
    }
}
