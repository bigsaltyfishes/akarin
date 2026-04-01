//! Guards for protected access to concurrent objects.

use core::{
    fmt,
    sync::atomic::{AtomicPtr, Ordering},
};

use crate::gc::{
    Collector,
    internal::{self, Local},
};

/// A guard that protects loads of concurrent objects.
///
/// A `Guard` is created by calling
/// [`LocalHandle::pin`](crate::gc::LocalHandle::pin). While a guard is held,
/// any pointers loaded through protected operations are guaranteed to remain
/// valid.
///
/// # Examples
///
/// ```rust
/// # use std::sync::atomic::{AtomicPtr, Ordering};
/// use libakarin_sync::gc::Collector;
///
/// let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
/// let collector = Collector::new(cpus);
/// let handle = collector.register();
///
/// // Create a guard that protects loads.
/// let guard = handle.pin();
///
/// // The guard will be dropped and memory may be reclaimed.
/// drop(guard);
/// ```
pub struct Guard {
    /// Reference to the Local that owns this guard.
    local: *const Local,
}

impl Guard {
    /// Creates a new guard for the given Local.
    ///
    /// This is called internally by `Local::pin()`.
    #[inline]
    pub(crate) fn new(local: *const Local) -> Guard {
        Guard { local }
    }

    #[inline]
    fn local(&self) -> Option<&Local> {
        // Safety: The guard holds a valid reference to the Local for its lifetime.
        if self.local.is_null() {
            None
        } else {
            Some(unsafe { &*self.local })
        }
    }

    /// Refreshes the guard.
    ///
    /// Calling this method is similar to dropping and immediately creating a
    /// new guard. The current thread remains active, but any pointers that
    /// were previously protected may be reclaimed.
    ///
    /// # Safety
    ///
    /// This method is not marked as `unsafe`, but will affect the validity of
    /// pointers loaded using [`Guard::protect`], similar to dropping a guard.
    /// It is intended to be used safely by users of concurrent data structures,
    /// as references will be tied to the guard and this method takes `&mut
    /// self`.
    #[inline]
    pub fn refresh(&mut self) {
        // Safety: We have &mut self, so we're the only guard.
        if let Some(local) = self.local() {
            unsafe { local.refresh() };
        }
    }

    /// Flush any retired values in the local batch.
    ///
    /// This method flushes any values from the current thread's local batch,
    /// starting the reclamation process. Note that no memory can be
    /// reclaimed while this guard is active, but calling `flush` may allow
    /// memory to be reclaimed more quickly after the guard is dropped.
    ///
    /// Note that the batch must contain at least as many objects as the number
    /// of currently active threads for a flush to be performed.
    #[inline]
    pub fn flush(&self) {
        // Note that this does not actually retire any values, it just attempts to add
        // the batch to any active reservations lists, including ours.
        //
        // Safety: We have a guard for this Local.
        if let Some(local) = self.local() {
            unsafe { local.try_retire_batch(self) };
        }
    }

    /// Returns the collector this guard was created from.
    #[inline]
    pub fn collector(&self) -> Option<&Collector> {
        if let Some(local) = self.local() {
            Some(local.collector())
        } else {
            None
        }
    }

    /// Protects the load of an atomic pointer.
    ///
    /// Any valid pointer loaded through a guard using the `protect` method is
    /// guaranteed to stay valid until the guard is dropped, or the object
    /// is retired by the current thread. Importantly, if another thread
    /// retires this object, it will not be reclaimed for the lifetime of
    /// this guard.
    ///
    /// Note that the lifetime of a guarded pointer is logically tied to that of
    /// the guard — when the guard is dropped the pointer is invalidated. Data
    /// structures that return shared references to values should ensure that
    /// the lifetime of the reference is tied to the lifetime of a guard.
    #[inline]
    pub fn protect<T>(&self, ptr: &AtomicPtr<T>, order: Ordering) -> *mut T {
        ptr.load(internal::Global::protect(order))
    }

