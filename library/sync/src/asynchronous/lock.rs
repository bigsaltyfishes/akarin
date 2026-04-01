//! Async synchronization primitives adapted for kernel use.
//!
//! Source references:
//! - https://github.com/smol-rs/async-lock.git
//! - https://github.com/smol-rs/event-listener.git

use core::{
    cell::UnsafeCell,
    future::Future,
    mem::MaybeUninit,
    ops::{Deref, DerefMut},
    pin::Pin,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::{Context, Poll},
};

use libakarin_machine_core::sync::{DroppableScopedGuard, NoOp, ScopedGuard};

use crate::asynchronous::event::{Event, Listener};

/// Asynchronous mutual exclusion primitive.
///
/// This lock mirrors the shape of `async-lock::Mutex` while keeping kernel
/// reentrancy protection through `S: DroppableScopedGuard` in its guards.
pub struct Mutex<T, S = ScopedGuard<NoOp>>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    _marker: core::marker::PhantomData<S>,
    locked: AtomicBool,
    waiters: Event,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send + ?Sized, S: DroppableScopedGuard> Send for Mutex<T, S> {}
unsafe impl<T: Send + ?Sized, S: DroppableScopedGuard> Sync for Mutex<T, S> {}

impl<T, S> Mutex<T, S>
where
    S: DroppableScopedGuard,
{
    /// Create one mutex initialized with `value`.
    pub fn new(value: T) -> Self {
        Self {
            _marker: core::marker::PhantomData,
            locked: AtomicBool::new(false),
            waiters: Event::new(),
            data: UnsafeCell::new(value),
        }
    }

    /// Consume the mutex and return the protected value.
    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }

    /// Borrow the protected value mutably without locking.
    ///
    /// This requires `&mut self`, so no concurrent access can exist.
    pub fn get_mut(&mut self) -> &mut T {
        self.data.get_mut()
    }
}

impl<T, S> Mutex<T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    fn lock_fast(&self) -> bool {
        self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Acquire)
            .is_ok()
    }

    fn try_lock_inner(&self, scope: S) -> Result<MutexGuard<'_, T, S>, S> {
        if self.lock_fast() {
            Ok(MutexGuard {
                mutex: self,
                _scope: scope,
            })
        } else {
            Err(scope)
        }
    }

    /// Try to acquire the mutex without blocking.
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T, S>> {
        self.try_lock_inner(S::enter()).ok()
    }

    /// Return one future that resolves when the mutex is acquired.
    pub fn lock(&self) -> Lock<'_, T, S> {
        Lock {
            mutex: self,
            listener: None,
        }
    }

    fn unlock(&self) {
        self.locked.store(false, Ordering::Release);
        self.waiters.notify(1);
    }
}

/// Future returned by [`Mutex::lock`].
pub struct Lock<'a, T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    mutex: &'a Mutex<T, S>,
    listener: Option<Listener<'a>>,
}

impl<'a, T, S> Lock<'a, T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    /// Wait for the mutex to be acquired in a blocking manner.
    pub fn wait(&self) -> MutexGuard<'a, T, S> {
        loop {
            if let Some(guard) = self.mutex.try_lock() {
                return guard;
            }

            core::hint::spin_loop();
        }
    }
}

impl<'a, T, S> Future for Lock<'a, T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    type Output = MutexGuard<'a, T, S>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            // Re-check fast path first to avoid registering waiters unless
            // the lock is actually contended.
            let scope = S::enter();
            if let Ok(guard) = this.mutex.try_lock_inner(scope) {
                return Poll::Ready(guard);
            }

            if this.listener.is_none() {
                this.listener = Some(this.mutex.waiters.listen());
            }

            if let Some(listener) = this.listener.as_mut() {
                if Pin::new(listener).poll(cx).is_pending() {
                    return Poll::Pending;
                }
                this.listener = None;
            }
        }
    }
}

