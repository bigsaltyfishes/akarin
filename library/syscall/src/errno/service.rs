//! Userspace service-dispatch syscall underlying errors.

/// Underlying error codes returned by userspace service wait and reply
/// syscalls.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceError {
    /// One syscall argument or decoded service request was invalid.
    InvalidArgument = 1,
    /// The current thread or object is not in the expected service state.
    InvalidState = 2,
    /// No matching service waiter or service object is currently available.
    ServiceUnavailable = 3,
    /// The target service violated the kernel-side request/reply protocol.
    ProtocolViolation = 4,
    /// The target service or owning process faulted before producing a reply.
    ServiceFaulted = 5,
}

impl TryFrom<usize> for ServiceError {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::InvalidArgument),
            2 => Ok(Self::InvalidState),
            3 => Ok(Self::ServiceUnavailable),
            4 => Ok(Self::ProtocolViolation),
            5 => Ok(Self::ServiceFaulted),
            _ => Err(()),
        }
    }
}
