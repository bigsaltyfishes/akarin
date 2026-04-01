//! LRU Cache implementation using raw pointers for performance.
//!
//! # Scoped Borrow Pattern
//!
//! This implementation uses a **scoped borrow pattern** for safe access to
//! cached data. Instead of returning references directly, methods like
//! `get_with` and `peek_with` accept a closure that receives references to the
//! key and value. This ensures that:
//!
//! 1. References cannot escape the closure scope
//! 2. The cache can be safely resized (shrink/grow) without invalidating
//!    references
//! 3. The cache can be safely used with `Mutex<Cache>` for thread safety
//!
//! ## API Overview
//!
//! - `get_with(key, |k, v| ...)` - Access with closure, updates LRU order
//! - `peek_with(key, |k, v| ...)` - Access with closure, does not update LRU
//!   order
//! - `get_owned(key)` - Returns cloned key-value pair (requires `K: Clone, V:
//!   Clone`)
//! - `peek_owned(key)` - Returns cloned key-value pair without updating order
//!
//! ## Thread Safety
//!
//! The cache implements `Send` when `K`, `V`, and `H` are `Send`, allowing safe
//! use with `Mutex<LruCache<K, V, H>>` for concurrent access.

use alloc::{boxed::Box, vec::Vec};
use core::{
    hash::{BuildHasher, Hash},
    mem::{self, MaybeUninit},
    ptr::NonNull,
};

use hashbrown::HashMap;

use crate::cache::{KeyRef, KeyWrapper, NoOpEvict, OnEvict};

/// Internal entry node for the LRU linked list.
///
/// Uses raw pointers for prev/next to avoid the overhead of `Rc<RefCell<T>>`.
/// Key and value use `MaybeUninit` to support sentinel nodes (head/tail
/// guards).
pub(crate) struct LruEntry<K, V> {
    pub(crate) key: MaybeUninit<K>,
    pub(crate) val: MaybeUninit<V>,
    prev: *mut LruEntry<K, V>,
    next: *mut LruEntry<K, V>,
}

impl<K, V> LruEntry<K, V> {
    /// Create a new entry with initialized key and value.
    fn new(key: K, val: V) -> Self {
        LruEntry {
            key: MaybeUninit::new(key),
            val: MaybeUninit::new(val),
            prev: core::ptr::null_mut(),
            next: core::ptr::null_mut(),
        }
    }

    /// Create a sentinel (guard) node with uninitialized key/value.
    fn new_sigil() -> Self {
        LruEntry {
            key: MaybeUninit::uninit(),
            val: MaybeUninit::uninit(),
            prev: core::ptr::null_mut(),
            next: core::ptr::null_mut(),
        }
    }

    /// Get a reference to the key (assumes initialized).
    ///
    /// # Safety
    /// Caller must ensure this is not a sentinel node.
    #[inline]
    pub(crate) unsafe fn key_ref(&self) -> &K {
        unsafe { self.key.assume_init_ref() }
    }

    /// Get a reference to the value (assumes initialized).
    ///
    /// # Safety
    /// Caller must ensure this is not a sentinel node.
    #[inline]
    pub(crate) unsafe fn val_ref(&self) -> &V {
        unsafe { self.val.assume_init_ref() }
    }

    /// Get a mutable reference to the value (assumes initialized).
    ///
    /// # Safety
    /// Caller must ensure this is not a sentinel node.
    #[inline]
    #[allow(dead_code)]
    pub(crate) unsafe fn val_mut(&mut self) -> &mut V {
        unsafe { self.val.assume_init_mut() }
    }
}

/// Least Recently Used (LRU) Cache implementation.
///
/// This implementation uses raw pointers for the internal doubly-linked list,
/// providing better performance than `Rc<RefCell<T>>` based implementations.
///
/// # Type Parameters
///
/// - `K`: Key type, must implement `Hash + Eq`
/// - `V`: Value type
/// - `H`: Hash builder type
/// - `E`: Eviction callback type, defaults to `NoOpEvict`
pub struct LruCache<K, V, H, E = NoOpEvict>
where
    H: BuildHasher,
{
    capacity: usize,
    map: HashMap<KeyRef<K>, NonNull<LruEntry<K, V>>, H>,
    /// Sentinel head node (next points to MRU item)
    head: *mut LruEntry<K, V>,
    /// Sentinel tail node (prev points to LRU item)
    tail: *mut LruEntry<K, V>,
    /// Eviction callback handler
    on_evict: E,
}

