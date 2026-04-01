use core::{
    cell::UnsafeCell,
    fmt::{Debug, Display},
    marker::PhantomData,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicBool, Ordering},
};

use libakarin_machine_core::sync::DroppableScopedGuard;

/// Spin lock implementation.
///
/// # Generics
///
/// - `T`: The type of data to be protected by the spin lock.
/// - `S`: A Irq Save scope guard type that implements `DroppableScopedGuard`.
pub struct SpinLock<T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    lock: AtomicBool,
    _marker: PhantomData<S>,
    data: UnsafeCell<T>,
}

impl<T, S> SpinLock<T, S>
where
    S: DroppableScopedGuard,
{
    /// Creates a new `SpinLock` instance wrapping the given data.
    #[inline]
    pub const fn new(data: T) -> Self {
        Self {
            lock: AtomicBool::new(false),
            _marker: PhantomData,
            data: UnsafeCell::new(data),
        }
    }

    #[inline]
    pub fn try_lock_inner(&self, scope: S) -> Result<SpinLockGuard<'_, T, S>, S> {
        if self
            .lock
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            Ok(SpinLockGuard {
                lock: &self.lock,
                data: self.data.get(),
                _scope: scope,
            })
        } else {
            Err(scope)
        }
    }

    /// Lock the spin lock, blocking until it is acquired.
    #[inline]
    pub fn lock<'a>(&'a self) -> SpinLockGuard<'a, T, S> {
        let mut scope = S::enter();

        loop {
            match self.try_lock_inner(scope) {
                Ok(lock) => return lock,
                Err(ret_scope) => scope = ret_scope,
            }

            core::hint::spin_loop();
        }
    }

    /// Lock the spin lock and execute a closure with the lock held.
    #[inline]
    pub fn with_lock<'a, F, R>(&'a self, f: F) -> R
    where
        F: FnOnce(SpinLockGuard<'a, T, S>) -> R,
    {
        f(self.lock())
    }

    /// Try to lock the spin lock without blocking.
    #[inline]
    pub fn try_lock<'a>(&'a self) -> Option<SpinLockGuard<'a, T, S>> {
        let scope = S::enter();

        self.try_lock_inner(scope).ok()
    }

    /// Force unlock the spin lock.
    ///
    /// # Safety
    ///
    /// This function is unsafe because it can lead to data races if used
    /// improperly.
    #[inline]
    pub unsafe fn force_unlock(&self) {
        self.lock.store(false, Ordering::Release);
    }
}

pub struct SpinLockGuard<'a, T, S>
where
    T: ?Sized + 'a,
    S: DroppableScopedGuard,
{
    lock: &'a AtomicBool,
    data: *mut T,
    _scope: S,
}

impl<T, S> Deref for SpinLockGuard<'_, T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.data }
    }
}

impl<T, S> DerefMut for SpinLockGuard<'_, T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.data }
    }
}

impl<T, S> Debug for SpinLockGuard<'_, T, S>
where
    T: ?Sized + Debug,
    S: DroppableScopedGuard,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        Debug::fmt(&**self, f)
    }
}

impl<T, S> Display for SpinLockGuard<'_, T, S>
where
    T: ?Sized + Display,
    S: DroppableScopedGuard,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        Display::fmt(&**self, f)
    }
}

impl<T, S> Drop for SpinLockGuard<'_, T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    fn drop(&mut self) {
        self.lock.store(false, Ordering::Release);
    }
}

unsafe impl<T: ?Sized + Send, S: DroppableScopedGuard> Send for SpinLock<T, S> {}
unsafe impl<T: ?Sized + Send, S: DroppableScopedGuard> Sync for SpinLock<T, S> {}

unsafe impl<T: ?Sized + Send, S: DroppableScopedGuard> Send for SpinLockGuard<'_, T, S> {}
unsafe impl<T: ?Sized + Sync, S: DroppableScopedGuard> Sync for SpinLockGuard<'_, T, S> {}
