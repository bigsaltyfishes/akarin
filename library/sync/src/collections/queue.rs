//! Concurrent queue primitives adapted for kernel use.
//!
//! Source reference:
//! - https://github.com/smol-rs/concurrent-queue.git

use crossbeam_queue::{ArrayQueue, SegQueue};

#[derive(Debug)]
pub enum PushError<T> {
    Full(T),
}

pub enum ConcurrentQueue<T> {
    Bounded(ArrayQueue<T>),
    Unbounded(SegQueue<T>),
}

impl<T> ConcurrentQueue<T> {
    pub fn bounded(capacity: usize) -> Self {
        Self::Bounded(ArrayQueue::new(capacity))
    }

    pub const fn unbounded() -> Self {
        Self::Unbounded(SegQueue::new())
    }

    pub fn push(&self, value: T) -> Result<(), PushError<T>> {
        match self {
            Self::Bounded(queue) => queue.push(value).map_err(PushError::Full),
            Self::Unbounded(queue) => {
                queue.push(value);
                Ok(())
            }
        }
    }

    pub fn pop(&self) -> Option<T> {
        match self {
            Self::Bounded(queue) => queue.pop(),
            Self::Unbounded(queue) => queue.pop(),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Bounded(queue) => queue.is_empty(),
            Self::Unbounded(queue) => queue.is_empty(),
        }
    }

    pub fn is_full(&self) -> bool {
        match self {
            Self::Bounded(queue) => queue.is_full(),
            Self::Unbounded(_) => false,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Bounded(queue) => queue.len(),
            Self::Unbounded(queue) => queue.len(),
        }
    }
}