/// Linked List related methods
impl<K, V, H, E> LruCache<K, V, H, E>
where
    H: BuildHasher,
{
    /// Attach a node right after the head sentinel (making it MRU).
    ///
    /// # Safety
    /// - `node` must be a valid, non-null pointer to an allocated `LruEntry`
    /// - `node` must not already be in the list
    unsafe fn attach(&mut self, node: *mut LruEntry<K, V>) {
        unsafe {
            (*node).next = (*self.head).next;
            (*node).prev = self.head;
            (*(*self.head).next).prev = node;
            (*self.head).next = node;
        }
    }

    /// Detach a node from the linked list.
    ///
    /// # Safety
    /// - `node` must be a valid pointer to a node currently in the list
    /// - `node` must not be a sentinel node
    unsafe fn detach(&mut self, node: *mut LruEntry<K, V>) {
        unsafe {
            (*(*node).prev).next = (*node).next;
            (*(*node).next).prev = (*node).prev;
        }
    }

    /// Move an existing node to the head (MRU position).
    ///
    /// # Safety
    /// - `node` must be a valid pointer to a node currently in the list
    unsafe fn move_to_head(&mut self, node: *mut LruEntry<K, V>) {
        unsafe {
            self.detach(node);
            self.attach(node);
        }
    }
}

impl<K, V, H> LruCache<K, V, H, NoOpEvict>
where
    K: Hash + Eq,
    H: BuildHasher,
{
    /// Create a new LRU cache with the specified capacity and hasher.
    ///
    /// This creates a cache with no eviction callback. Use `with_on_evict()` to
    /// create a cache with eviction callbacks.
    pub fn new(capacity: usize, hasher: H) -> Self {
        Self::with_on_evict(capacity, hasher, NoOpEvict)
    }
}

impl<K, V, H, E> LruCache<K, V, H, E>
where
    K: Hash + Eq,
    H: BuildHasher,
    E: OnEvict<K, V>,
{
    /// Create a new LRU cache with the specified capacity, hasher and eviction
    /// callback.
    ///
    /// The `on_evict` handler will be called whenever an item is evicted from
    /// the cache.
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
    /// let cache = LruCache::with_on_evict(100, DefaultHashBuilder::default(), LogEvict);
    /// ```
    pub fn with_on_evict(capacity: usize, hasher: H, on_evict: E) -> Self {
        // Create sentinel nodes
        let head = Box::into_raw(Box::new(LruEntry::new_sigil()));
        let tail = Box::into_raw(Box::new(LruEntry::new_sigil()));

        // Link sentinels together
        unsafe {
            (*head).next = tail;
            (*tail).prev = head;
        }

        LruCache {
            capacity,
            map: HashMap::with_capacity_and_hasher(capacity, hasher),
            head,
            tail,
            on_evict,
        }
    }

    /// Insert or update a key-value pair in the cache.
    ///
    /// If the key already exists, update its value using `mem::swap` and move
    /// it to MRU position. If the key does not exist and the cache is at
    /// capacity, evict the LRU item.
    ///
    /// # Returns
    ///
    /// - `Some((K, V))` if an item was evicted (due to capacity limit) or if
    ///   updating existing key.
    /// - `None` if no item was evicted and it was a new insertion.
    pub fn put(&mut self, key: K, value: V) -> Option<(K, V)> {
        if let Some(&node) = self.map.get(KeyWrapper::from_ref(&key)) {
            // Key exists: update value with mem::swap and move to head
            let node_ptr = node.as_ptr();
            let mut new_value = value;
            unsafe {
                mem::swap((*node_ptr).val.assume_init_mut(), &mut new_value);
                self.move_to_head(node_ptr);
            }
            // Return the old value (now in new_value after swap)
            return Some((key, new_value));
        }

        // New key: check if we need to evict
        let evicted = if self.map.len() >= self.capacity {
            let evicted = self.pop_back_internal();
            // Call eviction callback
            if let Some((ref k, ref v)) = evicted {
                self.on_evict.on_evict(k, v);
            }
            evicted
        } else {
            None
        };

        // Create and insert new node
        let new_node = Box::into_raw(Box::new(LruEntry::new(key, value)));
        let key_ref = unsafe { KeyRef((*new_node).key.assume_init_ref() as *const K) };

        unsafe {
            self.attach(new_node);
        }
        self.map
            .insert(key_ref, unsafe { NonNull::new_unchecked(new_node) });

        evicted
    }

    /// Access a cached value through a closure (scoped borrow pattern).
    ///
    /// This is the recommended way to access cached values. The closure
    /// receives references to the key and value, and can return any result.
    /// This pattern ensures that references cannot escape and the cache can
    /// be safely modified.
    ///
    /// If the key exists, the entry is moved to MRU position.
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
        K: core::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
        F: FnOnce(&K, &V) -> R,
    {
        if let Some(&node) = self.map.get(KeyWrapper::from_ref(key)) {
            let node_ptr = node.as_ptr();
            unsafe {
                self.move_to_head(node_ptr);
                Some(f((*node_ptr).key_ref(), (*node_ptr).val_ref()))
            }
        } else {
            None
        }
    }
}