/// RAII guard returned by [`Mutex::try_lock`] and [`Mutex::lock`].
pub struct MutexGuard<'a, T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    mutex: &'a Mutex<T, S>,
    _scope: S,
}

impl<T, S> Deref for MutexGuard<'_, T, S>
where
    S: DroppableScopedGuard,
    T: ?Sized,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T, S> DerefMut for MutexGuard<'_, T, S>
where
    S: DroppableScopedGuard,
    T: ?Sized,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<T, S> Drop for MutexGuard<'_, T, S>
where
    S: DroppableScopedGuard,
    T: ?Sized,
{
    fn drop(&mut self) {
        self.mutex.unlock();
    }
}

/// Asynchronous reader-writer lock.
///
/// Readers are blocked when one writer is queued to avoid writer starvation.
pub struct RwLock<T, S = ScopedGuard<NoOp>>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    _marker: core::marker::PhantomData<S>,
    state: AtomicUsize,
    waiting_writers: AtomicUsize,
    read_waiters: Event,
    write_waiters: Event,
    data: UnsafeCell<T>,
}

const RWLOCK_WRITER: usize = 1;
const RWLOCK_READER_INC: usize = 1 << 1;

unsafe impl<T: Send + ?Sized, S: DroppableScopedGuard> Send for RwLock<T, S> {}
unsafe impl<T: Send + Sync + ?Sized, S: DroppableScopedGuard> Sync for RwLock<T, S> {}

impl<T, S> RwLock<T, S>
where
    S: DroppableScopedGuard,
{
    /// Create one read-write lock initialized with `value`.
    pub fn new(value: T) -> Self {
        Self {
            _marker: core::marker::PhantomData,
            state: AtomicUsize::new(0),
            waiting_writers: AtomicUsize::new(0),
            read_waiters: Event::new(),
            write_waiters: Event::new(),
            data: UnsafeCell::new(value),
        }
    }

    /// Consume the lock and return the protected value.
    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }

    /// Borrow the protected value mutably without locking.
    ///
    /// This requires `&mut self`, so no concurrent access can exist.
    pub fn get_mut(&mut self) -> &mut T {
        self.data.get_mut()
    }
}

impl<T, S> RwLock<T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    fn try_read_fast(&self) -> bool {
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            // Writer bit set or pending writers => block new readers.
            if (current & RWLOCK_WRITER) != 0 || self.waiting_writers.load(Ordering::Acquire) > 0 {
                return false;
            }
            match self.state.compare_exchange_weak(
                current,
                current + RWLOCK_READER_INC,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(next) => current = next,
            }
        }
    }

    fn try_write_fast(&self) -> bool {
        self.state
            .compare_exchange(0, RWLOCK_WRITER, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn release_reader(&self) {
        let prev = self.state.fetch_sub(RWLOCK_READER_INC, Ordering::AcqRel);
        let readers_after = (prev - RWLOCK_READER_INC) >> 1;
        // Last reader gives priority to queued writers.
        if readers_after == 0 && self.waiting_writers.load(Ordering::Acquire) > 0 {
            self.write_waiters.notify(1);
        } else if self.waiting_writers.load(Ordering::Acquire) == 0 {
            self.read_waiters.notify_all();
        }
    }

    fn release_writer(&self) {
        self.state.fetch_and(!RWLOCK_WRITER, Ordering::AcqRel);
        // Keep writer preference when writers are queued, otherwise wake readers.
        if self.waiting_writers.load(Ordering::Acquire) > 0 {
            self.write_waiters.notify(1);
        } else {
            self.read_waiters.notify_all();
        }
    }

    fn try_read_inner(&self, scope: S) -> Result<RwLockReadGuard<'_, T, S>, S> {
        if self.try_read_fast() {
            Ok(RwLockReadGuard {
                rwlock: self,
                _scope: scope,
            })
        } else {
            Err(scope)
        }
    }

    fn try_write_inner(&self, scope: S) -> Result<RwLockWriteGuard<'_, T, S>, S> {
        if self.try_write_fast() {
            Ok(RwLockWriteGuard {
                rwlock: self,
                _scope: scope,
            })
        } else {
            Err(scope)
        }
    }

    /// Try to acquire one read guard without blocking.
    pub fn try_read(&self) -> Option<RwLockReadGuard<'_, T, S>> {
        self.try_read_inner(S::enter()).ok()
    }

    /// Try to acquire one write guard without blocking.
    pub fn try_write(&self) -> Option<RwLockWriteGuard<'_, T, S>> {
        self.try_write_inner(S::enter()).ok()
    }

    /// Return one future that resolves to a read guard.
    pub fn read(&self) -> Read<'_, T, S> {
        Read {
            rwlock: self,
            listener: None,
        }
    }

    /// Return one future that resolves to a write guard.
    pub fn write(&self) -> Write<'_, T, S> {
        Write {
            rwlock: self,
            queued: false,
            listener: None,
        }
    }
}

