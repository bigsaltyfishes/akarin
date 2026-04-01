use libakarin_macros::abstraction;

use crate::sync::class::GuardClass;

pub mod class {
    pub struct NoOpGuard;
    pub struct IrqGuard;
    pub struct IrqSaveGuard;
    pub struct NoPreemptGuard;

    pub trait GuardClass {}

    impl GuardClass for NoOpGuard {}
    impl GuardClass for IrqGuard {}
    impl GuardClass for IrqSaveGuard {}
    impl GuardClass for NoPreemptGuard {}
}

pub struct ScopedGuard<T>(Option<T>)
where
    T: RawScopedGuard;

#[abstraction(RawScopedGuard, visibility = "public")]
impl<T> RawScopedGuard for ScopedGuard<T>
where
    Self: Sized,
    T: RawScopedGuard,
{
    type Class: GuardClass = T::Class;

    #[inline]
    fn enter() -> Self {
        Self(Some(T::enter()))
    }

    #[inline]
    fn exit(self) {
        drop(self);
    }

    #[default]
    #[inline]
    fn with<F, R>(f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let _guard = Self::enter();
        f()
    }
}

impl<T> Drop for ScopedGuard<T>
where
    T: RawScopedGuard,
{
    fn drop(&mut self) {
        if let Some(inner) = self.0.take() {
            inner.exit();
        }
    }
}

pub struct NoOp;

impl RawScopedGuard for NoOp {
    type Class = class::NoOpGuard;

    fn enter() -> Self {
        Self
    }

    fn exit(self) {}
}

pub trait NoOpGuard: RawScopedGuard {}
pub trait IrqGuard: RawScopedGuard {}
pub trait IrqSaveGuard: RawScopedGuard {}
pub trait NoPreemptGuard: RawScopedGuard {}

impl<T> NoOpGuard for ScopedGuard<T> where T: RawScopedGuard<Class = class::NoOpGuard> {}
impl<T> IrqGuard for ScopedGuard<T> where T: RawScopedGuard<Class = class::IrqGuard> {}
impl<T> IrqSaveGuard for ScopedGuard<T> where T: RawScopedGuard<Class = class::IrqSaveGuard> {}
impl<T> NoPreemptGuard for ScopedGuard<T> where T: RawScopedGuard<Class = class::NoPreemptGuard> {}

pub unsafe trait DroppableScopedGuard: RawScopedGuard {}

unsafe impl<T> DroppableScopedGuard for ScopedGuard<T> where T: RawScopedGuard {}
