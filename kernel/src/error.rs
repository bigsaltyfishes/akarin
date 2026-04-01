use libakarin_object::ObjectError;

/// Generic boundary error that can carry either one object-system failure or
/// one subsystem-specific underlying failure.
#[derive(Debug)]
pub enum ObjectOrUnderlyingError<E> {
    Object(ObjectError),
    Underlying(E),
}

impl<E> ObjectOrUnderlyingError<E> {
    /// Split this boundary error into one object-or-underlying result shape.
    pub fn into_object_or_underlying(self) -> Result<ObjectError, E> {
        match self {
            Self::Object(error) => Ok(error),
            Self::Underlying(error) => Err(error),
        }
    }
}

impl<E> From<ObjectError> for ObjectOrUnderlyingError<E> {
    fn from(value: ObjectError) -> Self {
        Self::Object(value)
    }
}

impl<E> TryFrom<ObjectOrUnderlyingError<E>> for ObjectError {
    type Error = ObjectOrUnderlyingError<E>;

    fn try_from(value: ObjectOrUnderlyingError<E>) -> Result<Self, Self::Error> {
        match value {
            ObjectOrUnderlyingError::Object(error) => Ok(error),
            other => Err(other),
        }
    }
}
