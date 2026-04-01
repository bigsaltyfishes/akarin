use core::{cell::Cell, ops::Deref, panic};

use libakarin_machine_core::sync::DroppableScopedGuard;

use crate::spin::Once;

/// A lazily initialized value.
///
/// # Generics
///
/// - `T`: The type of data to be initialized.
/// - `S`: A Irq Save scope guard type that implements `DroppableScopedGuard`.
/// - `F`: The type of the initialization function.
#[repr(C)]
pub struct Lazy<T, S>
where
    S: DroppableScopedGuard,
{
    cell: Once<T, S>,
    init: Cell<Option<fn() -> T>>,
}

impl<T, S> Lazy<T, S>
where
    S: DroppableScopedGuard,
{
    /// Creates a new `Lazy` instance with the given initialization function.
    #[inline]
    pub const fn new(f: fn() -> T) -> Self {
        Self {
            cell: Once::new(),
            init: Cell::new(Some(f)),
        }
    }

    /// Forces the initialization of the lazy value and returns a reference to
    /// it.
    ///
    /// # Arguments
    ///
    /// - `this` - A reference to the `Lazy` instance.
    ///
    /// # Returns
    ///
    /// - `&T` - A reference to the initialized value.
    #[inline]
    pub fn force(this: &Self) -> &T {
        this.cell.get_or_else(|| match this.init.take() {
            Some(f) => f(),
            None => panic!("Lazy instance has previously been poisoned"),
        })
    }
}

impl<T, S> Deref for Lazy<T, S>
where
    S: DroppableScopedGuard,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        Self::force(self)
    }
}

unsafe impl<T, S> Sync for Lazy<T, S>
where
    Once<T, S>: Sync,
    S: DroppableScopedGuard,
{
}