impl<K, V, H, E> LruCache<K, V, H, E>
where
    K: Hash + Eq,
    H: BuildHasher,
{
    /// Peek at a cached value through a closure without modifying LRU order.
    ///
    /// Similar to `get_with`, but does not update the LRU order.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let result = cache.peek_with(&key, |k, v| v.clone());
    /// ```
    pub fn peek_with<Q, F, R>(&self, key: &Q, f: F) -> Option<R>
    where
        K: core::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
        F: FnOnce(&K, &V) -> R,
    {
        if let Some(&node) = self.map.get(KeyWrapper::from_ref(key)) {
            let node_ptr = node.as_ptr();
            unsafe { Some(f((*node_ptr).key_ref(), (*node_ptr).val_ref())) }
        } else {
            None
        }
    }

    /// Get a reference to the most recently used item.
    ///
    /// # Returns
    ///
    /// - `Some((&K, &V))` if the cache is not empty.
    /// - `None` if the cache is empty.
    pub fn front(&self) -> Option<(&K, &V)> {
        unsafe {
            let first = (*self.head).next;
            if first == self.tail {
                None
            } else {
                Some(((*first).key_ref(), (*first).val_ref()))
            }
        }
    }

    /// Access the most recently used item through a closure.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let result = cache.front_with(|k, v| (k.clone(), v.clone()));
    /// ```
    pub fn front_with<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&K, &V) -> R,
    {
        unsafe {
            let first = (*self.head).next;
            if first == self.tail {
                None
            } else {
                Some(f((*first).key_ref(), (*first).val_ref()))
            }
        }
    }

    /// Remove and return the most recently used item.
    pub fn pop_front(&mut self) -> Option<(K, V)> {
        unsafe {
            let first = (*self.head).next;
            if first == self.tail {
                return None;
            }

            self.detach(first);
            let key_ref = KeyRef((*first).key.assume_init_ref() as *const K);
            self.map.remove(&key_ref);

            let node = Box::from_raw(first);
            Some((node.key.assume_init(), node.val.assume_init()))
        }
    }

    /// Get a reference to the least recently used item.
    ///
    /// # Returns
    ///
    /// - `Some((&K, &V))` if the cache is not empty.
    /// - `None` if the cache is empty.
    pub fn back(&self) -> Option<(&K, &V)> {
        unsafe {
            let last = (*self.tail).prev;
            if last == self.head {
                None
            } else {
                Some(((*last).key_ref(), (*last).val_ref()))
            }
        }
    }

    /// Access the least recently used item through a closure.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let result = cache.back_with(|k, v| (k.clone(), v.clone()));
    /// ```
    pub fn back_with<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&K, &V) -> R,
    {
        unsafe {
            let last = (*self.tail).prev;
            if last == self.head {
                None
            } else {
                Some(f((*last).key_ref(), (*last).val_ref()))
            }
        }
    }

    /// Internal method to pop the LRU item and return it.
    fn pop_back_internal(&mut self) -> Option<(K, V)> {
        unsafe {
            let last = (*self.tail).prev;
            if last == self.head {
                return None;
            }

            self.detach(last);
            let key_ref = KeyRef((*last).key.assume_init_ref() as *const K);
            self.map.remove(&key_ref);

            let node = Box::from_raw(last);
            Some((node.key.assume_init(), node.val.assume_init()))
        }
    }

    /// Remove and return the least recently used item from the cache.
    ///
    /// # Returns
    ///
    /// - `Some((K, V))` if the cache is not empty.
    /// - `None` if the cache is empty.
    pub fn pop_back(&mut self) -> Option<(K, V)> {
        self.pop_back_internal()
    }

    /// Remove the LRU node from the list but return the node without
    /// deallocating.
    ///
    /// This is used for node transfer between caches (e.g., to ghost lists in
    /// ARC). The returned node is detached from the list and removed from
    /// the map, but its memory is not freed.
    ///
    /// # Returns
    ///
    /// - `Some(NonNull<LruEntry<K, V>>)` if the cache is not empty.
    /// - `None` if the cache is empty.
    pub(crate) fn remove_lru_node(&mut self) -> Option<NonNull<LruEntry<K, V>>> {
        unsafe {
            let last = (*self.tail).prev;
            if last == self.head {
                return None;
            }

            self.detach(last);
            let key_ref = KeyRef((*last).key.assume_init_ref() as *const K);
            self.map.remove(&key_ref);

            Some(NonNull::new_unchecked(last))
        }
    }

    /// Insert an externally allocated node into the cache at MRU position.
    ///
    /// This is used for node transfer between caches (e.g., from main cache to
    /// ghost list). The node will be added to the map and linked list.
    ///
    /// # Returns
    ///
    /// - `Some(NonNull<LruEntry<K, V>>)` if an existing node was evicted to
    ///   make room.
    /// - `None` if no eviction was needed.
    pub(crate) fn put_node(
        &mut self,
        node: NonNull<LruEntry<K, V>>,
    ) -> Option<NonNull<LruEntry<K, V>>> {
        let node_ptr = node.as_ptr();

        // Check if we need to evict first
        let evicted = if self.map.len() >= self.capacity {
            self.remove_lru_node()
        } else {
            None
        };

        // Insert the node
        let key_ref = unsafe { KeyRef((*node_ptr).key.assume_init_ref() as *const K) };
        unsafe {
            self.attach(node_ptr);
        }
        self.map.insert(key_ref, node);

        evicted
    }

    /// Remove a key-value pair from the cache.
    ///
    /// # Returns
    ///
    /// - `Some((K, V))` if the key existed and was removed.
    /// - `None` if the key did not exist.
    pub fn remove<Q>(&mut self, key: &Q) -> Option<(K, V)>
    where
        K: core::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        if let Some(node) = self.map.remove(KeyWrapper::from_ref(key)) {
            let node_ptr = node.as_ptr();
            unsafe {
                self.detach(node_ptr);
                let node = Box::from_raw(node_ptr);
                Some((node.key.assume_init(), node.val.assume_init()))
            }
        } else {
            None
        }
    }

    /// Remove a key from the cache and return the node without deallocating.
    ///
    /// This is used for node transfer between caches.
    ///
    /// # Returns
    ///
    /// - `Some(NonNull<LruEntry<K, V>>)` if the key existed and was removed.
    /// - `None` if the key did not exist.
    pub(crate) fn remove_node<Q>(&mut self, key: &Q) -> Option<NonNull<LruEntry<K, V>>>
    where
        K: core::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        if let Some(node) = self.map.remove(KeyWrapper::from_ref(key)) {
            let node_ptr = node.as_ptr();
            unsafe {
                self.detach(node_ptr);
            }
            Some(node)
        } else {
            None
        }
    }

    /// Check if the cache contains a key.
    ///
    /// # Returns
    ///
    /// - `true` if the cache contains the key.
    /// - `false` otherwise.
    pub fn contains<Q>(&self, key: &Q) -> bool
    where
        K: core::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.contains_key(KeyWrapper::from_ref(key))
    }
}

