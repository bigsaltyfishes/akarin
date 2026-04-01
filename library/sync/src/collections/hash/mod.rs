use core::{hash::Hash, ops::Deref};

use hashbrown::DefaultHashBuilder;
use traits::RawHashMap;

mod locked_impl;
mod rcu_impl;
mod traits;
mod wrapper;

#[cfg(test)]
mod tests;

pub mod locked {
    pub use super::locked_impl::*;
}

pub mod rcu {
    pub use super::rcu_impl::*;
}

pub mod prelude {
    pub use super::{
        traits::*,
        wrapper::{ConcurrentMap, MaybeArc},
    };
}

pub type LockedMap<K, V, S, H = DefaultHashBuilder> =
    DefaultHashMap<K, V, locked_impl::LockedMap<K, V, S, H>>;
pub type RcuMap<K, V> = DefaultHashMap<K, V, rcu_impl::HamtMap<K, V>>;

pub struct DefaultHashMap<K, V, M>
where
    M: RawHashMap<K, V>,
    K: Hash + Eq + Send,
    V: Send,
{
    inner: M,
    _marker: core::marker::PhantomData<(K, V)>,
}

impl<K, V, M> Default for DefaultHashMap<K, V, M>
where
    M: RawHashMap<K, V> + Default,
    K: Hash + Eq + Send,
    V: Send,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V, M> DefaultHashMap<K, V, M>
where
    M: RawHashMap<K, V> + Default,
    K: Hash + Eq + Send,
    V: Send,
{
    /// Creates a new `DefaultHashMap` with the given inner map.
    pub fn new() -> Self {
        Self {
            inner: M::default(),
            _marker: core::marker::PhantomData,
        }
    }
}

impl<K, V, M> Deref for DefaultHashMap<K, V, M>
where
    M: RawHashMap<K, V>,
    K: Hash + Eq + Send,
    V: Send,
{
    type Target = M;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
