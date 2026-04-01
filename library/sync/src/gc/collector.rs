//! A concurrent garbage collector.

use alloc::sync::Arc;
use core::{fmt, ops::Deref, sync::atomic::Ordering};

use crate::gc::{
    Guard,
    internal::{self, Local},
    unprotected,
};

/// A concurrent garbage collector.
///
/// A `Collector` manages the access and retirement of concurrent objects.
/// Objects can be safely loaded through *guards*, which are created by
/// registering a [`LocalHandle`] and calling [`pin`](LocalHandle::pin).
///
/// Every instance of a concurrent data structure should typically own its
/// `Collector`. This allows the garbage collection of non-`'static` values, as
/// memory reclamation is guaranteed to run when the `Collector` is dropped.
///
/// # Examples
///
/// ```rust
/// use libakarin_sync::gc::Collector;
///
/// // Create a collector.
/// let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
/// let collector = Collector::new(cpus);
///
/// // Register to get a handle for this participant.
/// let handle = collector.register();
///
/// // Pin to create a guard that protects loads.
/// let guard = handle.pin();
///
/// // Use the guard to protect loads...
/// drop(guard);
/// ```
#[derive(Clone)]
pub struct Collector {
    /// The underlying raw collector instance.
    pub(crate) raw: Arc<internal::Global>,
}

impl Collector {
    /// The default batch size for a new collector.
    const DEFAULT_BATCH_SIZE: usize = 32;

    /// Creates a new collector.
    ///
    /// The number of CPUs is used to determine the minimum batch size for
    /// reclamation.
    /// # Examples
    /// ```
    /// use libakarin_sync::gc::Collector;
    /// use std::sync::Arc;
    /// let collector = Collector::new(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1));
    /// ```
    #[inline]
    pub fn new(cpus: usize) -> Self {
        // Ensure every batch accumulates at least as many entries
        // as there are threads on the system.
        let batch_size = cpus.max(Self::DEFAULT_BATCH_SIZE);

        Self {
            raw: Arc::new(internal::Global::new(cpus, batch_size)),
        }
    }

    /// Sets the number of objects that must be in a batch before reclamation is
    /// attempted.
    ///
    /// Retired objects are added to *batches* before starting the
    /// reclamation process. After `batch_size` is hit, the objects are moved to
    /// separate *retirement lists*, where reference counting kicks in and
    /// batches are eventually reclaimed.
    ///
    /// A larger batch size amortizes the cost of retirement. However,
    /// reclamation latency can also grow due to the large number of objects
    /// needed to be freed. Note that reclamation can not be attempted
    /// unless the batch contains at least as many objects as the number of
    /// active participants.
    ///
    /// The default batch size is `32`.
    pub fn batch_size(mut self, batch_size: usize) -> Self {
        // We need to modify the Arc, so we have to get_mut or recreate
        if let Some(raw) = Arc::get_mut(&mut self.raw) {
            raw.batch_size = batch_size;
        }
        self
    }

    /// Register a new participant with this collector, returning a handle.
    ///
    /// Each participant (thread/task) that needs to access protected objects
    /// should register with the collector to get a [`LocalHandle`].
    /// The handle can then be used to create guards via [`LocalHandle::pin`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use libakarin_sync::gc::Collector;
    /// use std::sync::Arc;
    ///
    /// let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    /// let collector = Collector::new(cpus);
    /// let handle = collector.register();
    ///
    /// // Use the handle to create guards.
    /// let guard = handle.pin();
    /// ```
    #[inline]
    pub fn register(&self) -> LocalHandle {
        Local::register(self)
    }

    /// Reclaim any values that have been retired.
    ///
    /// This method reclaims any objects that have been retired across *all*
    /// participants. After calling this method, any values that were previously
    /// retired, or retired recursively on the current thread during this
    /// call, will have been reclaimed.
    ///
    /// # Safety
    ///
    /// This function is **extremely unsafe** to call. It is only sound when no
    /// threads are currently active, whether accessing values that have
    /// been retired or accessing the collector through any type of guard.
    /// This is akin to having a unique reference to the collector. However,
    /// this method takes a shared reference, as reclaimers to
    /// be run by this thread are allowed to access the collector recursively.
    ///
    /// # Notes
    ///
    /// Note that if reclaimers initialize guards across threads, objects
    /// retired through those guards may not be reclaimed.
    pub unsafe fn reclaim_all(&self, guard: &Guard) {
        unsafe { self.raw.reclaim_all(guard) };
    }
}

impl From<Arc<internal::Global>> for Collector {
    fn from(raw: Arc<internal::Global>) -> Self {
        Self { raw }
    }
}

impl Eq for Collector {}

impl PartialEq for Collector {
    /// Checks if both references point to the same collector.
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.raw.id == other.raw.id
    }
}

impl From<Collector> for Arc<internal::Global> {
    fn from(collector: Collector) -> Self {
        collector.raw
    }
}

impl fmt::Debug for Collector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Collector")
            .field("batch_size", &self.raw.batch_size)
            .finish()
    }
}

/// A handle to a registered `Local`.
///
/// This handle can be cloned and shared. When all handles are dropped,
/// the `Local` is marked for deletion.
pub struct LocalHandle {
    pub(crate) local: *const Local,
}

impl LocalHandle {
    /// Pin the `Local`, returning a guard that protects loads.
    #[inline]
    pub fn pin(&self) -> Guard {
        unsafe { (*self.local).pin() }
    }

    /// Returns true if this `Local` is currently pinned.
    #[inline]
    pub fn is_pinned(&self) -> bool {
        unsafe { (*self.local).is_pinned() }
    }

    /// Returns a reference to the underlying `Local`.
    ///
    /// # Safety
    ///
    /// The caller must ensure the `LocalHandle` is still valid.
    #[inline]
    pub unsafe fn local(&self) -> &Local {
        unsafe { &*self.local }
    }
}

impl Deref for LocalHandle {
    type Target = Local;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.local }
    }
}

impl Clone for LocalHandle {
    fn clone(&self) -> Self {
        unsafe {
            let local = &*self.local;
            local.handle_count.set(local.handle_count.get() + 1);
        }
        LocalHandle { local: self.local }
    }
}

impl Drop for LocalHandle {
    fn drop(&mut self) {
        unsafe {
            let local = &*self.local;
            let count = local.handle_count.get();
            local.handle_count.set(count - 1);

            if count == 1 {
                // Last handle - mark the Local for deletion.
                local.entry.delete(unprotected());
                let old = local
                    .collector()
                    .raw
                    .local_count
                    .fetch_sub(1, Ordering::Relaxed);
                if old == 1 {
                    // Last Local - reclaim all.
                    local.collector().raw.reclaim_all(unprotected());
                }
            }
        }
    }
}