impl<K, V, H, E> Clone for LruCache<K, V, H, E>
where
    K: Hash + Eq + Clone,
    V: Clone,
    H: BuildHasher + Clone,
    E: OnEvict<K, V> + Clone,
{
    fn clone(&self) -> Self {
        let mut new_cache = LruCache::with_on_evict(
            self.capacity,
            self.map.hasher().clone(),
            self.on_evict.clone(),
        );

        // Iterate through the list from tail to head (oldest to newest)
        // so that the order is preserved after cloning
        unsafe {
            let mut current = (*self.tail).prev;
            while current != self.head {
                let key = (*current).key_ref().clone();
                let val = (*current).val_ref().clone();
                new_cache.put(key, val);
                current = (*current).prev;
            }
        }

        new_cache
    }
}

/// Methods requiring `Clone` for convenience owned access.
impl<K, V, H, E> LruCache<K, V, H, E>
where
    K: Hash + Eq + Clone,
    V: Clone,
    H: BuildHasher,
    E: OnEvict<K, V>,
{
    /// Get a cloned copy of the key-value pair, updating LRU order.
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
        K: core::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.get_with(key, |k, v| (k.clone(), v.clone()))
    }

    /// Get a cloned copy of the key-value pair without updating LRU order.
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
        K: core::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.peek_with(key, |k, v| (k.clone(), v.clone()))
    }
}

