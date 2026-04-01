//! Cache implementations with raw pointer optimization.
//!
//! This module provides high-performance cache implementations using raw
//! pointers instead of `Rc<RefCell<T>>` for better performance. The
//! implementations support node reuse and transfer between caches for efficient
//! memory management.
//!
//! # Scoped Borrow Pattern
//!
//! These caches use a **scoped borrow pattern** for safe access to cached data.
//! Methods like `get_with` and `peek_with` accept closures that receive
//! references, ensuring references cannot escape and enabling safe cache
//! resizing.
//!
//! # Thread Safety
//!
//! These caches implement `Send` when `K`, `V`, and hasher types are `Send`,
//! allowing safe use with `Mutex<Cache>` for concurrent access.
//!
//! # Eviction Callbacks
//!
//! Both `LruCache` and `AdaptiveCache` support eviction callbacks via the
//! `OnEvict` trait. Implement this trait to receive notifications when items
//! are evicted from the cache. Use `with_on_evict()` constructor to create a
//! cache with eviction callbacks.

use core::{
    borrow::Borrow,
    hash::{Hash, Hasher},
};

pub mod arc;
pub mod lru;

/// Trait for handling eviction callbacks.
///
/// Implement this trait to receive notifications when items are evicted
/// from the cache. Both `LruCache` and `AdaptiveCache` support this trait.
///
/// # Example
///
/// ```ignore
/// struct LogEvict;
///
/// impl<K: Debug, V: Debug> OnEvict<K, V> for LogEvict {
///     fn on_evict(&self, key: &K, value: &V) {
///         println!("Evicted: {:?} => {:?}", key, value);
///     }
/// }
///
/// let lru_cache = LruCache::with_on_evict(100, hasher, LogEvict);
/// let arc_cache = AdaptiveCache::with_on_evict(100, hasher, LogEvict);
/// ```
pub trait OnEvict<K, V> {
    /// Called when an item is about to be evicted from the cache.
    ///
    /// This is called before the item is removed, so the key and value are
    /// still valid. For `AdaptiveCache`, this is NOT called when items are
    /// evicted from ghost lists.
    fn on_evict(&self, key: &K, value: &V);
}

/// Default no-op eviction callback.
///
/// This is the default eviction handler that does nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoOpEvict;

impl<K, V> OnEvict<K, V> for NoOpEvict {
    #[inline]
    fn on_evict(&self, _key: &K, _value: &V) {}
}

#[repr(transparent)]
struct KeyWrapper<Q: ?Sized>(Q);

impl<Q: ?Sized> KeyWrapper<Q> {
    fn from_ref(q: &Q) -> &Self {
        unsafe { core::mem::transmute(q) }
    }
}

impl<Q> Hash for KeyWrapper<Q>
where
    Q: ?Sized + Hash,
{
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl<Q> PartialEq for KeyWrapper<Q>
where
    Q: ?Sized + PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        self.0.eq(&other.0)
    }
}

impl<Q> Eq for KeyWrapper<Q> where Q: ?Sized + Eq {}

impl<K, Q> Borrow<KeyWrapper<Q>> for KeyRef<K>
where
    K: Borrow<Q>,
    Q: ?Sized,
{
    fn borrow(&self) -> &KeyWrapper<Q> {
        KeyWrapper::from_ref(unsafe { (*self.0).borrow() })
    }
}

pub struct KeyRef<K>(*const K);

impl<K: Hash> Hash for KeyRef<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        unsafe {
            (*self.0).hash(state);
        }
    }
}

impl<K: PartialEq> PartialEq for KeyRef<K> {
    fn eq(&self, other: &Self) -> bool {
        unsafe { (*self.0).eq(&*other.0) }
    }
}

impl<K: Eq> Eq for KeyRef<K> {}
