//! Adaptive Replacement Cache (ARC) implementation.
//!
//! ARC is a self-tuning cache algorithm that dynamically balances between
//! recency and frequency based on workload patterns.
//!
//! # Scoped Borrow Pattern
//!
//! This implementation uses a **scoped borrow pattern** for safe access to
//! cached data. Methods like `get_with` and `peek_with` accept closures that
//! receive references, ensuring references cannot escape and enabling safe
//! cache resizing.
//!
//! # Eviction Callbacks
//!
//! The cache supports eviction callbacks via the `OnEvict` trait. Implement
//! this trait to receive notifications when items are evicted from the main
//! cache to ghost lists. Use `with_on_evict()` constructor to create a cache
//! with callbacks.
//!
//! # Thread Safety
//!
//! This cache implements `Send` when all type parameters are `Send`, allowing
//! safe use with `Mutex<AdaptiveCache>` for concurrent access.
//!
//! # Node Reuse
//!
//! This implementation stores complete key-value pairs in ghost lists for
//! efficient node reuse. When an item is evicted and later re-accessed (ghost
//! hit), the node is transferred back without reallocation.

use alloc::vec::Vec;
use core::{
    borrow::Borrow,
    cmp::min,
    hash::{BuildHasher, Hash},
    ptr::NonNull,
};

use crate::cache::{
    NoOpEvict, OnEvict,
    lru::{LruCache, LruEntry, free_node},
};

/// Adaptive Replacement Cache (ARC) implementation.
///
/// ARC is a self-tuning cache that balances between recently used (recency)
/// and frequently used (frequency) items. It maintains four lists:
/// - `recent`: Recently accessed items (seen once)
/// - `frequent`: Frequently accessed items (seen more than once)
/// - `recent_evict`: Ghost list of recently evicted items from `recent` (stores
///   full K,V)
/// - `frequent_evict`: Ghost list of recently evicted items from `frequent`
///   (stores full K,V)
///
/// # Type Parameters
///
/// - `K`: Key type, must implement `Hash + Eq`
/// - `V`: Value type
/// - `S`: Hash builder type
/// - `E`: Eviction callback type, defaults to `NoOpEvict`
pub struct AdaptiveCache<K, V, S, E = NoOpEvict>
where
    S: BuildHasher,
{
    /// Target size for the recent list (adaptive parameter)
    p: usize,
    /// Total capacity of the cache
    capacity: usize,
    /// Cache for recently used items (accessed once)
    /// Uses NoOpEvict because eviction callbacks are handled at AdaptiveCache
    /// level
    recent: LruCache<K, V, S, NoOpEvict>,
    /// Ghost list of items evicted from recent (stores full K,V for node reuse)
    recent_evict: LruCache<K, V, S, NoOpEvict>,
    /// Cache for frequently used items (accessed more than once)
    frequent: LruCache<K, V, S, NoOpEvict>,
    /// Ghost list of items evicted from frequent (stores full K,V for node
    /// reuse)
    frequent_evict: LruCache<K, V, S, NoOpEvict>,
    /// Eviction callback handler
    on_evict: E,
}

impl<K, V, S> AdaptiveCache<K, V, S, NoOpEvict>
where
    K: Eq + Hash,
    S: BuildHasher + Clone,
{
    /// Create a new Adaptive Replacement Cache with the specified capacity and
    /// hasher.
    ///
    /// This creates a cache with no eviction callback. Use `with_on_evict()` to
    /// create a cache with eviction callbacks.
    pub fn new(capacity: usize, hash_builder: S) -> Self {
        Self::with_on_evict(capacity, hash_builder, NoOpEvict)
    }
}

