use core::sync::atomic::{AtomicUsize, Ordering};

use libakarin_machine_core::sync::DroppableScopedGuard;

use super::Semaphore;

/// An asynchronous condition variable.
///
/// This type allows asynchronous tasks to wait for certain conditions to be
/// met. It provides methods to wait for a condition and to notify waiting tasks
/// when the condition has changed.
pub struct Condvar<S: DroppableScopedGuard> {
    inner: Semaphore<S>,
    waiters: AtomicUsize,
}

impl<S: DroppableScopedGuard> Condvar<S> {
    /// Creates a new [`Condvar`].
    pub fn new() -> Self {
        Self {
            inner: Semaphore::new(0),
            waiters: AtomicUsize::new(0),
        }
    }

    /// Blocks the current asynchronous task until notified.
    pub async fn wait(&self) {
        self.waiters.fetch_add(1, Ordering::SeqCst);
        self.inner.acquire().await.forget();
        self.waiters.fetch_sub(1, Ordering::SeqCst);
    }

    /// Notifies one waiting asynchronous task.
    pub fn notify_one(&self) {
        if self.waiters.load(Ordering::SeqCst) > 0 {
            self.inner.add_permits(1);
        }
    }

    /// Notifies all waiting asynchronous tasks.
    pub fn notify_all(&self) {
        let waiters = self.waiters.swap(0, Ordering::SeqCst);
        if waiters > 0 {
            self.inner.add_permits(waiters);
        }
    }
}
