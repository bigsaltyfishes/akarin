use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::{
    borrow::Borrow,
    sync::atomic::{AtomicU32, AtomicUsize, Ordering},
};

use atomig::Atomic;
use crossbeam_queue::SegQueue;
use libakarin_collections::intrusive::{DoubleLink, LinkedList};
use libakarin_syscall::ObjectLifecycleFlags;

use crate::{
    Capability, Handle, Map, NameSpace, ObjectContainer, ObjectError, ObjectStatus, Payload,
    object::{Object, ObjectRef, Token},
};

pub type Index = usize;

pub(crate) struct IndexAllocator {
    next: AtomicUsize,
    reclaimed: SegQueue<Index>,
}

impl IndexAllocator {
    const fn new(start: usize) -> Self {
        Self {
            next: AtomicUsize::new(start),
            reclaimed: SegQueue::new(),
        }
    }

    pub(crate) fn allocate(&self) -> Index {
        if let Some(index) = self.reclaimed.pop() {
            index
        } else {
            self.next.fetch_add(1, Ordering::AcqRel)
        }
    }

    pub(crate) fn reclaim(&self, index: Index) {
        self.reclaimed.push(index);
    }
}

pub struct ObjectPath {
    inner: String,
}

impl ObjectPath {
    pub fn new<S: AsRef<str>>(s: S) -> Self {
        Self {
            inner: s.as_ref().to_string(),
        }
    }

    pub fn is_absolute(&self) -> bool {
        self.inner.starts_with('/')
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.inner
            .split('/')
            .filter(|component| !component.is_empty())
    }

    pub fn to_string(&self) -> String {
        self.inner.clone()
    }

    pub fn parent(&self) -> Option<ObjectPath> {
        let mut components: Vec<&str> = self.components().collect();
        if components.is_empty() {
            return None;
        }
        components.pop();
        Some(ObjectPath {
            inner: format!("/{}", components.join("/")),
        })
    }

    pub fn join(&self, other: &str) -> ObjectPath {
        let mut new_path = self.inner.clone();
        if !new_path.ends_with('/') {
            new_path.push('/');
        }
        new_path.push_str(other);
        ObjectPath { inner: new_path }
    }
}

/// A registry that manages objects.
///
/// The [`Registry`] maintains a mapping of object indices to their
/// corresponding [`ObjectContainer`]s, and provides methods for object
/// creation, lookup, and removal. It also handles path resolution for locating
/// objects based on their hierarchical paths. The registry uses an
/// [`IndexAllocator`] to manage object indices, allowing for efficient
/// allocation and reclamation of indices as objects are created and destroyed.
pub struct Registry {
    pub(crate) map: Map<Index, ObjectContainer>,
    pub(crate) idx_allocator: IndexAllocator,
}

impl Registry {
    /// Predefined token for the root object with ID `0`.
    /// This token has `SEND` and `READ` capabilities enabled.
    const PRE_ROOT_TOKEN: Token = Token {
        id: None,
        capabilities: Capability::from_bits_truncate(
            Capability::SEND.bits() | Capability::READ.bits(),
        ),
        interface_caps: 0,
    };

    /// Create a new `Registry` instance with a root object.
    ///
    /// The root object is initialized with ID `0` and has all capabilities
    /// (`EXECUTE`, `ADMIN`, and `AGENT`) disabled. The root object serves as
    /// the starting point for the object hierarchy managed by this
    /// `Registry`.
    ///
    /// # Returns
    ///
    /// * `Registry` - A new instance of `Registry` with a root object.
    pub fn new() -> Self {
        Self {
            map: Map::new(),
            idx_allocator: IndexAllocator::new(1),
        }
    }

    /// Initialize the root object in the registry.
    ///
    /// This method creates the root object with ID `0` and inserts it into
    /// the registry's map. The root object is initialized with all capabilities
    /// (`EXECUTE`, `ADMIN`, and `AGENT`) disabled.
    ///
    /// # Returns
    ///
    /// * `Ok(Handle)` - A handle to the root object if it is successfully
    ///   created and inserted into the registry.
    /// * `Err(ObjectError)` - An error indicating why the root object creation
    ///   failed, such as if the root object already exists in the registry.
    pub fn init_root(&'static self) -> Result<Handle, ObjectError> {
        let root = ObjectContainer::new(|| Object {
            link: DoubleLink::new(),
            registry: Some(&self),
            id: 0,
            parent: None,
            status: Atomic::new(ObjectStatus::Active),
            name: String::new(),
            masked_caps: Atomic::new(Capability::EXECUTE | Capability::ADMIN | Capability::AGENT),
            lifecycle_flags: AtomicU32::new(ObjectLifecycleFlags::STICKY.bits()),
            public_interface_caps: AtomicU32::new(0),
            children: Map::new(),
            payload: Payload::new(NameSpace),
        });
        let handle = root.acquire_handle(Token::new(None, Capability::TRUSTED, u32::MAX))?;
        self.map.insert(0, root);

        Ok(handle)
    }

