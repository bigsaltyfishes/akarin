use core::{
    cell::UnsafeCell,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicU32, Ordering},
};

use libakarin_core::clock::time::Duration;
use libakarin_syscall::{FutexWaitFlags, FutexWakeFlags, SyscallStatus, errno::FutexError};

use crate::syscall::{RawSyscallInvoker, SyscallFailure};

/// One userspace futex syscall failure decoded into stable runtime semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FutexCallError {
    Object(SyscallFailure),
    Underlying(FutexError),
}

impl FutexCallError {
    /// Convert one raw syscall failure into the futex-specific error domain.
    pub fn from_failure(error: SyscallFailure) -> Self {
        match error.status() {
            SyscallStatus::UnderlyingError => match FutexError::try_from(error.detail()) {
                Ok(error) => Self::Underlying(error),
                Err(_) => Self::Object(error),
            },
            _ => Self::Object(error),
        }
    }
}

/// Thin runtime wrapper around the futex wait/wake syscalls.
pub struct Futex;

impl Futex {
    /// Sleep while the supplied futex word remains equal to
    /// `expected`.
    pub fn wait(
        word: &AtomicU32,
        expected: u32,
        timeout: Option<Duration>,
    ) -> Result<(), FutexCallError> {
        let invoker = RawSyscallInvoker;
        let timeout_ns = timeout
            .map(|duration| duration.as_nanos().min(usize::MAX as u128) as usize)
            .unwrap_or(usize::MAX);
        invoker
            .futex_wait(
                word as *const AtomicU32 as *const u32 as usize,
                expected,
                timeout_ns,
                FutexWaitFlags::NONE,
            )
            .map_err(FutexCallError::from_failure)
    }

    /// Wake up to `count` waiters sleeping on the supplied futex
    /// word.
    pub fn wake(word: &AtomicU32, count: usize) -> Result<usize, FutexCallError> {
        let invoker = RawSyscallInvoker;
        invoker
            .futex_wake(
                word as *const AtomicU32 as *const u32 as usize,
                count,
                FutexWakeFlags::NONE,
            )
            .map_err(FutexCallError::from_failure)
    }
}

/// Blocking mutex backed by one futex word.
///
/// State machine:
/// - `0`: unlocked
/// - `1`: locked, no known waiter
/// - `2`: locked, contended
pub struct FutexMutex<T> {
    state: AtomicU32,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for FutexMutex<T> {}
unsafe impl<T: Send> Sync for FutexMutex<T> {}

impl<T> FutexMutex<T> {
    /// Construct one unlocked futex-backed mutex.
    pub const fn new(value: T) -> Self {
        Self {
            state: AtomicU32::new(0),
            value: UnsafeCell::new(value),
        }
    }

    /// Acquire the mutex, blocking in the kernel if the lock stays contended.
    pub fn lock(&self) -> Result<FutexMutexGuard<'_, T>, FutexCallError> {
        if self
            .state
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return Ok(FutexMutexGuard { mutex: self });
        }

        loop {
            if self.state.swap(2, Ordering::Acquire) == 0 {
                return Ok(FutexMutexGuard { mutex: self });
            }

            match Futex::wait(&self.state, 2, None) {
                Ok(()) => {}
                Err(FutexCallError::Underlying(FutexError::WouldBlock)) => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

/// Guard returned by [`FutexMutex::lock`].
pub struct FutexMutexGuard<'a, T> {
    mutex: &'a FutexMutex<T>,
}

impl<T> Deref for FutexMutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T> DerefMut for FutexMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T> Drop for FutexMutexGuard<'_, T> {
    fn drop(&mut self) {
        if self.mutex.state.fetch_sub(1, Ordering::Release) != 1 {
            self.mutex.state.store(0, Ordering::Release);
            let _ = Futex::wake(&self.mutex.state, 1);
        }
    }
}