/// Shared read guard returned by [`RwLock::try_read`] and [`RwLock::read`].
pub struct RwLockReadGuard<'a, T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    rwlock: &'a RwLock<T, S>,
    _scope: S,
}

impl<T, S> Deref for RwLockReadGuard<'_, T, S>
where
    S: DroppableScopedGuard,
    T: ?Sized,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.rwlock.data.get() }
    }
}

impl<T, S> Drop for RwLockReadGuard<'_, T, S>
where
    S: DroppableScopedGuard,
    T: ?Sized,
{
    fn drop(&mut self) {
        self.rwlock.release_reader();
    }
}

/// Exclusive write guard returned by [`RwLock::try_write`] and
/// [`RwLock::write`].
pub struct RwLockWriteGuard<'a, T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    rwlock: &'a RwLock<T, S>,
    _scope: S,
}

impl<T, S> Deref for RwLockWriteGuard<'_, T, S>
where
    S: DroppableScopedGuard,
    T: ?Sized,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.rwlock.data.get() }
    }
}

impl<T, S> DerefMut for RwLockWriteGuard<'_, T, S>
where
    S: DroppableScopedGuard,
    T: ?Sized,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.rwlock.data.get() }
    }
}

impl<T, S> Drop for RwLockWriteGuard<'_, T, S>
where
    S: DroppableScopedGuard,
    T: ?Sized,
{
    fn drop(&mut self) {
        self.rwlock.release_writer();
    }
}

/// Future returned by [`RwLock::read`].
pub struct Read<'a, T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    rwlock: &'a RwLock<T, S>,
    listener: Option<Listener<'a>>,
}

impl<'a, T, S> Read<'a, T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    /// Wait for the read guard to be acquired in a blocking manner.
    pub fn wait(&self) -> RwLockReadGuard<'a, T, S> {
        loop {
            if let Some(guard) = self.rwlock.try_read() {
                return guard;
            }

            core::hint::spin_loop();
        }
    }
}

impl<'a, T, S> Future for Read<'a, T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    type Output = RwLockReadGuard<'a, T, S>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            // Keep read fast path ahead of waiter registration.
            let scope = S::enter();
            if let Ok(guard) = this.rwlock.try_read_inner(scope) {
                return Poll::Ready(guard);
            }

            if this.listener.is_none() {
                this.listener = Some(this.rwlock.read_waiters.listen());
            }

            if let Some(listener) = this.listener.as_mut() {
                if Pin::new(listener).poll(cx).is_pending() {
                    return Poll::Pending;
                }
                this.listener = None;
            }
        }
    }
}

/// Future returned by [`RwLock::write`].
pub struct Write<'a, T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    rwlock: &'a RwLock<T, S>,
    queued: bool,
    listener: Option<Listener<'a>>,
}

impl<'a, T, S> Write<'a, T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    /// Wait for the write guard to be acquired in a blocking manner.
    pub fn wait(&self) -> RwLockWriteGuard<'a, T, S> {
        self.rwlock.waiting_writers.fetch_add(1, Ordering::AcqRel);

        loop {
            if let Some(guard) = self.rwlock.try_write() {
                self.rwlock.waiting_writers.fetch_sub(1, Ordering::AcqRel);
                return guard;
            }

            core::hint::spin_loop();
        }
    }
}