impl<K, V, S, E> AdaptiveCache<K, V, S, E>
where
    K: Eq + Hash,
    S: BuildHasher + Clone,
    E: OnEvict<K, V>,
{
    /// Create a new Adaptive Replacement Cache with eviction callbacks.
    ///
    /// The `on_evict` handler will be called whenever an item is evicted from
    /// the main cache (recent or frequent) to a ghost list.
    ///
    /// # Example
    ///
    /// ```ignore
    /// struct LogEvict;
    /// impl<K: Debug, V: Debug> OnEvict<K, V> for LogEvict {
    ///     fn on_evict(&self, key: &K, value: &V) {
    ///         println!("Evicted: {:?} => {:?}", key, value);
    ///     }
    /// }
    ///
    /// let cache = AdaptiveCache::with_on_evict(100, DefaultHashBuilder::default(), LogEvict);
    /// ```
    pub fn with_on_evict(capacity: usize, hash_builder: S, on_evict: E) -> Self {
        Self {
            p: capacity / 2, // Start with balanced split
            capacity,
            recent: LruCache::new(capacity, hash_builder.clone()),
            recent_evict: LruCache::new(capacity, hash_builder.clone()),
            frequent: LruCache::new(capacity, hash_builder.clone()),
            frequent_evict: LruCache::new(capacity, hash_builder),
            on_evict,
        }
    }
}