impl<K, V, H, E> LruCache<K, V, H, E>
where
    K: Hash + Eq,
    H: BuildHasher,
{
    /// Get the current capacity of the cache.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Get the current number of items in the cache.
    #[inline]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Check if the cache is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl<K, V, H, E> LruCache<K, V, H, E>
where
    K: Hash + Eq,
    H: BuildHasher,
    E: OnEvict<K, V>,
{
    /// Resize the cache to a new capacity.
    ///
    /// If the new capacity is smaller than the current size, LRU items will be
    /// evicted and the eviction callback will be called for each.
    ///
    /// This method is safe because the scoped borrow pattern (`get_with`,
    /// `peek_with`) ensures that no references to cache entries can escape
    /// the closure scope.
    ///
    /// # Returns
    ///
    /// A vector containing all evicted key-value pairs.
    pub fn resize(&mut self, new_capacity: usize) -> Vec<(K, V)> {
        self.capacity = new_capacity;

        let mut evicted = Vec::new();
        while self.map.len() > self.capacity {
            if let Some((k, v)) = self.pop_back_internal() {
                self.on_evict.on_evict(&k, &v);
                evicted.push((k, v));
            } else {
                break;
            }
        }

        evicted
    }

    /// Resize the cache and return evicted nodes without deallocating them.
    ///
    /// This is used internally by ARC to transfer evicted nodes to ghost lists.
    /// The caller is responsible for properly managing the returned nodes'
    /// memory. Note: Does NOT call on_evict - caller is responsible for
    /// callbacks.
    pub(crate) fn resize_to_nodes(&mut self, new_capacity: usize) -> Vec<NonNull<LruEntry<K, V>>> {
        self.capacity = new_capacity;

        let mut evicted = Vec::new();
        while self.map.len() > self.capacity {
            if let Some(node) = self.remove_lru_node() {
                evicted.push(node);
            } else {
                break;
            }
        }

        evicted
    }
}

impl<K, V, H, E> Drop for LruCache<K, V, H, E>
where
    H: BuildHasher,
{
    fn drop(&mut self) {
        // Clear all entries from the map and deallocate nodes
        unsafe {
            let mut current = (*self.head).next;
            while current != self.tail {
                let next = (*current).next;
                // Drop the node and its contents
                let _ = Box::from_raw(current);
                current = next;
            }

            // Drop sentinel nodes (they have uninitialized key/val, so just free memory)
            let _ = Box::from_raw(self.head);
            let _ = Box::from_raw(self.tail);
        }

        // Clear the map (entries already deallocated above)
        self.map.clear();
    }
}

/// Free a detached node, dropping its key and value.
///
/// # Safety
/// The node must be detached from any list and not referenced elsewhere.
pub(crate) unsafe fn free_node<K, V>(node: NonNull<LruEntry<K, V>>) {
    unsafe {
        let _ = Box::from_raw(node.as_ptr());
    }
}

// SAFETY: LruCache can be sent between threads if K, V, H and E are Send.
// The raw pointers in the linked list are only accessed through &mut self,
// ensuring exclusive access. The scoped borrow pattern prevents any
// references from escaping, making this safe for use with Mutex<LruCache>.
unsafe impl<K: Send, V: Send, H: Send, E: Send> Send for LruCache<K, V, H, E> where H: BuildHasher {}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use hashbrown::DefaultHashBuilder;

    use super::LruCache;

    #[test]
    fn test_lru_cache() {
        let mut cache = LruCache::new(2, DefaultHashBuilder::default());

        assert_eq!(cache.put(1, "one"), None);
        assert_eq!(cache.put(2, "two"), None);
        assert!(cache.get_with(&1, |_, _| ()).is_some());
        assert!(cache.get_with(&1, |_, v| v.eq(&"one")).unwrap());
        assert_eq!(cache.put(3, "three"), Some((2, "two")));
        assert!(cache.get_with(&2, |_, _| ()).is_none());
        assert_eq!(cache.put(4, "four"), Some((1, "one")));
        assert!(cache.get_with(&1, |_, _| ()).is_none());
        assert!(cache.get_with(&3, |_, v| v.eq(&"three")).unwrap());
        assert!(cache.get_with(&4, |_, v| v.eq(&"four")).unwrap());
    }

    #[test]
    fn test_resize() {
        let mut cache = LruCache::new(3, DefaultHashBuilder::default());

        cache.put(1, "one");
        cache.put(2, "two");
        cache.put(3, "three");

        // Shrink: resize to smaller capacity
        let evicted = cache.resize(2);
        assert_eq!(evicted, vec![(1, "one")]);
        assert!(cache.get_with(&1, |_, _| ()).is_none());
        assert!(cache.get_with(&2, |_, v| v.eq(&"two")).unwrap());
        assert!(cache.get_with(&3, |_, v| v.eq(&"three")).unwrap());

        // Grow: resize to larger capacity
        let evicted = cache.resize(5);
        assert!(evicted.is_empty());
        cache.put(4, "four");
        cache.put(5, "five");

        assert!(cache.get_with(&2, |_, v| v.eq(&"two")).unwrap());
        assert!(cache.get_with(&3, |_, v| v.eq(&"three")).unwrap());
        assert!(cache.get_with(&4, |_, v| v.eq(&"four")).unwrap());
        assert!(cache.get_with(&5, |_, v| v.eq(&"five")).unwrap());
    }

    #[test]
    fn test_lru_front_back() {
        let mut cache = LruCache::new(3, DefaultHashBuilder::default());

        // Empty cache
        assert!(cache.front().is_none());
        assert!(cache.back().is_none());

        cache.put(1, "one");
        cache.put(2, "two");
        cache.put(3, "three");

        // Front should be most recently used (3)
        assert_eq!(cache.front().unwrap().0, &3);
        assert_eq!(cache.front().unwrap().1, &"three");

        // Back should be least recently used (1)
        assert_eq!(cache.back().unwrap().0, &1);
        assert_eq!(cache.back().unwrap().1, &"one");

        // Access 1 to make it most recently used
        cache.get_with(&1, |_, _| ());
        assert_eq!(cache.front().unwrap().0, &1);
        assert_eq!(cache.back().unwrap().0, &2);
    }

    #[test]
    fn test_lru_pop_front_back() {
        let mut cache = LruCache::new(3, DefaultHashBuilder::default());

        cache.put(1, "one");
        cache.put(2, "two");
        cache.put(3, "three");

        // Pop front (most recently used)
        assert_eq!(cache.pop_front(), Some((3, "three")));
        assert!(cache.get_with(&3, |_, _| ()).is_none());
        assert_eq!(cache.len(), 2);

        // Pop back (least recently used)
        assert_eq!(cache.pop_back(), Some((1, "one")));
        assert!(cache.get_with(&1, |_, _| ()).is_none());
        assert_eq!(cache.len(), 1);

        // Only 2 remains
        assert!(cache.get_with(&2, |_, _| ()).is_some());
    }

    #[test]
    fn test_lru_remove() {
        let mut cache = LruCache::new(3, DefaultHashBuilder::default());

        cache.put(1, "one");
        cache.put(2, "two");
        cache.put(3, "three");

        // Remove middle element
        assert_eq!(cache.remove(&2), Some((2, "two")));
        assert!(cache.get_with(&2, |_, _| ()).is_none());
        assert_eq!(cache.len(), 2);

        // Remaining elements should still be accessible
        assert!(cache.get_with(&1, |_, _| ()).is_some());
        assert!(cache.get_with(&3, |_, _| ()).is_some());

        // Remove non-existent
        assert_eq!(cache.remove(&999), None);
    }

    #[test]
    fn test_lru_contains() {
        let mut cache = LruCache::new(2, DefaultHashBuilder::default());

        assert!(!cache.contains(&1));

        cache.put(1, "one");
        assert!(cache.contains(&1));
        assert!(!cache.contains(&2));

        cache.put(2, "two");
        assert!(cache.contains(&1));
        assert!(cache.contains(&2));

        // Evict 1 by adding 3
        cache.put(3, "three");
        assert!(!cache.contains(&1));
        assert!(cache.contains(&2));
        assert!(cache.contains(&3));
    }

    #[test]
    fn test_lru_capacity_and_len() {
        let mut cache = LruCache::new(3, DefaultHashBuilder::default());

        assert_eq!(cache.capacity(), 3);
        assert_eq!(cache.len(), 0);

        cache.put(1, "one");
        assert_eq!(cache.len(), 1);

        cache.put(2, "two");
        assert_eq!(cache.len(), 2);

        cache.put(3, "three");
        assert_eq!(cache.len(), 3);

        // Adding more should evict, keeping len at capacity
        cache.put(4, "four");
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.capacity(), 3);
    }

    #[test]
    fn test_lru_update_existing() {
        let mut cache = LruCache::new(2, DefaultHashBuilder::default());

        cache.put(1, "one");
        cache.put(2, "two");

        // Update existing key
        let old = cache.put(1, "ONE");
        assert_eq!(old, Some((1, "one")));
        assert!(cache.get_with(&1, |_, v| v.eq(&"ONE")).unwrap());

        // Key 1 should now be most recently used
        assert_eq!(cache.front().unwrap().0, &1);
    }

    #[test]
    fn test_lru_clone() {
        let mut cache = LruCache::new(3, DefaultHashBuilder::default());

        cache.put(1, "one");
        cache.put(2, "two");
        cache.put(3, "three");

        let mut cloned = cache.clone();

        // Both caches should have same content
        assert!(cloned.get_with(&1, |_, v| v.eq(&"one")).unwrap());
        assert!(cloned.get_with(&2, |_, v| v.eq(&"two")).unwrap());
        assert!(cloned.get_with(&3, |_, v| v.eq(&"three")).unwrap());

        // Modifying clone shouldn't affect original
        cloned.put(4, "four");
        assert!(cloned.get_with(&4, |_, _| ()).is_some());
        assert!(cache.get_with(&4, |_, _| ()).is_none());
    }

    #[test]
    fn test_lru_empty_operations() {
        let mut cache: LruCache<i32, &str, _> = LruCache::new(2, DefaultHashBuilder::default());

        assert!(cache.front().is_none());
        assert!(cache.back().is_none());
        assert!(cache.pop_front().is_none());
        assert!(cache.pop_back().is_none());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_lru_single_element() {
        let mut cache = LruCache::new(1, DefaultHashBuilder::default());

        cache.put(1, "one");
        assert_eq!(cache.front().unwrap().0, &1);
        assert_eq!(cache.back().unwrap().0, &1);

        // Adding another should evict the first
        cache.put(2, "two");
        assert!(cache.get_with(&1, |_, _| ()).is_none());
        assert!(cache.get_with(&2, |_, _| ()).is_some());
    }

    #[test]
    fn test_lru_on_evict_callback() {
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

        impl Clone for CountingEvict {
            fn clone(&self) -> Self {
                CountingEvict {
                    count: Arc::clone(&self.count),
                }
            }
        }

        let count = Arc::new(AtomicUsize::new(0));
        let mut cache = LruCache::with_on_evict(
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

        // This should evict key 1 and trigger on_evict
        cache.put(3, "three");
        assert_eq!(count.load(Ordering::SeqCst), 1);

        // Resize to smaller capacity should evict and trigger on_evict
        let evicted = cache.resize(1);
        assert_eq!(evicted.len(), 1);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
}