impl<'a, T, S> Future for Write<'a, T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    type Output = RwLockWriteGuard<'a, T, S>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // Queue once to make writer preference observable to incoming readers.
        if !this.queued {
            this.rwlock.waiting_writers.fetch_add(1, Ordering::AcqRel);
            this.queued = true;
        }

        loop {
            let scope = S::enter();
            if let Ok(guard) = this.rwlock.try_write_inner(scope) {
                if this.queued {
                    this.rwlock.waiting_writers.fetch_sub(1, Ordering::AcqRel);
                    this.queued = false;
                }
                return Poll::Ready(guard);
            }

            if this.listener.is_none() {
                this.listener = Some(this.rwlock.write_waiters.listen());
            }

            if let Some(listener) = this.listener.as_mut() {
                if Pin::new(listener).poll(cx).is_pending() {
                    return Poll::Pending;
                }
                this.listener = None;
            }
        }
    }
}

impl<T, S> Drop for Write<'_, T, S>
where
    T: ?Sized,
    S: DroppableScopedGuard,
{
    fn drop(&mut self) {
        if self.queued {
            self.rwlock.waiting_writers.fetch_sub(1, Ordering::AcqRel);
            self.queued = false;
        }
    }
}

/// Counting semaphore implemented on top of atomic permits plus event waiters.
pub struct Semaphore<S>
where
    S: DroppableScopedGuard,
{
    count: AtomicUsize,
    event: Event,
    _marker: core::marker::PhantomData<S>,
}

impl<S> Semaphore<S>
where
    S: DroppableScopedGuard,
{
    /// Create one semaphore with `permits` initial units.
    pub fn new(permits: usize) -> Self {
        Self {
            count: AtomicUsize::new(permits),
            event: Event::new(),
            _marker: core::marker::PhantomData,
        }
    }

    fn try_acquire_inner(&self, scope: S) -> Result<SemaphoreGuard<'_, S>, S> {
        let mut current = self.count.load(Ordering::Acquire);
        loop {
            if current == 0 {
                return Err(scope);
            }
            match self.count.compare_exchange_weak(
                current,
                current - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(SemaphoreGuard {
                        sem: self,
                        held: true,
                        _scope: scope,
                    });
                }
                Err(next) => current = next,
            }
        }
    }

    /// Try to acquire one permit without blocking.
    pub fn try_acquire(&self) -> Option<SemaphoreGuard<'_, S>> {
        self.try_acquire_inner(S::enter()).ok()
    }

    /// Return one future that resolves after one permit is acquired.
    pub fn acquire(&self) -> Acquire<'_, S> {
        Acquire {
            sem: self,
            listener: None,
        }
    }

    /// Add `n` permits and notify up to `n` waiting tasks.
    pub fn add_permits(&self, n: usize) {
        if n == 0 {
            return;
        }
        self.count.fetch_add(n, Ordering::AcqRel);
        self.event.notify(n);
    }
}

/// Future returned by [`Semaphore::acquire`].
pub struct Acquire<'a, S>
where
    S: DroppableScopedGuard,
{
    sem: &'a Semaphore<S>,
    listener: Option<Listener<'a>>,
}

impl<'a, S> Acquire<'a, S>
where
    S: DroppableScopedGuard,
{
    /// Wait for one permit to be acquired in a blocking manner.
    pub fn wait(&self) -> SemaphoreGuard<'a, S> {
        loop {
            if let Some(guard) = self.sem.try_acquire() {
                return guard;
            }

            core::hint::spin_loop();
        }
    }
}

