use core::sync::atomic::{AtomicUsize, Ordering};

/// A synchronization primitive that can block a set number of threads until all
/// have reached a certain point.
pub struct Barrier {
    total: usize,
    barrier: AtomicUsize,
}

impl Barrier {
    /// Creates a new `Barrier` that can block a given number of threads.
    ///
    /// # Arguments
    ///
    /// * `total` - The number of threads that must call `wait` before any of
    ///   them can proceed.
    #[inline]
    pub const fn new(total: usize) -> Self {
        Self {
            total,
            barrier: AtomicUsize::new(0),
        }
    }

    /// Blocks the current thread until the barrier is released.
    ///
    /// # Returns
    ///
    /// - `true` if the current thread is the last to arrive at the barrier.
    /// - `false` otherwise.
    #[inline]
    pub fn wait(&self) -> bool {
        let previous = self.barrier.fetch_add(1, Ordering::AcqRel);

        if previous + 1 == self.total {
            self.barrier.store(0, Ordering::Release);
            true
        } else {
            while self.barrier.load(Ordering::Acquire) != 0 {
                core::hint::spin_loop();
            }
            false
        }
    }
}