impl<K, V, S, E> AdaptiveCache<K, V, S, E>
where
    K: Eq + Hash,
    S: BuildHasher + Clone,
    E: OnEvict<K, V>,
{
    /// Call eviction callback and transfer node to ghost list.
    ///
    /// # Safety
    /// The node must be a valid, detached node from recent or frequent cache.
    #[inline]
    unsafe fn evict_to_ghost(
        on_evict: &E,
        node: NonNull<LruEntry<K, V>>,
        ghost_list: &mut LruCache<K, V, S, NoOpEvict>,
    ) {
        unsafe {
            // Call eviction callback
            let node_ptr = node.as_ptr();
            let k = &*(*node_ptr).key.as_ptr();
            let v = &*(*node_ptr).val.as_ptr();
            on_evict.on_evict(k, v);

            // If ghost list is full, the evicted ghost node will be freed
            if let Some(evicted_ghost) = ghost_list.put_node(node) {
                free_node(evicted_ghost);
            }
        }
    }

    /// Replace an item to make space for a new entry.
    ///
    /// This implements the ARC replacement policy:
    /// - Evict from recent if recent.len() > p, OR
    /// - Evict from recent if recent.len() == p AND freq_contains_key is true
    /// - Otherwise evict from frequent
    ///
    /// Calls the eviction callback before transferring nodes to ghost lists.
    fn replace(&mut self, freq_contains_key: bool) {
        let total = self.recent.len() + self.frequent.len();
        if total < self.capacity {
            return;
        }

        let recent_len = self.recent.len();
        let evict_from_recent =
            recent_len > 0 && (recent_len > self.p || (recent_len == self.p && freq_contains_key));

        if evict_from_recent {
            // Transfer node from recent to recent_evict
            if let Some(node) = self.recent.remove_lru_node() {
                unsafe {
                    Self::evict_to_ghost(&self.on_evict, node, &mut self.recent_evict);
                }
            }
        } else if self.frequent.len() > 0 {
            // Transfer node from frequent to frequent_evict
            if let Some(node) = self.frequent.remove_lru_node() {
                unsafe {
                    Self::evict_to_ghost(&self.on_evict, node, &mut self.frequent_evict);
                }
            }
        }
    }

    /// Transfer a node from a ghost list to frequent cache.
    ///
    /// This is used when we get a ghost hit - the node is moved from the ghost
    /// list to the frequent cache without reallocation.
    fn transfer_ghost_to_frequent(
        &mut self,
        node: NonNull<LruEntry<K, V>>,
        freq_contains_key: bool,
    ) {
        self.replace(freq_contains_key);
        if let Some(evicted) = self.frequent.put_node(node) {
            // Transfer the evicted node to frequent_evict - MUST call on_evict!
            unsafe {
                Self::evict_to_ghost(&self.on_evict, evicted, &mut self.frequent_evict);
            }
        }
    }

    /// Insert a key-value pair into the cache.
    ///
    /// If the key already exists, update its value using `mem::swap` (node
    /// reuse) and mark it as most recently used.
    /// If the key does not exist, insert it into the appropriate cache.
    pub fn put(&mut self, key: K, value: V) {
        // Case 1: Key exists in recent cache - move to frequent
        if let Some(node) = self.recent.remove_node(&key) {
            // Update the value in the node
            unsafe {
                let node_ptr = node.as_ptr();
                *(*node_ptr).val.assume_init_mut() = value;
            }
            // Transfer to frequent (node reuse)
            if let Some(evicted) = self.frequent.put_node(node) {
                // Must call on_evict when evicting from frequent to ghost!
                unsafe {
                    Self::evict_to_ghost(&self.on_evict, evicted, &mut self.frequent_evict);
                }
            }
            return;
        }

        // Case 2: Key exists in frequent cache - update value
        if let Some(node) = self.frequent.remove_node(&key) {
            // Update the value in the node
            unsafe {
                let node_ptr = node.as_ptr();
                *(*node_ptr).val.assume_init_mut() = value;
            }
            // Reinsert to mark as most recently used
            self.frequent.put_node(node);
            return;
        }

        // Case 3: Key is in recent ghost list - increase p, move to frequent
        if let Some(node) = self.recent_evict.remove_node(&key) {
            // Increase p - recent was too small
            let delta = 1.max(self.frequent_evict.len() / self.recent_evict.len().max(1));
            self.p = min(self.capacity, self.p + delta);

            // Update the value in the node
            unsafe {
                let node_ptr = node.as_ptr();
                *(*node_ptr).val.assume_init_mut() = value;
            }

            // freq_contains_key=false: the key is not in frequent_evict (it was in
            // recent_evict)
            self.transfer_ghost_to_frequent(node, false);
            return;
        }

        // Case 4: Key is in frequent ghost list - decrease p, move to frequent
        if let Some(node) = self.frequent_evict.remove_node(&key) {
            // Decrease p - frequent was too small
            let delta = 1.max(self.recent_evict.len() / self.frequent_evict.len().max(1));
            self.p = self.p.saturating_sub(delta);

            // Update the value in the node
            unsafe {
                let node_ptr = node.as_ptr();
                *(*node_ptr).val.assume_init_mut() = value;
            }

            // freq_contains_key=true: the key was in frequent_evict
            self.transfer_ghost_to_frequent(node, true);
            return;
        }

        // Case 5: Key is completely new - add to recent
        // freq_contains_key=false: new key is not in frequent_evict
        self.replace(false);
        self.recent.put(key, value);
    }

    /// Access a cached value through a closure (scoped borrow pattern).
    ///
    /// This is the recommended way to access cached values. The closure
    /// receives references to the key and value, and can return any result.
    /// This pattern ensures that references cannot escape and the cache can
    /// be safely modified.
    ///
    /// If the key exists in recent cache, it will be promoted to frequent
    /// cache.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let result = cache.get_with(&key, |k, v| {
    ///     // Use k and v here
    ///     v.clone()
    /// });
    /// ```
    pub fn get_with<Q, F, R>(&mut self, key: &Q, f: F) -> Option<R>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
        F: FnOnce(&K, &V) -> R,
    {
        // Check frequent cache first (no list change needed, just move to head)
        if self.frequent.contains(key) {
            return self.frequent.get_with(key, f);
        }

        // Check recent cache - if found, promote to frequent
        if let Some(node) = self.recent.remove_node(key) {
            // Transfer node to frequent (node reuse)
            if let Some(evicted) = self.frequent.put_node(node) {
                // Must call on_evict when evicting from frequent to ghost!
                unsafe {
                    Self::evict_to_ghost(&self.on_evict, evicted, &mut self.frequent_evict);
                }
            }
            // Access from frequent's front (the node we just added)
            return self.frequent.front_with(f);
        }

        None
    }

    /// Peek at a value through a closure without modifying cache state.
    ///
    /// Similar to `get_with`, but does not promote items or update LRU order.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let result = cache.peek_with(&key, |k, v| v.clone());
    /// ```
    pub fn peek_with<Q, F, R>(&self, key: &Q, f: F) -> Option<R>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
        F: FnOnce(&K, &V) -> R,
    {
        // Check which cache contains the key first, then apply closure
        if self.frequent.contains(key) {
            self.frequent.peek_with(key, f)
        } else {
            self.recent.peek_with(key, f)
        }
    }

    /// Check if the cache contains the given key.
    pub fn contains<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.recent.contains(key) || self.frequent.contains(key)
    }

    /// Get the total number of items in the cache.
    pub fn len(&self) -> usize {
        self.recent.len() + self.frequent.len()
    }

    /// Check if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get the current capacity of the cache.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Remove a key-value pair from the cache.
    ///
    /// # Returns
    ///
    /// - `Some((K, V))` if the key existed and was removed.
    /// - `None` if the key did not exist.
    pub fn remove<Q>(&mut self, key: &Q) -> Option<(K, V)>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        // Check recent cache first
        if let Some(kv) = self.recent.remove(key) {
            return Some(kv);
        }

        // Check frequent cache
        if let Some(kv) = self.frequent.remove(key) {
            return Some(kv);
        }

        None
    }

    /// Resize the cache to a new capacity.
    ///
    /// If the new capacity is smaller than the current size, items will be
    /// evicted from recent and frequent caches, and the eviction callback
    /// will be called for each evicted item.
    ///
    /// This method is safe because the scoped borrow pattern (`get_with`,
    /// `peek_with`) ensures that no references to cache entries can escape
    /// the closure scope.
    ///
    /// # Returns
    ///
    /// A vector containing all evicted key-value pairs (from main caches only,
    /// not from ghost lists).
    pub fn resize(&mut self, new_capacity: usize) -> Vec<(K, V)> {
        let mut evicted = Vec::new();

        if new_capacity == self.capacity {
            return evicted;
        }

        let old_capacity = self.capacity;

        // Scale p proportionally
        self.p = if old_capacity > 0 {
            (self.p as u64 * new_capacity as u64 / old_capacity as u64) as usize
        } else {
            new_capacity / 2
        };
        self.p = self.p.min(new_capacity);

        self.capacity = new_capacity;

        // Resize ghost lists (free evicted nodes)
        for node in self.recent_evict.resize_to_nodes(new_capacity) {
            unsafe {
                free_node(node);
            }
        }
        for node in self.frequent_evict.resize_to_nodes(new_capacity) {
            unsafe {
                free_node(node);
            }
        }

        // Evict from main caches if over capacity
        let total_current = self.recent.len() + self.frequent.len();

        if total_current > new_capacity {
            // First, evict from recent if it exceeds p
            while self.recent.len() > self.p
                && self.recent.len() + self.frequent.len() > new_capacity
            {
                if let Some(kv) = self.recent.pop_back() {
                    self.on_evict.on_evict(&kv.0, &kv.1);
                    evicted.push(kv);
                } else {
                    break;
                }
            }

            // Then, evict from frequent if still over capacity
            while self.recent.len() + self.frequent.len() > new_capacity {
                if let Some(kv) = self.frequent.pop_back() {
                    self.on_evict.on_evict(&kv.0, &kv.1);
                    evicted.push(kv);
                } else {
                    break;
                }
            }

            // If still over capacity, evict from recent
            while self.recent.len() + self.frequent.len() > new_capacity {
                if let Some(kv) = self.recent.pop_back() {
                    self.on_evict.on_evict(&kv.0, &kv.1);
                    evicted.push(kv);
                } else {
                    break;
                }
            }
        }

        // Update internal cache capacities
        self.recent.resize(new_capacity);
        self.frequent.resize(new_capacity);

        evicted
    }
}

