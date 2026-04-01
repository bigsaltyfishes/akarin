//! Futex syscall underlying errors.

/// Underlying error codes returned by futex syscalls after the current task
/// and process context have been resolved successfully.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FutexError {
    /// One futex syscall argument or decoded flag set was invalid.
    InvalidArgument = 1,
    /// The supplied futex word could not be accessed safely.
    Fault = 2,
    /// The futex word no longer matches the expected value.
    WouldBlock = 3,
    /// The requested futex wait timed out.
    TimedOut = 4,
    /// The requested futex operation is not supported yet.
    NotSupported = 5,
}

impl TryFrom<usize> for FutexError {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::InvalidArgument),
            2 => Ok(Self::Fault),
            3 => Ok(Self::WouldBlock),
            4 => Ok(Self::TimedOut),
            5 => Ok(Self::NotSupported),
            _ => Err(()),
        }
    }
}