impl<'a, S> Future for Acquire<'a, S>
where
    S: DroppableScopedGuard,
{
    type Output = SemaphoreGuard<'a, S>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            // Attempt permit acquisition before subscribing to events.
            let scope = S::enter();
            if let Ok(guard) = this.sem.try_acquire_inner(scope) {
                return Poll::Ready(guard);
            }

            if this.listener.is_none() {
                this.listener = Some(this.sem.event.listen());
            }

            if let Some(listener) = this.listener.as_mut() {
                if Pin::new(listener).poll(cx).is_pending() {
                    return Poll::Pending;
                }
                this.listener = None;
            }
        }
    }
}

/// RAII permit guard returned by [`Semaphore::try_acquire`] and
/// [`Semaphore::acquire`].
pub struct SemaphoreGuard<'a, S>
where
    S: DroppableScopedGuard,
{
    sem: &'a Semaphore<S>,
    held: bool,
    _scope: S,
}

impl<S> SemaphoreGuard<'_, S>
where
    S: DroppableScopedGuard,
{
    /// Consume this guard without releasing one permit back to the semaphore.
    pub fn forget(mut self) {
        self.held = false;
    }
}

impl<S> Drop for SemaphoreGuard<'_, S>
where
    S: DroppableScopedGuard,
{
    fn drop(&mut self) {
        if self.held {
            self.sem.add_permits(1);
        }
    }
}

/// Reusable barrier for synchronizing a fixed number of tasks.
pub struct Barrier<S = ScopedGuard<NoOp>>
where
    S: DroppableScopedGuard,
{
    total: usize,
    arrived: AtomicUsize,
    generation: AtomicUsize,
    event: Event,
    _marker: core::marker::PhantomData<S>,
}

impl<S> Barrier<S>
where
    S: DroppableScopedGuard,
{
    /// Create one barrier that opens after `total` waiters arrive.
    pub fn new(total: usize) -> Self {
        Self {
            total,
            arrived: AtomicUsize::new(0),
            generation: AtomicUsize::new(0),
            event: Event::new(),
            _marker: core::marker::PhantomData,
        }
    }

    /// Return one future for the current barrier generation.
    pub fn wait(&self) -> BarrierWait<'_, S> {
        BarrierWait {
            barrier: self,
            observed_generation: self.generation.load(Ordering::Acquire),
            registered: false,
            listener: None,
        }
    }
}

/// Future returned by [`Barrier::wait`].
pub struct BarrierWait<'a, S>
where
    S: DroppableScopedGuard,
{
    barrier: &'a Barrier<S>,
    observed_generation: usize,
    registered: bool,
    listener: Option<Listener<'a>>,
}

/// Result of one barrier wait operation.
///
/// `true` means this waiter is the generation leader that completed the cycle.
pub struct BarrierWaitResult(pub bool);

impl BarrierWaitResult {
    /// Return whether this waiter released the barrier generation.
    pub const fn is_leader(&self) -> bool {
        self.0
    }
}

impl<S> Future for BarrierWait<'_, S>
where
    S: DroppableScopedGuard,
{
    type Output = BarrierWaitResult;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let generation = this.barrier.generation.load(Ordering::Acquire);
        if generation != this.observed_generation {
            return Poll::Ready(BarrierWaitResult(false));
        }

        if !this.registered {
            let previous = this.barrier.arrived.fetch_add(1, Ordering::AcqRel);
            this.registered = true;

            // Last arrival advances generation and wakes all waiters.
            if previous + 1 == this.barrier.total {
                this.barrier.arrived.store(0, Ordering::Release);
                this.barrier.generation.fetch_add(1, Ordering::AcqRel);
                this.barrier.event.notify_all();
                return Poll::Ready(BarrierWaitResult(true));
            }

            this.listener = Some(this.barrier.event.listen());
            if this.barrier.generation.load(Ordering::Acquire) != this.observed_generation {
                return Poll::Ready(BarrierWaitResult(false));
            }
        }

        if let Some(listener) = this.listener.as_mut() {
            if Pin::new(listener).poll(cx).is_pending() {
                return Poll::Pending;
            }
        }

        Poll::Ready(BarrierWaitResult(false))
    }
}