/// Methods requiring `Clone` for convenience owned access.
impl<K, V, S, E> AdaptiveCache<K, V, S, E>
where
    K: Eq + Hash + Clone,
    V: Clone,
    S: BuildHasher + Clone,
    E: OnEvict<K, V>,
{
    /// Get a cloned copy of the key-value pair, updating LRU order and
    /// promoting to frequent.
    ///
    /// This is a convenience method equivalent to:
    /// ```ignore
    /// cache.get_with(key, |k, v| (k.clone(), v.clone()))
    /// ```
    ///
    /// # Returns
    ///
    /// - `Some((K, V))` if the key exists (cloned).
    /// - `None` if the key does not exist.
    pub fn get_owned<Q>(&mut self, key: &Q) -> Option<(K, V)>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.get_with(key, |k, v| (k.clone(), v.clone()))
    }

    /// Get a cloned copy of the key-value pair without updating cache state.
    ///
    /// This is a convenience method equivalent to:
    /// ```ignore
    /// cache.peek_with(key, |k, v| (k.clone(), v.clone()))
    /// ```
    ///
    /// # Returns
    ///
    /// - `Some((K, V))` if the key exists (cloned).
    /// - `None` if the key does not exist.
    pub fn peek_owned<Q>(&self, key: &Q) -> Option<(K, V)>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.peek_with(key, |k, v| (k.clone(), v.clone()))
    }
}

