//! IRQ syscall and object underlying errors.

/// Stable backend error codes carried by IRQ fast-path syscalls and IRQ
/// object methods.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqUnderlyingErrorCode {
    /// The requested IRQ number is invalid for the active controller.
    InvalidIrq = 1,
    /// One method or syscall argument was invalid.
    InvalidParameter = 2,
    /// The controller could not allocate enough backing resources.
    OutOfResources = 3,
    /// The requested feature or delivery mode is not supported.
    NotSupported = 4,
    /// The requested non-blocking wait would block.
    WouldBlock = 5,
    /// The IRQ session or line has already been closed.
    Closed = 6,
    /// The controller does not support deadline-based waits.
    DeadlineUnsupported = 7,
}

impl TryFrom<usize> for IrqUnderlyingErrorCode {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::InvalidIrq),
            2 => Ok(Self::InvalidParameter),
            3 => Ok(Self::OutOfResources),
            4 => Ok(Self::NotSupported),
            5 => Ok(Self::WouldBlock),
            6 => Ok(Self::Closed),
            7 => Ok(Self::DeadlineUnsupported),
            _ => Err(()),
        }
    }
}
