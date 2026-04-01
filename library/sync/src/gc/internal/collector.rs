//! Fast and efficient concurrent memory reclamation.
//!
//! The core memory reclamation algorithm used by seize is described
//! [in this paper](https://arxiv.org/pdf/2108.02763.pdf). Specifically,
//! this module implements the Hyaline-1 variant of the algorithm.

use core::sync::atomic::{AtomicUsize, Ordering};

use super::local::Local;
use crate::gc::{Guard, guard::unprotected, list::List};

/// The raw collector that manages memory reclamation.
///
/// This collector maintains a list of all registered `Local` instances
/// and coordinates the global aspects of memory reclamation.
pub struct Global {
    /// All registered Local state machines.
    ///
    /// Local instances are stored in a lock-free intrusive linked list.
    /// Each Local is reference-counted through its handles.
    pub(crate) locals: List<Local>,

    /// All registered Local state machines number.
    pub(crate) local_count: AtomicUsize,

    /// A unique identifier for this collector.
    pub(crate) id: usize,

    /// The minimum number of nodes required in a batch before attempting
    /// retirement.
    pub(crate) batch_size: usize,
}

impl Global {
    /// Create a collector with the provided batch size and initial thread
    /// count.
    pub fn new(_threads: usize, batch_size: usize) -> Self {
        // A counter for collector IDs.
        static ID: AtomicUsize = AtomicUsize::new(0);

        Self {
            id: ID.fetch_add(1, Ordering::Relaxed),
            locals: List::new(),
            local_count: AtomicUsize::new(0),
            batch_size: batch_size.next_power_of_two(),
        }
    }

    /// Strengthens an ordering to that necessary to protect the load of a
    /// pointer.
    #[inline]
    pub fn protect(_order: Ordering) -> Ordering {
        // We have to respect both the user provided ordering and the ordering required
        // by the membarrier strategy. `SeqCst` is equivalent to `Acquire` on
        // most platforms, so we just use it unconditionally.
        //
        // Loads performed with this ordering, paired with the light barrier in `enter`,
        // will participate in the total order established by `enter`, and thus see the
        // new values of any pointers that were retired when the thread was inactive.
        Ordering::SeqCst
    }

    /// Reclaim all values that have been retired across all Locals.
    ///
    /// # Safety
    ///
    /// No threads may be accessing the collector or any values that have been
    /// retired. This is equivalent to having a unique reference to the data
    /// structure containing the collector.
    #[inline]
    pub unsafe fn reclaim_all(&self, guard: &Guard) {
        // Iterate over all Locals and reclaim their batches.
        for local in self.locals.iter(guard).filter_map(|r| r.ok()) {
            unsafe { local.reclaim() };
        }
    }
}

impl Drop for Global {
    fn drop(&mut self) {
        // Safety: Values are only retired after being made inaccessible to any
        // inactive threads. Additionally, we have `&mut self`, meaning that any
        // active threads are no longer accessing retired values.
        unsafe { self.reclaim_all(unprotected()) };
    }
}