    /// Remove an object by its handle.
    ///
    /// Caller must have `ADMIN` capability to remove the object.
    ///
    /// This method marks the object and all its descendants as `Destroying`
    /// and then reclaims their indices from the allocator.
    ///
    /// # Arguments
    ///
    /// * `handle` - A handle to the object to be removed.
    ///
    /// # Returns
    ///
    /// * `Ok(())` - If the object and its descendants are successfully removed.
    /// * `Err(ObjectError)` - If the removal fails due to insufficient
    ///   capabilities or if the object has already been destroyed.
    pub fn remove<H>(&self, handle: H, is_kernel: bool) -> Result<(), ObjectError>
    where
        H: Borrow<Handle>,
    {
        let handle = handle.borrow();
        if !handle.capabilities().contains(Capability::ADMIN) {
            return Err(ObjectError::InsufficientCapabilities);
        }

        // Mark the object as destroying.
        let object = handle
            .object
            .upgrade()
            .ok_or(ObjectError::ObjectDestroyed)?;
        if !is_kernel && object.is_sticky() {
            return Err(ObjectError::InvalidArgument);
        }

        self.remove_internal(object)
    }

    pub(crate) fn remove_internal(&self, object: Arc<Object>) -> Result<(), ObjectError> {
        // Unregister the object from its parent's children map if applicable.
        // We only remove the entry when the stored value matches this object's
        // ID. This prevents a concurrent add_child rollback from accidentally
        // removing an entry that belongs to a *different* child that won the
        // name race. The get-then-remove is safe here because:
        //   - In normal destruction the entry belongs to us and no other thread will
        //     race to insert the same name (the parent is being destroyed or the caller
        //     already holds the canonical reference).
        //   - In rollback (duplicate name) the ID will *not* match, so we correctly
        //     skip the remove and leave the winner's entry intact.
        if let Some(parent_id) = object.parent {
            if let Some(parent_entry) = self.map.get(&parent_id) {
                let children = &parent_entry.value().object.children;
                if let Some(entry) = children.get(&object.name) {
                    if *entry.value() == object.id {
                        children.remove(&object.name);
                    }
                }
            }
        }

        // Mark the object and all its descendants as `Destroying` using a layer visit,
        // then reclaim their indices from the allocator.
        if object
            .status
            .compare_exchange(
                ObjectStatus::Active,
                ObjectStatus::Destroying,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Err(ObjectError::ObjectDestroyed);
        }

        let mut queue = LinkedList::<Object, DoubleLink>::new();
        let mut visited = LinkedList::<Object, DoubleLink>::new();
        unsafe { queue.push_back(Arc::into_raw(object) as _) };

        // Perform a layer visit to mark all objects as `Destroying`
        while let Some(ptr) = unsafe { queue.pop_front() } {
            let reference = unsafe { &*ptr };
            for child_id in reference.children.iter().map(|ent| ent.value()) {
                let child = match self.map.get(child_id).map(|ent| ent.value()) {
                    Some(child) => child,
                    None => {
                        // Child was already removed (e.g., concurrent add_child rollback).
                        // Skip gracefully.
                        continue;
                    }
                };

                if child
                    .object
                    .status
                    .compare_exchange(
                        ObjectStatus::Active,
                        ObjectStatus::Destroying,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_err()
                {
                    continue;
                }

                // Enqueue the child for further traversal.
                unsafe { queue.push_back(Arc::into_raw(child.object.clone()) as _) };
            }

            // Record the visited object for later reclamation.
            unsafe { visited.push_front(ptr) };
        }

        // Reclaim all visited objects.
        while let Some(ptr) = unsafe { visited.pop_front() } {
            let reference = unsafe { Arc::from_raw(ptr) };
            self.map.remove(&reference.id);
            self.idx_allocator.reclaim(reference.id);
        }

        Ok(())
    }

    /// Helpers to insert an object into the registry.
    pub(crate) fn insert_internal(
        &'static self,
        object: ObjectContainer,
    ) -> Result<Handle, ObjectError> {
        let id = object.object.id;
        // Construct the owner's ADMIN handle directly, bypassing derive_token.
        // The owner should always have full ADMIN capabilities regardless of
        // the object's masked_caps (which only restricts externally acquired
        // handles through path traversal or parent token derivation).
        let handle = Handle::new(
            Token::new(Some(id), Capability::ADMIN_GRP, u32::MAX),
            Some(self),
            ObjectRef::Shared(Arc::downgrade(&object.object)),
        );
        self.map.insert(id, object);
        Ok(handle)
    }

    /// Locate an object by its path starting from the given current handle,
    /// and then apply the provided function `f` to the located object's handle.
    ///
    /// This method resolves the path by traversing the object tree using
    /// lightweight [`Token`]-based capability derivation at each level, and
    /// only constructs a full [`Handle`] for the final resolved object. This
    /// avoids the overhead of intermediate `Arc`/`Weak` reference counting
    /// operations during path traversal.
    ///
    /// # Arguments
    ///
    /// * `curr` - An optional handle representing the current object from which
    ///   the path resolution starts. If `None`, the path must be absolute.
    /// * `path` - The object path to locate.
    /// * `f` - A closure that takes the located object's handle and returns a
    ///   result.
    ///
    /// # Returns
    ///
    /// * `Ok(R)` - The result of the closure if the object is successfully
    ///   located and the function is applied.
    /// * `Err(ObjectError)` - An error indicating why the location or function
    ///   application failed, such as if the object is not found or if the
    ///   arguments are invalid.
    pub fn locate_and_then<H, F, R>(
        &self,
        curr: Option<H>,
        path: &ObjectPath,
        f: F,
    ) -> Result<R, ObjectError>
    where
        H: AsRef<Handle>,
        F: FnOnce(Handle) -> R,
    {
        // Initialize the traversal token from the starting point.
        let mut current_token = match (&curr, path.is_absolute()) {
            // Relative path: start from the provided handle's token.
            (Some(handle), false) => {
                let h = handle.as_ref();
                Token::new(h.object_id(), h.capabilities(), h.interface_caps())
            }
            // Absolute path with root handle: use the root handle's token.
            (Some(handle), true) if handle.as_ref().object_id() == Some(0) => {
                let h = handle.as_ref();
                Token::new(h.object_id(), h.capabilities(), h.interface_caps())
            }
            // Absolute path without handle: derive token for root from the
            // predefined root token.
            (None, true) => {
                let root = self.map.get(&0).ok_or(ObjectError::ObjectNotFound)?;
                root.value().derive_token(&Self::PRE_ROOT_TOKEN)?
            }
            _ => return Err(ObjectError::InvalidArgument),
        };

        // Traverse path components using token-level capability derivation.
        for name in path.components() {
            if name == "self" {
                continue;
            }

            let current_id = current_token.id.ok_or(ObjectError::InvalidArgument)?;

            // Look up the current object to query its children.
            let current_entry = self
                .map
                .get(&current_id)
                .ok_or(ObjectError::ObjectNotFound)?;
            let current_obj = &current_entry.value().object;

            // Find the child by name.
            let child_id = *current_obj
                .children
                .get(name)
                .ok_or(ObjectError::ObjectNotFound)?
                .value();

            // Derive the child's token from the current token.
            // This validates child status, parent ID match, and READ/ADMIN
            // capability, then computes child_caps = current_caps & !masked.
            let child_entry = self.map.get(&child_id).ok_or(ObjectError::ObjectNotFound)?;
            current_token = child_entry.value().derive_token(&current_token)?;
        }

        // Construct a Handle only for the final resolved object.
        let final_id = current_token.id.ok_or(ObjectError::InvalidArgument)?;
        let final_entry = self.map.get(&final_id).ok_or(ObjectError::ObjectNotFound)?;
        let final_obj = &final_entry.value().object;
        let final_handle = Handle::new(
            current_token,
            final_obj.registry,
            ObjectRef::Shared(Arc::downgrade(final_obj)),
        );

        Ok(f(final_handle))
    }
}