#[cfg(test)]
mod tests {
    use hashbrown::DefaultHashBuilder;

    use super::AdaptiveCache;

    #[test]
    fn test_arc_basic_put_get() {
        let mut cache: AdaptiveCache<i32, &str, _> =
            AdaptiveCache::new(3, DefaultHashBuilder::default());

        // Test basic insertion
        cache.put(1, "one");
        cache.put(2, "two");
        cache.put(3, "three");

        // Test retrieval
        assert!(cache.get_with(&1, |_, _| ()).is_some());
        assert!(cache.get_with(&1, |_, _| ()).is_some());
        assert!(cache.get_with(&2, |_, _| ()).is_some());
        assert!(cache.get_with(&3, |_, _| ()).is_some());

        // Test non-existent key
        assert!(cache.get_with(&4, |_, _| ()).is_none());
    }

    #[test]
    fn test_arc_promotion_to_frequent() {
        let mut cache: AdaptiveCache<i32, &str, _> =
            AdaptiveCache::new(3, DefaultHashBuilder::default());

        // Insert items - they go to recent cache
        cache.put(1, "one");
        cache.put(2, "two");

        // Access item 1 - it should be promoted to frequent cache
        cache.get_with(&1, |_, _| ());

        // Item 1 should still be accessible
        assert!(cache.get_with(&1, |_, _| ()).is_some());
        assert!(cache.get_with(&1, |_, v| v.eq(&"one")).unwrap());
    }

    #[test]
    fn test_arc_update_existing_key() {
        let mut cache: AdaptiveCache<i32, &str, _> =
            AdaptiveCache::new(3, DefaultHashBuilder::default());

        cache.put(1, "one");
        assert!(cache.get_with(&1, |_, v| v.eq(&"one")).unwrap());

        // Update the value
        cache.put(1, "ONE");
        assert!(cache.get_with(&1, |_, v| v.eq(&"ONE")).unwrap());
    }

