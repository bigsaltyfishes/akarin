//! Event listener primitive adapted for kernel use.
//!
//! Source reference:
//! - https://github.com/smol-rs/event-listener.git

mod strategy;

use core::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll, Waker},
};

use crossbeam_queue::SegQueue;
pub use strategy::{Blocking, NonBlocking, Strategy};

/// An asynchronous event primitive.
///
/// This type allows asynchronous tasks to wait for certain events to occur. It
/// provides methods to listen for events and to notify waiting tasks when the
/// event has occurred.
pub struct Event {
    epoch: AtomicUsize,
    waiters: SegQueue<Waker>,
}

impl Event {
    pub fn new() -> Self {
        Self {
            epoch: AtomicUsize::new(0),
            waiters: SegQueue::new(),
        }
    }

    pub fn listen(&self) -> Listener<'_> {
        Listener {
            event: self,
            observed: self.epoch.load(Ordering::Acquire),
            registered: false,
        }
    }

    pub fn notify(&self, count: usize) -> usize {
        if count == 0 {
            return 0;
        }
        self.epoch.fetch_add(1, Ordering::AcqRel);
        let mut wake_count = 0;
        for _ in 0..count {
            let Some(waker) = self.waiters.pop() else {
                break;
            };
            wake_count += 1;
            waker.wake();
        }
        wake_count
    }

    pub fn notify_all(&self) -> usize {
        self.notify(usize::MAX)
    }
}

impl Default for Event {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Listener<'a> {
    event: &'a Event,
    observed: usize,
    registered: bool,
}

impl Listener<'_> {
    pub fn wait_blocking(self) {
        let target = self.observed;
        while self.event.epoch.load(Ordering::Acquire) == target {
            core::hint::spin_loop();
        }
    }
}

impl Future for Listener<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let now = this.event.epoch.load(Ordering::Acquire);
        if now != this.observed {
            return Poll::Ready(());
        }

        if !this.registered {
            this.event.waiters.push(cx.waker().clone());
            this.registered = true;
        }

        Poll::Pending
    }
}
