use core::{
    cell::UnsafeCell,
    fmt::{Debug, Display},
    marker::PhantomData,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicUsize, Ordering},
};

use libakarin_machine_core::sync::DroppableScopedGuard;

const READER: usize = 1 << 1;
const WRITER: usize = 1;

const MAX_READERS: usize = usize::MAX >> 2;

/// Spin read-write lock implementation.
///
/// # Generics
///
/// - `T`: The type of data to be protected by the spin read-write lock.
/// - `S`: A Irq Save scope guard type that implements `DroppableScopedGuard`.
pub struct SpinRwLock<T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    lock: AtomicUsize,
    _marker: PhantomData<S>,
    data: UnsafeCell<T>,
}

impl<T, S> SpinRwLock<T, S>
where
    S: DroppableScopedGuard,
{
    /// Creates a new `SpinRwLock` instance wrapping the given data.
    #[inline]
    pub const fn new(data: T) -> Self {
        Self {
            lock: AtomicUsize::new(0),
            _marker: PhantomData,
            data: UnsafeCell::new(data),
        }
    }

    #[inline]
    fn add_reader(&self) -> usize {
        let v = self.lock.fetch_add(READER, Ordering::AcqRel);

        if v > MAX_READERS * READER {
            self.lock.fetch_sub(READER, Ordering::AcqRel);
            panic!("Maximum number of readers exceeded");
        } else {
            v
        }
    }

    #[inline]
    fn try_read_inner(&self, scope: S) -> Result<SpinRwLockReadGuard<'_, T, S>, S> {
        let v = self.add_reader();

        if v & WRITER != 0 {
            self.lock.fetch_sub(READER, Ordering::AcqRel);
            Err(scope)
        } else {
            Ok(SpinRwLockReadGuard {
                inner: self,
                data: self.data.get(),
                _scope: Some(scope),
            })
        }
    }

    /// Lock the spin read-write lock for reading, blocking until it is
    /// acquired.
    #[inline]
    pub fn read(&self) -> SpinRwLockReadGuard<'_, T, S> {
        let mut scope = S::enter();

        loop {
            match self.try_read_inner(scope) {
                Ok(lock) => return lock,
                Err(ret_scope) => scope = ret_scope,
            }

            core::hint::spin_loop();
        }
    }

    /// Try to lock the spin read-write lock for reading.
    #[inline]
    pub fn try_read(&self) -> Option<SpinRwLockReadGuard<'_, T, S>> {
        let scope = S::enter();

        self.try_read_inner(scope).ok()
    }

    #[inline]
    fn try_write_inner(&self, scope: S) -> Result<SpinRwLockWriteGuard<'_, T, S>, S> {
        if self
            .lock
            .compare_exchange(0, WRITER, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            Ok(SpinRwLockWriteGuard {
                inner: self,
                data: self.data.get(),
                _scope: Some(scope),
            })
        } else {
            Err(scope)
        }
    }

    /// Lock the spin read-write lock for writing, blocking until it is
    /// acquired.
    #[inline]
    pub fn write(&self) -> SpinRwLockWriteGuard<'_, T, S> {
        let mut scope = S::enter();

        loop {
            match self.try_write_inner(scope) {
                Ok(lock) => return lock,
                Err(ret_scope) => scope = ret_scope,
            }

            core::hint::spin_loop();
        }
    }

    /// Try to lock the spin read-write lock for writing.
    #[inline]
    pub fn try_write(&self) -> Option<SpinRwLockWriteGuard<'_, T, S>> {
        let scope = S::enter();

        self.try_write_inner(scope).ok()
    }

    /// Forcefully unlock the spin read-write lock.
    ///
    /// # Safety
    ///
    /// This function is unsafe because it can lead to data races if used
    /// improperly.
    #[inline]
    pub unsafe fn force_unlock(&self) {
        self.lock.store(0, Ordering::Release);
    }
}

pub struct SpinRwLockReadGuard<'a, T, S>
where
    T: 'a + ?Sized,
    S: DroppableScopedGuard,
{
    inner: &'a SpinRwLock<T, S>,
    data: *const T,
    _scope: Option<S>,
}