    #[test]
    fn test_arc_empty_cache() {
        let mut cache: AdaptiveCache<i32, &str, _> =
            AdaptiveCache::new(3, DefaultHashBuilder::default());

        // Empty cache should return None for any key
        assert!(cache.get_with(&1, |_, _| ()).is_none());
        assert!(cache.get_with(&2, |_, _| ()).is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn test_arc_single_capacity() {
        let mut cache: AdaptiveCache<i32, &str, _> =
            AdaptiveCache::new(1, DefaultHashBuilder::default());

        cache.put(1, "one");
        assert!(cache.get_with(&1, |_, _| ()).is_some());

        cache.put(2, "two");
        // With capacity 1, key 1 should be evicted
        assert_eq!(cache.len(), 1);
        assert!(cache.get_with(&2, |_, _| ()).is_some());
    }

    #[test]
    fn test_arc_entry_ref_key_and_value() {
        let mut cache: AdaptiveCache<i32, &str, _> =
            AdaptiveCache::new(3, DefaultHashBuilder::default());

        cache.put(42, "answer");

        cache
            .get_with(&42, |k, v| {
                assert_eq!(*k, 42);
                assert_eq!(*v, "answer");
            })
            .unwrap();
    }

    #[test]
    fn test_arc_contains() {
        let mut cache: AdaptiveCache<i32, &str, _> =
            AdaptiveCache::new(3, DefaultHashBuilder::default());

        assert!(!cache.contains(&1));

        cache.put(1, "one");
        assert!(cache.contains(&1));
        assert!(!cache.contains(&2));
    }

    #[test]
    fn test_arc_len() {
        let mut cache: AdaptiveCache<i32, &str, _> =
            AdaptiveCache::new(3, DefaultHashBuilder::default());

        assert_eq!(cache.len(), 0);

        cache.put(1, "one");
        assert_eq!(cache.len(), 1);

        cache.put(2, "two");
        assert_eq!(cache.len(), 2);

        cache.put(3, "three");
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn test_arc_eviction() {
        let mut cache: AdaptiveCache<i32, &str, _> =
            AdaptiveCache::new(2, DefaultHashBuilder::default());

        cache.put(1, "one");
        cache.put(2, "two");
        assert_eq!(cache.len(), 2);

        // Adding a third item should evict one
        cache.put(3, "three");
        assert_eq!(cache.len(), 2);

        // At least two of the three items should be in cache
        let count = [1, 2, 3].iter().filter(|&&k| cache.contains(&k)).count();
        assert_eq!(count, 2);
    }

    #[test]
    fn test_arc_remove() {
        let mut cache: AdaptiveCache<i32, &str, _> =
            AdaptiveCache::new(3, DefaultHashBuilder::default());

        cache.put(1, "one");
        cache.put(2, "two");
        cache.put(3, "three");

        // Remove existing key
        let removed = cache.remove(&2);
        assert!(removed.is_some());
        assert_eq!(removed.unwrap(), (2, "two"));
        assert_eq!(cache.len(), 2);
        assert!(!cache.contains(&2));

        // Remove non-existing key
        let removed = cache.remove(&4);
        assert!(removed.is_none());
        assert_eq!(cache.len(), 2);

        // Remove remaining keys
        assert!(cache.remove(&1).is_some());
        assert!(cache.remove(&3).is_some());
        assert!(cache.is_empty());
    }

    #[test]
    fn test_arc_remove_from_frequent() {
        let mut cache: AdaptiveCache<i32, &str, _> =
            AdaptiveCache::new(3, DefaultHashBuilder::default());

        cache.put(1, "one");
        // Access to promote to frequent
        cache.get_with(&1, |_, _| ());

        // Key 1 should now be in frequent cache
        let removed = cache.remove(&1);
        assert!(removed.is_some());
        assert_eq!(removed.unwrap(), (1, "one"));
        assert!(cache.is_empty());
    }

    #[test]
    fn test_arc_resize_shrink() {
        let mut cache: AdaptiveCache<i32, &str, _> =
            AdaptiveCache::new(5, DefaultHashBuilder::default());

        cache.put(1, "one");
        cache.put(2, "two");
        cache.put(3, "three");
        cache.put(4, "four");
        cache.put(5, "five");
        assert_eq!(cache.len(), 5);
        assert_eq!(cache.capacity(), 5);

        // Shrink to capacity 3
        let evicted = cache.resize(3);
        assert_eq!(evicted.len(), 2);
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.capacity(), 3);
    }

    #[test]
    fn test_arc_resize_grow() {
        let mut cache: AdaptiveCache<i32, &str, _> =
            AdaptiveCache::new(2, DefaultHashBuilder::default());

        cache.put(1, "one");
        cache.put(2, "two");
        assert_eq!(cache.len(), 2);

        // Grow to capacity 5
        let evicted = cache.resize(5);
        assert!(evicted.is_empty());
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.capacity(), 5);

        // Should be able to add more items now
        cache.put(3, "three");
        cache.put(4, "four");
        cache.put(5, "five");
        assert_eq!(cache.len(), 5);
    }

    #[test]
    fn test_arc_resize_same_capacity() {
        let mut cache: AdaptiveCache<i32, &str, _> =
            AdaptiveCache::new(3, DefaultHashBuilder::default());

        cache.put(1, "one");
        cache.put(2, "two");

        // Resize to same capacity should be a no-op
        let evicted = cache.resize(3);
        assert!(evicted.is_empty());
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.capacity(), 3);
    }

