//! Generic syscall-family underlying errors.

/// Generic underlying error codes returned by the core syscall runtime and
/// the object/task syscall families.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyscallError {
    /// One syscall argument or decoded user buffer was invalid.
    InvalidArgument = 1,
    /// The supplied user buffer was too small for the requested operation.
    BufferTooSmall = 2,
    /// One user-supplied UTF-8 buffer was not valid UTF-8.
    InvalidUtf8 = 3,
    /// One user pointer or user buffer could not be accessed safely.
    Fault = 4,
    /// The requested syscall feature is defined but not supported here.
    NotSupported = 5,
    /// The requested non-blocking operation would block.
    WouldBlock = 6,
    /// The current syscall was interrupted before completion.
    Interrupted = 7,
    /// The requested syscall or method is not implemented yet.
    NotImplemented = 8,
    /// The kernel hit one unexpected internal condition.
    Internal = 9,
    /// The kernel could not allocate enough backing resources.
    OutOfMemory = 10,
}

impl TryFrom<usize> for SyscallError {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::InvalidArgument),
            2 => Ok(Self::BufferTooSmall),
            3 => Ok(Self::InvalidUtf8),
            4 => Ok(Self::Fault),
            5 => Ok(Self::NotSupported),
            6 => Ok(Self::WouldBlock),
            7 => Ok(Self::Interrupted),
            8 => Ok(Self::NotImplemented),
            9 => Ok(Self::Internal),
            10 => Ok(Self::OutOfMemory),
            _ => Err(()),
        }
    }
}
