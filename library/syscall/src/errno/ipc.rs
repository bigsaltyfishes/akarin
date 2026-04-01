//! IPC syscall underlying errors.

/// Underlying error codes returned by IPC syscalls after authentication and
/// object lookup have succeeded.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcError {
    /// One syscall argument or decoded message field was invalid.
    InvalidArgument = 1,
    /// The submitted message layout or transfer set was invalid.
    InvalidMessage = 2,
    /// The target port currently has no bound receiver.
    Unbound = 3,
    /// The source or destination port has been closed.
    PortClosed = 4,
    /// The bound receiver endpoint has been closed.
    ReceiverClosed = 5,
    /// The requested non-blocking IPC operation would block.
    WouldBlock = 6,
    /// The supplied user buffer is too small for the transferred message.
    BufferTooSmall = 7,
    /// The supplied user pointer could not be accessed.
    Fault = 8,
    /// The requested bind operation conflicts with an existing binding.
    AlreadyBound = 9,
    /// The target is already subscribed to the requested fanout.
    AlreadySubscribed = 10,
    /// The target is not currently subscribed to the requested fanout.
    NotSubscribed = 11,
    /// The operation is not valid for the supplied port kind.
    InvalidPortKind = 12,
}

impl TryFrom<usize> for IpcError {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::InvalidArgument),
            2 => Ok(Self::InvalidMessage),
            3 => Ok(Self::Unbound),
            4 => Ok(Self::PortClosed),
            5 => Ok(Self::ReceiverClosed),
            6 => Ok(Self::WouldBlock),
            7 => Ok(Self::BufferTooSmall),
            8 => Ok(Self::Fault),
            9 => Ok(Self::AlreadyBound),
            10 => Ok(Self::AlreadySubscribed),
            11 => Ok(Self::NotSubscribed),
            12 => Ok(Self::InvalidPortKind),
            _ => Err(()),
        }
    }
}