    /// Stores a value into the pointer, returning the protected previous value.
    ///
    /// This method is equivalent to [`AtomicPtr::swap`], except the returned
    /// value is guaranteed to be protected with the same guarantees as
    /// [`Guard::protect`].
    #[inline]
    pub fn swap<T>(&self, ptr: &AtomicPtr<T>, value: *mut T, order: Ordering) -> *mut T {
        ptr.swap(value, internal::Global::protect(order))
    }

    /// Stores a value into the pointer if the current value is the same as the
    /// `current` value, returning the protected previous value.
    ///
    /// This method is equivalent to [`AtomicPtr::compare_exchange`], except the
    /// returned value is guaranteed to be protected with the same
    /// guarantees as [`Guard::protect`].
    #[inline]
    pub fn compare_exchange<T>(
        &self,
        ptr: &AtomicPtr<T>,
        current: *mut T,
        new: *mut T,
        success: Ordering,
        failure: Ordering,
    ) -> Result<*mut T, *mut T> {
        ptr.compare_exchange(
            current,
            new,
            internal::Global::protect(success),
            internal::Global::protect(failure),
        )
    }

    /// Stores a value into the pointer if the current value is the same as the
    /// `current` value, returning the protected previous value.
    ///
    /// This method is equivalent to [`AtomicPtr::compare_exchange_weak`],
    /// except the returned value is guaranteed to be protected with the
    /// same guarantees as [`Guard::protect`].
    #[inline]
    pub fn compare_exchange_weak<T>(
        &self,
        ptr: &AtomicPtr<T>,
        current: *mut T,
        new: *mut T,
        success: Ordering,
        failure: Ordering,
    ) -> Result<*mut T, *mut T> {
        ptr.compare_exchange_weak(
            current,
            new,
            internal::Global::protect(success),
            internal::Global::protect(failure),
        )
    }

    /// Unpins and then immediately re-pins the thread.
    ///
    /// This method is useful when you don't want delay the reclamation of
    /// retired objects any longer than necessary.
    ///
    /// If this method is called from an [`unprotected`] guard, then the call
    /// will be just no-op.
    #[inline]
    pub fn repin(&self) {
        if let Some(local) = self.local() {
            local.repin();
        }
    }

    /// Retires a value, running `reclaim` when no threads hold a reference to
    /// it.
    ///
    /// This method delays reclamation until the guard is dropped, as opposed to
    /// immediate reclamation.
    ///
    /// # Safety
    ///
    /// The retired pointer must no longer be accessible to any thread that
    /// enters after it is removed. Additionally, the pointer must be valid
    /// to pass to the provided reclaimer, once it is safe to reclaim.
    #[inline]
    pub unsafe fn defer_retire<T>(&self, ptr: *mut T, reclaim: unsafe fn(*mut T, Option<&Local>)) {
        // Safety: Guaranteed by caller.
        if let Some(local) = self.local() {
            unsafe { local.retire(ptr, self, reclaim) };
        } else {
            // Safety: No threads are active, so we can reclaim immediately.
            unsafe { reclaim(ptr, None) };
        }
    }
}

impl Drop for Guard {
    #[inline]
    fn drop(&mut self) {
        if let Some(local) = self.local() {
            local.unpin();
        }
    }
}

impl fmt::Debug for Guard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Guard").finish()
    }
}

/// Safety: The returned guard has no associated Local, so no threads are
/// active.
#[inline]
pub unsafe fn unprotected() -> &'static Guard {
    // An unprotected guard is just a `Guard` with its field `local` set to null.
    // We make a newtype over `Guard` because `Guard` isn't `Sync`, so can't be
    // directly stored in a `static`
    struct GuardWrapper(Guard);
    unsafe impl Sync for GuardWrapper {}
    static UNPROTECTED: GuardWrapper = GuardWrapper(Guard {
        local: core::ptr::null(),
    });
    &UNPROTECTED.0
}
