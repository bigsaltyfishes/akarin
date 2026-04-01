//! MPMC channel primitives for kernel/runtime IPC.
//!
//! Source references:
//! - https://github.com/smol-rs/concurrent-queue.git
//! - https://github.com/crossbeam-rs/crossbeam.git

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::{
    super::collections::{ConcurrentQueue, PushError},
    event::Event,
};

#[derive(Debug)]
pub enum SendError<T> {
    Closed(T),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvError {
    Empty,
    Closed,
}

struct Channel<T> {
    queue: ConcurrentQueue<T>,
    available: Event,
    space: Event,
    closed: AtomicBool,
    sender_count: AtomicUsize,
    receiver_count: AtomicUsize,
}

impl<T> Channel<T> {
    fn new_bounded(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            queue: ConcurrentQueue::bounded(capacity),
            available: Event::new(),
            space: Event::new(),
            closed: AtomicBool::new(false),
            sender_count: AtomicUsize::new(1),
            receiver_count: AtomicUsize::new(1),
        })
    }

    fn new_unbounded() -> Arc<Self> {
        Arc::new(Self {
            queue: ConcurrentQueue::unbounded(),
            available: Event::new(),
            space: Event::new(),
            closed: AtomicBool::new(false),
            sender_count: AtomicUsize::new(1),
            receiver_count: AtomicUsize::new(1),
        })
    }
}

pub struct Sender<T> {
    inner: Arc<Channel<T>>,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.inner.sender_count.fetch_add(1, Ordering::AcqRel);
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if self.inner.sender_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.inner.closed.store(true, Ordering::Release);
            self.inner.available.notify_all();
        }
    }
}

impl<T> Sender<T> {
    pub fn try_send(&self, mut value: T) -> Result<(), SendError<T>> {
        if self.inner.closed.load(Ordering::Acquire)
            || self.inner.receiver_count.load(Ordering::Acquire) == 0
        {
            return Err(SendError::Closed(value));
        }

        match self.inner.queue.push(value) {
            Ok(()) => {
                self.inner.available.notify(1);
                Ok(())
            }
            Err(PushError::Full(v)) => {
                value = v;
                Err(SendError::Closed(value))
            }
        }
    }

    pub async fn send(&self, mut value: T) -> Result<(), SendError<T>> {
        loop {
            if self.inner.closed.load(Ordering::Acquire)
                || self.inner.receiver_count.load(Ordering::Acquire) == 0
            {
                return Err(SendError::Closed(value));
            }

            match self.inner.queue.push(value) {
                Ok(()) => {
                    self.inner.available.notify(1);
                    return Ok(());
                }
                Err(PushError::Full(v)) => {
                    value = v;
                    self.inner.space.listen().await;
                }
            }
        }
    }

    pub fn send_blocking(&self, mut value: T) -> Result<(), SendError<T>> {
        loop {
            if self.inner.closed.load(Ordering::Acquire)
                || self.inner.receiver_count.load(Ordering::Acquire) == 0
            {
                return Err(SendError::Closed(value));
            }

            match self.inner.queue.push(value) {
                Ok(()) => {
                    self.inner.available.notify(1);
                    return Ok(());
                }
                Err(PushError::Full(v)) => {
                    value = v;
                    self.inner.space.listen().wait_blocking();
                }
            }
        }
    }
}

pub struct Receiver<T> {
    inner: Arc<Channel<T>>,
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        self.inner.receiver_count.fetch_add(1, Ordering::AcqRel);
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        if self.inner.receiver_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.inner.closed.store(true, Ordering::Release);
            self.inner.space.notify_all();
        }
    }
}

impl<T> Receiver<T> {
    pub fn try_recv(&self) -> Result<T, RecvError> {
        if let Some(value) = self.inner.queue.pop() {
            self.inner.space.notify(1);
            return Ok(value);
        }
        if self.inner.closed.load(Ordering::Acquire)
            || self.inner.sender_count.load(Ordering::Acquire) == 0
        {
            return Err(RecvError::Closed);
        }
        Err(RecvError::Empty)
    }

    pub async fn recv(&self) -> Result<T, RecvError> {
        loop {
            if let Some(value) = self.inner.queue.pop() {
                self.inner.space.notify(1);
                return Ok(value);
            }
            if self.inner.closed.load(Ordering::Acquire)
                || self.inner.sender_count.load(Ordering::Acquire) == 0
            {
                return Err(RecvError::Closed);
            }
            self.inner.available.listen().await;
        }
    }

    pub fn recv_blocking(&self) -> Result<T, RecvError> {
        loop {
            if let Some(value) = self.inner.queue.pop() {
                self.inner.space.notify(1);
                return Ok(value);
            }
            if self.inner.closed.load(Ordering::Acquire)
                || self.inner.sender_count.load(Ordering::Acquire) == 0
            {
                return Err(RecvError::Closed);
            }
            self.inner.available.listen().wait_blocking();
        }
    }
}

pub fn bounded<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    let inner = Channel::<T>::new_bounded(capacity);
    (
        Sender {
            inner: inner.clone(),
        },
        Receiver { inner },
    )
}

pub fn unbounded<T>() -> (Sender<T>, Receiver<T>) {
    let inner = Channel::<T>::new_unbounded();
    (
        Sender {
            inner: inner.clone(),
        },
        Receiver { inner },
    )
}
