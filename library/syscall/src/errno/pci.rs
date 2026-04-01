//! PCI syscall and object underlying errors.

/// Underlying error codes returned by PCI object methods after authentication
/// and object lookup have succeeded.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PciUnderlyingErrorCode {
    /// One method argument or decoded slow-path argument block was invalid.
    InvalidParameter = 1,
    /// The requested interrupt mode or action is not supported.
    NotSupported = 2,
    /// The controller could not allocate enough vectors or kernel objects.
    OutOfResources = 3,
    /// The supplied user buffer could not be accessed safely.
    Fault = 4,
    /// The supplied output slot buffer is too small for the requested count.
    BufferTooSmall = 5,
    /// The kernel PCI runtime or interrupt binder is not installed.
    RuntimeUnavailable = 6,
    /// Another interrupt reconfiguration is already in progress.
    Busy = 7,
    /// The interrupt controller failed while programming the requested mode.
    ControllerFailure = 8,
}

impl TryFrom<usize> for PciUnderlyingErrorCode {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::InvalidParameter),
            2 => Ok(Self::NotSupported),
            3 => Ok(Self::OutOfResources),
            4 => Ok(Self::Fault),
            5 => Ok(Self::BufferTooSmall),
            6 => Ok(Self::RuntimeUnavailable),
            7 => Ok(Self::Busy),
            8 => Ok(Self::ControllerFailure),
            _ => Err(()),
        }
    }
}
