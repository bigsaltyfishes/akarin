//! Stable capability/object-system authorization errors.

use core::fmt::{Display, Formatter, Result as FmtResult};

/// Stable capability/object-system authorization errors returned through the
/// top-level `ObjectError` syscall channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectError {
    /// The object has already been destroyed.
    ObjectDestroyed,
    /// The supplied handle does not carry the required capabilities.
    InsufficientCapabilities,
    /// One decoded argument, path, or handle payload type was invalid.
    InvalidArgument,
    /// The requested object or child entry does not exist.
    ObjectNotFound,
    /// The supplied child name is reserved or otherwise unsafe to create.
    DangerousName,
    /// The requested child name already exists under the target parent.
    DuplicateChildName,
    /// The object already owns one unique `AGENT` handle.
    AgentHandleAlreadyExists,
}

impl ObjectError {
    /// Return the stable ABI code carried in the `ObjectError` result slot.
    pub const fn abi_code(self) -> usize {
        match self {
            Self::ObjectDestroyed => 1,
            Self::InsufficientCapabilities => 2,
            Self::InvalidArgument => 3,
            Self::ObjectNotFound => 4,
            Self::DangerousName => 5,
            Self::DuplicateChildName => 6,
            Self::AgentHandleAlreadyExists => 7,
        }
    }

    /// Decode one stable ABI code back into the corresponding object error.
    pub const fn from_abi_code(code: usize) -> Option<Self> {
        match code {
            1 => Some(Self::ObjectDestroyed),
            2 => Some(Self::InsufficientCapabilities),
            3 => Some(Self::InvalidArgument),
            4 => Some(Self::ObjectNotFound),
            5 => Some(Self::DangerousName),
            6 => Some(Self::DuplicateChildName),
            7 => Some(Self::AgentHandleAlreadyExists),
            _ => None,
        }
    }
}

impl Display for ObjectError {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        let text = match self {
            Self::ObjectDestroyed => "object has been destroyed",
            Self::InsufficientCapabilities => "insufficient capabilities",
            Self::InvalidArgument => "invalid argument provided",
            Self::ObjectNotFound => "object not found",
            Self::DangerousName => "dangerous name",
            Self::DuplicateChildName => "duplicate child name",
            Self::AgentHandleAlreadyExists => "agent handle already exists for object",
        };
        f.write_str(text)
    }
}

impl core::error::Error for ObjectError {}