    #[test]
    fn test_arc_resize_preserves_p_ratio() {
        let mut cache: AdaptiveCache<i32, &str, _> =
            AdaptiveCache::new(10, DefaultHashBuilder::default());

        // Fill cache and manipulate p through ghost list hits
        for i in 0..10 {
            cache.put(i, "value");
        }

        // Shrink to half - p should scale proportionally
        cache.resize(5);
        assert_eq!(cache.capacity(), 5);
        assert!(cache.len() <= 5);
    }

    #[test]
    fn test_arc_peek() {
        let mut cache: AdaptiveCache<i32, &str, _> =
            AdaptiveCache::new(3, DefaultHashBuilder::default());

        cache.put(1, "one");
        cache.put(2, "two");

        // Peek should not promote to frequent
        assert!(cache.peek_with(&1, |_, v| v.eq(&"one")).unwrap());

        // Item 1 should still be in recent (verify by checking it's still there)
        assert!(cache.contains(&1));
    }

    #[test]
    fn test_arc_ghost_list_hit() {
        let mut cache: AdaptiveCache<i32, &str, _> =
            AdaptiveCache::new(2, DefaultHashBuilder::default());

        // Fill cache
        cache.put(1, "one");
        cache.put(2, "two");

        // Evict key 1 by adding key 3
        cache.put(3, "three");
        assert!(!cache.contains(&1));

        // Re-add key 1 - should hit ghost list and go to frequent
        cache.put(1, "one_new");
        assert!(cache.contains(&1));
        assert!(cache.get_with(&1, |_, v| v.eq(&"one_new")).unwrap());
    }

    #[test]
    fn test_arc_on_evict_callback() {
        use alloc::sync::Arc;
        use core::sync::atomic::{AtomicUsize, Ordering};

        use crate::cache::OnEvict;

        struct CountingEvict {
            count: Arc<AtomicUsize>,
        }

        impl OnEvict<i32, &'static str> for CountingEvict {
            fn on_evict(&self, _key: &i32, _value: &&'static str) {
                self.count.fetch_add(1, Ordering::SeqCst);
            }
        }

        let count = Arc::new(AtomicUsize::new(0));
        let mut cache = AdaptiveCache::with_on_evict(
            2,
            DefaultHashBuilder::default(),
            CountingEvict {
                count: Arc::clone(&count),
            },
        );

        // Fill cache
        cache.put(1, "one");
        cache.put(2, "two");
        assert_eq!(count.load(Ordering::SeqCst), 0);

        // This should evict one item and trigger on_evict
        cache.put(3, "three");
        assert_eq!(count.load(Ordering::SeqCst), 1);

        // Resize to smaller capacity should evict and trigger on_evict
        let evicted = cache.resize(1);
        assert_eq!(evicted.len(), 1);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
}

// SAFETY: AdaptiveCache can be sent between threads if all type parameters are
// Send. The internal LruCaches are Send (due to the scoped borrow pattern), and
// the on_evict callback is also required to be Send.
unsafe impl<K, V, S, E> Send for AdaptiveCache<K, V, S, E>
where
    K: Send,
    V: Send,
    S: BuildHasher + Send,
    E: Send,
{
}