impl<'a, T, S> SpinRwLockReadGuard<'a, T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    /// Try to upgrade the read guard to a write guard.
    #[inline]
    pub fn try_upgrade(mut self) -> Result<SpinRwLockWriteGuard<'a, T, S>, Self> {
        if self
            .inner
            .lock
            .compare_exchange(READER, WRITER, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            Ok(SpinRwLockWriteGuard {
                inner: self.inner,
                data: self.data as _,
                _scope: self._scope.take(),
            })
        } else {
            Err(self)
        }
    }

    /// Upgrade the read guard to a write guard, blocking until it is acquired.
    #[inline]
    pub fn upgrade(mut self) -> SpinRwLockWriteGuard<'a, T, S> {
        loop {
            match self.try_upgrade() {
                Ok(lock) => return lock,
                Err(ret_self) => self = ret_self,
            }

            core::hint::spin_loop();
        }
    }
}

impl<T, S> Drop for SpinRwLockReadGuard<'_, T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    fn drop(&mut self) {
        self.inner.lock.fetch_sub(READER, Ordering::AcqRel);
    }
}

impl<T, S> Deref for SpinRwLockReadGuard<'_, T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.data }
    }
}

impl<T, S> Display for SpinRwLockReadGuard<'_, T, S>
where
    T: ?Sized + Display,
    S: DroppableScopedGuard,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        Display::fmt(&**self, f)
    }
}

impl<T, S> Debug for SpinRwLockReadGuard<'_, T, S>
where
    T: ?Sized + Debug,
    S: DroppableScopedGuard,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        Debug::fmt(&**self, f)
    }
}

pub struct SpinRwLockWriteGuard<'a, T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    inner: &'a SpinRwLock<T, S>,
    data: *mut T,
    _scope: Option<S>,
}

impl<'a, T, S> SpinRwLockWriteGuard<'a, T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    /// Downgrade the write guard to a read guard.
    #[inline]
    pub fn downgrade(mut self) -> SpinRwLockReadGuard<'a, T, S> {
        self.inner.lock.store(READER, Ordering::Release);

        SpinRwLockReadGuard {
            inner: self.inner,
            data: self.data,
            _scope: self._scope.take(),
        }
    }
}

impl<T, S> Deref for SpinRwLockWriteGuard<'_, T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.data }
    }
}

impl<T, S> DerefMut for SpinRwLockWriteGuard<'_, T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.data }
    }
}

impl<T, S> Display for SpinRwLockWriteGuard<'_, T, S>
where
    T: ?Sized + Display,
    S: DroppableScopedGuard,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        Display::fmt(&**self, f)
    }
}

impl<T, S> Debug for SpinRwLockWriteGuard<'_, T, S>
where
    T: ?Sized + Debug,
    S: DroppableScopedGuard,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        Debug::fmt(&**self, f)
    }
}

impl<T, S> Drop for SpinRwLockWriteGuard<'_, T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    fn drop(&mut self) {
        self.inner.lock.fetch_and(!WRITER, Ordering::AcqRel);
    }
}

// Same unsafe impls as `std::sync::RwLock`
unsafe impl<T: ?Sized + Send, S: DroppableScopedGuard> Send for SpinRwLock<T, S> {}
unsafe impl<T: ?Sized + Send + Sync, S: DroppableScopedGuard> Sync for SpinRwLock<T, S> {}
unsafe impl<T: ?Sized + Sync, S: DroppableScopedGuard> Send for SpinRwLockReadGuard<'_, T, S> {}
unsafe impl<T: ?Sized + Sync, S: DroppableScopedGuard> Sync for SpinRwLockReadGuard<'_, T, S> {}
unsafe impl<T: ?Sized + Send + Sync, S: DroppableScopedGuard> Send
    for SpinRwLockWriteGuard<'_, T, S>
{
}
unsafe impl<T: ?Sized + Send + Sync, S: DroppableScopedGuard> Sync
    for SpinRwLockWriteGuard<'_, T, S>
{
}
