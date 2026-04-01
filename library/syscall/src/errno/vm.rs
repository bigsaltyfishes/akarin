//! VM syscall underlying errors.

/// Underlying error codes returned by VM syscalls after authentication and
/// object lookup have succeeded.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmError {
    /// One syscall argument or decoded VM request was invalid.
    InvalidArgument = 1,
    /// One VM range was malformed or out of bounds.
    InvalidRange = 2,
    /// The target address range is already mapped.
    AlreadyMapped = 3,
    /// The requested operation is not permitted by the mapping or object.
    PermissionDenied = 4,
    /// The target address range or mapping does not exist.
    NotMapped = 5,
    /// The supplied user buffer is too small for the result.
    BufferTooSmall = 6,
    /// The supplied user pointer could not be accessed.
    Fault = 7,
}

impl TryFrom<usize> for VmError {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::InvalidArgument),
            2 => Ok(Self::InvalidRange),
            3 => Ok(Self::AlreadyMapped),
            4 => Ok(Self::PermissionDenied),
            5 => Ok(Self::NotMapped),
            6 => Ok(Self::BufferTooSmall),
            7 => Ok(Self::Fault),
            _ => Err(()),
        }
    }
}
