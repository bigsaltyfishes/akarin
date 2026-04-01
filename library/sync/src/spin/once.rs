use core::{
    cell::UnsafeCell,
    fmt::Debug,
    marker::PhantomData,
    mem::MaybeUninit,
    sync::atomic::{AtomicUsize, Ordering},
};

use libakarin_machine_core::sync::DroppableScopedGuard;
use thiserror::Error;

const UNINITIALIZED: usize = 0;
const INITIALIZING: usize = 1;
const INITIALIZED: usize = 2;

#[derive(Error)]
pub enum TryInitError<T> {
    #[error("already initialized")]
    AlreadyInitialized(T),
    #[error("initialization in progress")]
    Initializing(T),
}

impl<T> Debug for TryInitError<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::AlreadyInitialized(_) => f.write_str("AlreadyInitialized"),
            Self::Initializing(_) => f.write_str("Initializing"),
        }
    }
}

/// A synchronization primitive which can be initialized only once.
///
/// # Generics
///
/// - `T`: The type of data to be initialized.
/// - `S`: A Irq Save scope guard type that implements `DroppableScopedGuard`.
#[repr(C)]
pub struct Once<T, S>
where
    S: DroppableScopedGuard,
{
    value: UnsafeCell<MaybeUninit<T>>,
    status: AtomicUsize,
    _marker: PhantomData<S>,
}

impl<T, S> Once<T, S>
where
    S: DroppableScopedGuard,
{
    #[inline]
    pub const fn new() -> Self {
        Self {
            value: UnsafeCell::new(MaybeUninit::uninit()),
            status: AtomicUsize::new(UNINITIALIZED),
            _marker: PhantomData,
        }
    }

    /// Returns a reference to the initialized value.
    ///
    /// # Panics
    ///
    /// Panics if the instance has not been initialized.
    #[inline]
    pub fn get(&self) -> &T {
        if self.status.load(Ordering::Acquire) != INITIALIZED {
            panic!("Once instance is not initialized");
        }
        unsafe { (&*self.value.get()).assume_init_ref() }
    }

    /// Return whether the instance has already been initialized.
    #[inline]
    pub fn is_initialized(&self) -> bool {
        self.status.load(Ordering::Acquire) == INITIALIZED
    }

    /// Returns a reference to the initialized value, or initializes it via the
    /// provided closure.
    ///
    /// # Arguments
    ///
    /// - `f` - A closure that initializes the instance if it has not been
    ///   initialized.
    #[inline]
    pub fn get_or_else<F>(&self, f: F) -> &T
    where
        F: Fn() -> T,
    {
        let mut v = None;

        loop {
            match self.status.load(Ordering::Acquire) {
                INITIALIZED => {
                    return unsafe { (&*self.value.get()).assume_init_ref() };
                }
                INITIALIZING => {
                    core::hint::spin_loop();
                }
                UNINITIALIZED => {
                    if let Err(e) = self.try_init(v.unwrap_or(f())) {
                        v = Some(match e {
                            TryInitError::Initializing(ret)
                            | TryInitError::AlreadyInitialized(ret) => ret,
                        });
                    } else {
                        return self.get();
                    }
                }
                _ => unreachable!(),
            }
        }
    }

    /// Attempts to initialize the instance with the provided value.
    ///
    /// # Arguments
    ///
    /// - `v` - The value to initialize the instance with.
    ///
    /// # Returns
    ///
    /// - `Ok(())` if the instance was successfully initialized.
    /// - `Err(TryInitError)` if the instance was already initialized or is
    ///   currently being initialized.
    #[inline]
    pub fn try_init(&self, v: T) -> Result<(), TryInitError<T>> {
        let _scope = S::enter();

        if let Err(e) = self.status.compare_exchange(
            UNINITIALIZED,
            INITIALIZING,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Err(match e {
                INITIALIZED => TryInitError::AlreadyInitialized(v),
                INITIALIZING => TryInitError::Initializing(v),
                _ => unreachable!(),
            })
        } else {
            unsafe {
                self.value.get().write(MaybeUninit::new(v));
            };
            self.status.store(INITIALIZED, Ordering::Release);
            Ok(())
        }
    }

    /// Initializes the instance with the provided value.
    ///
    /// # Arguments
    ///
    /// - `v` - The value to initialize the instance with.
    ///
    /// # Panics
    ///
    /// Panics if the instance was already initialized or is currently being
    /// initialized.
    #[inline]
    pub fn init(&self, v: T) {
        self.try_init(v).unwrap()
    }
}

unsafe impl<T: Send, S: DroppableScopedGuard> Send for Once<T, S> {}
unsafe impl<T: Send + Sync, S: DroppableScopedGuard> Sync for Once<T, S> {}
