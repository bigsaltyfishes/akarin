mod cap;
mod container;
mod handle;

use alloc::{boxed::Box, string::String};
use core::{
    any::Any,
    mem::offset_of,
    ops::Deref,
    sync::atomic::{AtomicU32, Ordering},
};

use async_trait::async_trait;
use atomig::{Atom, Atomic};
pub use cap::{Capability, Token};
pub use container::ObjectContainer;
pub use handle::{Handle, ObjectRef, Permit};
use libakarin_collections::intrusive::{DoubleLink, ElememtOf};
use libakarin_macros::abstraction;
pub use libakarin_syscall::errno::ObjectError;
use libakarin_syscall::{
    CpAccessMode, ObjectLifecycleFlags, SyscallContext as GenericSyscallContext,
    SyscallDispatch as GenericSyscallDispatch, SyscallResult, UserCopyError,
};

use crate::{Map, MapIter, Registry, registry::Index};

pub type ObjectSyscallContext = dyn GenericSyscallContext<
        ObjectError = ObjectError,
        UserError = UserCopyError,
        Handle = Handle,
        Payload = Payload,
        Capability = Capability,
    >;

pub(crate) fn cp_mode_allows(mode: CpAccessMode, caps: Capability) -> Result<(), ObjectError> {
    let allowed = match mode {
        CpAccessMode::Read => caps.contains(Capability::READ),
        CpAccessMode::Write => caps.contains(Capability::WRITE),
        CpAccessMode::Execute => caps.contains(Capability::EXECUTE),
        CpAccessMode::Agent => caps.contains(Capability::AGENT),
        CpAccessMode::Admin => caps.contains(Capability::ADMIN),
    };

    if allowed {
        Ok(())
    } else {
        Err(ObjectError::InsufficientCapabilities)
    }
}

pub struct Payload {
    inner: Box<dyn ErasedControlPlane>,
}

impl Payload {
    pub fn new<T>(payload: T) -> Self
    where
        T: ControlPlane + Send + Sync + 'static,
        for<'a> T::ReadGuard<'a>: Send,
        for<'a> T::WriteGuard<'a>: Send,
        for<'a> T::ExecuteGuard<'a>: Send,
        for<'a> T::AgentGuard<'a>: Send,
        for<'a> T::AdminGuard<'a>: Send,
    {
        Self {
            inner: Box::new(payload),
        }
    }

    pub(crate) fn typed<T>(&self) -> Option<&T>
    where
        T: ControlPlane + 'static,
    {
        self.inner.as_any().downcast_ref::<T>()
    }

    pub(crate) async fn dispatch<'a>(
        &self,
        caller: &ObjectSyscallContext,
        mode: CpAccessMode,
        interface_caps: u32,
        method_id: usize,
        arg1: usize,
        arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        match mode {
            CpAccessMode::Read => {
                self.inner
                    .read_dispatch(caller, interface_caps, method_id, arg1, arg2)
            }
            CpAccessMode::Write => {
                self.inner
                    .write_dispatch(caller, interface_caps, method_id, arg1, arg2)
            }
            CpAccessMode::Execute => {
                self.inner
                    .execute_dispatch(caller, interface_caps, method_id, arg1, arg2)
            }
            CpAccessMode::Agent => {
                self.inner
                    .agent_dispatch(caller, interface_caps, method_id, arg1, arg2)
            }
            CpAccessMode::Admin => {
                self.inner
                    .admin_dispatch(caller, interface_caps, method_id, arg1, arg2)
            }
        }
        .await
    }
}

/// A reference to the payload of an object``.
pub struct PayloadRef<'a, T> {
    payload: &'a T,
    _container: ObjectContainer,
}

impl<T> Deref for PayloadRef<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.payload
    }
}

/// A special object that only acts as a container for organizing other objects.
///
/// Though all objects can have children, a NameSpace is intended to be used
/// purely for organizational purposes, and does not have any special
/// capabilities or payload. It is useful for creating a hierarchical structure
/// of objects, such as a filesystem-like namespace, without granting any
/// special permissions or capabilities to the objects themselves. This can help
/// with managing and organizing objects in a large system, while keeping the
/// access control and capabilities of the objects separate from their
/// organizational structure.
pub struct NameSpace;

impl ControlPlane for NameSpace {
    type ReadGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type WriteGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type AgentGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type AdminGuard<'a>
        = &'a Self
    where
        Self: 'a;

    fn read(&self, _interface_caps: u32) -> Self::ReadGuard<'_> {
        self
    }

    fn write(&self, _interface_caps: u32) -> Self::WriteGuard<'_> {
        self
    }

    fn execute(&self, _interface_caps: u32) -> Self::ExecuteGuard<'_> {
        self
    }

    fn agent(&self, _interface_caps: u32) -> Self::AgentGuard<'_> {
        self
    }

    fn admin(&self, _interface_caps: u32) -> Self::AdminGuard<'_> {
        self
    }
}

#[async_trait]
impl GenericSyscallDispatch<ObjectSyscallContext> for NameSpace {}

#[async_trait]
trait ErasedControlPlane: Any + Send + Sync {
    fn as_any(&self) -> &(dyn Any + Send + Sync + 'static);

    async fn read_dispatch(
        &self,
        caller: &ObjectSyscallContext,
        interface_caps: u32,
        method_id: usize,
        arg1: usize,
        arg2: usize,
    ) -> Result<SyscallResult, ObjectError>;

    async fn write_dispatch(
        &self,
        caller: &ObjectSyscallContext,
        interface_caps: u32,
        method_id: usize,
        arg1: usize,
        arg2: usize,
    ) -> Result<SyscallResult, ObjectError>;

    async fn execute_dispatch(
        &self,
        caller: &ObjectSyscallContext,
        interface_caps: u32,
        method_id: usize,
        arg1: usize,
        arg2: usize,
    ) -> Result<SyscallResult, ObjectError>;

    async fn agent_dispatch(
        &self,
        caller: &ObjectSyscallContext,
        interface_caps: u32,
        method_id: usize,
        arg1: usize,
        arg2: usize,
    ) -> Result<SyscallResult, ObjectError>;

    async fn admin_dispatch(
        &self,
        caller: &ObjectSyscallContext,
        interface_caps: u32,
        method_id: usize,
        arg1: usize,
        arg2: usize,
    ) -> Result<SyscallResult, ObjectError>;
}

#[async_trait]
impl<T> ErasedControlPlane for T
where
    T: ControlPlane + Send + Sync + 'static,
    for<'a> T::ReadGuard<'a>: Send,
    for<'a> T::WriteGuard<'a>: Send,
    for<'a> T::ExecuteGuard<'a>: Send,
    for<'a> T::AgentGuard<'a>: Send,
    for<'a> T::AdminGuard<'a>: Send,
{
    fn as_any(&self) -> &(dyn Any + Send + Sync + 'static) {
        self
    }

    async fn read_dispatch(
        &self,
        caller: &ObjectSyscallContext,
        interface_caps: u32,
        method_id: usize,
        arg1: usize,
        arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let guard = self.read(interface_caps);
        guard.dispatch(caller, method_id, arg1, arg2).await
    }

    async fn write_dispatch(
        &self,
        caller: &ObjectSyscallContext,
        interface_caps: u32,
        method_id: usize,
        arg1: usize,
        arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let guard = self.write(interface_caps);
        guard.dispatch(caller, method_id, arg1, arg2).await
    }

    async fn execute_dispatch(
        &self,
        caller: &ObjectSyscallContext,
        interface_caps: u32,
        method_id: usize,
        arg1: usize,
        arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let guard = self.execute(interface_caps);
        guard.dispatch(caller, method_id, arg1, arg2).await
    }

    async fn agent_dispatch(
        &self,
        caller: &ObjectSyscallContext,
        interface_caps: u32,
        method_id: usize,
        arg1: usize,
        arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let guard = self.agent(interface_caps);
        guard.dispatch(caller, method_id, arg1, arg2).await
    }

    async fn admin_dispatch(
        &self,
        caller: &ObjectSyscallContext,
        interface_caps: u32,
        method_id: usize,
        arg1: usize,
        arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let guard = self.admin(interface_caps);
        guard.dispatch(caller, method_id, arg1, arg2).await
    }
}

/* legacy ObjectError definition moved to libakarin_syscall::errno::ObjectError
pub enum ObjectError {
    /// The object has been destroyed, and the handle is no longer valid.
    #[error("object has been destroyed")]
    ObjectDestroyed,
    /// The handle does not have the required capabilities to perform the
    /// requested operation.
    #[error("insufficient capabilities")]
    InsufficientCapabilities,
    /// The caller provided an invalid argument, such as an invalid type
    /// for the payload.
    #[error("invalid argument provided")]
    InvalidArgument,
    /// The specified object does not exist.
    #[error("object not found")]
    ObjectNotFound,
    /// An attempt was made to create an object with a name that is reserved
    /// or otherwise not allowed.
    #[error("dangerous name")]
    DangerousName,
    /// An attempt was made to create a child object with a name that already
    /// exists.
    #[error("duplicate child name")]
    DuplicateChildName,
    /// An attempt was made to create a [`AGENT`] handle on an object that
    /// already has an existing [`AGENT`] handle.
    #[error("agent handle already exists for object")]
    AgentHandleAlreadyExists,
}

impl ObjectError {
    /// Return the stable ABI code used when one syscall reports this as one
    /// top-level `ObjectError`.
    pub const fn abi_code(self) -> usize {
        match self {
            Self::ObjectDestroyed => 1,
            Self::InsufficientCapabilities => 2,
            Self::InvalidArgument => 3,
            Self::ObjectNotFound => 4,
            Self::DangerousName => 5,
            Self::DuplicateChildName => 6,
            Self::AgentHandleAlreadyExists => 7,
        }
    }

    /// Decode one stable ABI code back into one object error value.
    pub const fn from_abi_code(code: usize) -> Option<Self> {
        match code {
            1 => Some(Self::ObjectDestroyed),
            2 => Some(Self::InsufficientCapabilities),
            3 => Some(Self::InvalidArgument),
            4 => Some(Self::ObjectNotFound),
            5 => Some(Self::DangerousName),
            6 => Some(Self::DuplicateChildName),
            7 => Some(Self::AgentHandleAlreadyExists),
            _ => None,
        }
    }
}
*/
#[repr(u8)]
#[derive(Atom, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectStatus {
    /// The object is active and can be interacted with through its handles.
    Active,
    /// The object is in the process of being destroyed, and its handles are
    /// being invalidated. During this state, the object may still exist, but
    /// it should not be interacted with as its state is unstable.
    Destroying,
}

/// The main object struct that represents an entity in the system with
/// associated metadata and payload.
pub struct Object {
    /// The intrusive link for destroying objects. This allows the object to be
    /// part of a linked list of objects that are pending destruction, enabling
    /// efficient cleanup and resource management when objects are destroyed.
    pub(crate) link: DoubleLink,
    /// The reference to the registry that manages this object.
    pub(crate) registry: Option<&'static Registry>,
    /// The index of the object in the global object manager.
    pub(crate) id: Index,
    /// The parent index of the object, which can be used to establish
    /// hierarchical relationships between objects. This allows for organizing
    /// objects in a tree-like structure, where each object can have a parent
    /// object and multiple child objects, facilitating structured management
    /// and access control.
    pub(crate) parent: Option<Index>,
    /// The name of the object, which can be used for debugging and
    /// identification purposes.
    pub(crate) name: String,
    /// The status of the object, which indicates whether the object is active,
    /// being destroyed, or has been destroyed. This allows for proper handling
    /// of the object's lifecycle and ensures that operations on the object are
    /// performed only when the object is in a valid state.
    pub(crate) status: Atomic<ObjectStatus>,
    /// Masked capabilities of the object, which define the permissions that
    /// must be given up when creating a handle for this object. This allows the
    /// object to enforce certain restrictions on the handles that can be
    /// created for it, ensuring that only handles with appropriate
    /// permissions can be used to interact with the object.
    pub(crate) masked_caps: Atomic<Capability>,
    /// Lifecycle attributes that affect teardown policy.
    pub(crate) lifecycle_flags: AtomicU32,
    /// Public interface capabilities automatically attached to handles
    /// derived through namespace traversal when the resulting handle does not
    /// carry `ADMIN` or `AGENT`.
    pub(crate) public_interface_caps: AtomicU32,
    /// The payload of the object, which can be any type that implements `Any`
    /// and `Sync`.
    pub(crate) payload: Payload,
    /// Children objects of this object. This allows for hierarchical
    /// relationships between objects, where an object can have multiple
    /// child objects.
    pub(crate) children: Map<String, Index>,
}

impl Object {
    pub(crate) fn payload<T: ControlPlane + 'static>(&self) -> Result<&T, ObjectError> {
        self.payload
            .typed::<T>()
            .ok_or(ObjectError::InvalidArgument)
    }

    pub fn lifecycle_flags(&self) -> ObjectLifecycleFlags {
        ObjectLifecycleFlags::from_bits(self.lifecycle_flags.load(Ordering::Acquire))
            .unwrap_or(ObjectLifecycleFlags::NONE)
    }

    pub fn set_lifecycle_flags(&self, flags: ObjectLifecycleFlags) {
        self.lifecycle_flags.store(flags.bits(), Ordering::Release);
    }

    pub fn is_sticky(&self) -> bool {
        self.lifecycle_flags()
            .contains(ObjectLifecycleFlags::STICKY)
    }
}

#[abstraction(ReadOperation, visibility = "public")]
impl ReadOperation for Object {
    /// Get the index of the object.
    fn id(&self) -> Index {
        self.id
    }

    /// Get the parent index of the object, if it has a parent.
    fn parent(&self) -> Option<Index> {
        self.parent
    }

    /// Get the name of the object.
    fn name(&self) -> &str {
        &self.name
    }

    /// Get the masked capabilities of the object.
    fn masked_caps(&self) -> Capability {
        self.masked_caps.load(Ordering::Acquire)
    }

    /// Get the lifecycle flags of the object.
    fn lifecycle_flags(&self) -> ObjectLifecycleFlags {
        Object::lifecycle_flags(self)
    }

    /// Get the public interface capabilities of the object.
    fn public_interface_caps(&self) -> u32 {
        self.public_interface_caps.load(Ordering::Acquire)
    }

    /// Get the list of child objects.
    fn children(&self) -> MapIter<'_, String, Index> {
        self.children.iter()
    }

    /// Query the id of a child object by its name.
    fn query_child(&self, name: &str) -> Result<Index, ObjectError> {
        let ent = self.children.get(name).ok_or(ObjectError::ObjectNotFound)?;
        Ok(*ent.value())
    }

    /// Get the status of the object.
    fn status(&self) -> ObjectStatus {
        self.status.load(Ordering::Acquire)
    }
}

#[abstraction(WriteOperation, visibility = "public")]
impl WriteOperation for Object {
    /// Set the masked capabilities of the object.
    fn set_masked_caps(&self, caps: Capability) {
        self.masked_caps.store(caps, Ordering::Release);
    }

    /// Set the public interface capabilities of the object.
    fn set_public_interface_caps(&self, caps: u32) {
        self.public_interface_caps.store(caps, Ordering::Release);
    }

    /// Add a child object to this object.
    fn add_child(
        &self,
        name: String,
        masked_caps: Capability,
        payload: Payload,
    ) -> Result<Handle, ObjectError> {
        // Reject dangerous names that can cause confusion or
        // conflicts in the object hierarchy.
        if name.is_empty() || name == "self" {
            return Err(ObjectError::DangerousName);
        }

        // Fast path: reject duplicate name before allocating.
        if self.children.get(&name).is_some() {
            return Err(ObjectError::DuplicateChildName);
        }

        let object = ObjectContainer::new(|| Object {
            link: DoubleLink::new(),
            registry: self.registry,
            id: self
                .registry
                .expect("namespace object missing registry")
                .idx_allocator
                .allocate(),
            parent: Some(self.id),
            name: name.clone(),
            status: Atomic::new(ObjectStatus::Active),
            masked_caps: Atomic::new(masked_caps),
            lifecycle_flags: AtomicU32::new(ObjectLifecycleFlags::NONE.bits()),
            public_interface_caps: AtomicU32::new(0),
            payload,
            children: Map::new(),
        });
        let id = object.object.id;
        // Keep an Arc reference to allow rollback via remove_internal.
        let child_arc = object.object.clone();

        // Step 1: Insert the child into the registry first.
        // This ensures that if a concurrent BFS destruction traverses the
        // parent's children, it will find the child in the registry map
        // and won't trigger a "Corrupted Object Registry" panic.
        // The child is not yet reachable through the object tree, so no
        // external observer can access it at this point.
        let handle = self
            .registry
            .expect("namespace object missing registry")
            .insert_internal(object)?;

        // Step 2: Record the child in the parent's children map.
        // compare_insert with |_| false means "never replace existing".
        // The skiplist's level-0 CAS serialises concurrent inserts of the
        // same key: exactly one thread wins the CAS and its value is stored;
        // all other threads observe the existing node and return it.  We
        // therefore detect duplicates by checking whether the returned
        // entry's value matches the id we tried to insert.
        //
        // The rollback path calls remove_internal on the loser's child.
        // remove_internal conditionally removes from parent.children only
        // when the stored value matches the child being destroyed, so the
        // winner's entry is never accidentally removed.
        let entry = self.children.compare_insert(name, id, |_| false);
        if *entry.value() != id {
            // Another thread inserted a child with the same name concurrently.
            // Rollback: destroy the child we just inserted into the registry.
            // Since no one can reach the child through the tree, remove_internal
            // will cleanly reclaim it. The ADMIN handle's Drop will observe
            // ObjectDestroyed and gracefully no-op.
            let _ = self
                .registry
                .expect("namespace object missing registry")
                .remove_internal(child_arc);
            return Err(ObjectError::DuplicateChildName);
        }

        // Step 3: After the child is recorded in the parent's children, check
        // whether the parent is being destroyed. This is the critical
        // synchronization point with remove_internal's BFS:
        //
        // - If the parent's status is still Active, the BFS has not started yet. When
        //   it does start, it will see this child in the parent's children map and
        //   properly destroy it.
        //
        // - If the parent's status is Destroying, the BFS may or may not have already
        //   traversed the parent's children. In either case, we proactively destroy the
        //   child. If the BFS has already processed it, remove_internal will observe
        //   the child's status as Destroying and return ObjectDestroyed (which we
        //   ignore). If the BFS has not yet reached it, the BFS will later skip it
        //   (already Destroying).
        if self.status.load(Ordering::Acquire) != ObjectStatus::Active {
            // The parent is being destroyed.  Delegate the full teardown
            // (status CAS + BFS + reclamation) to remove_internal so that
            // the child is not left as a zombie in the registry.
            // If the BFS has already processed this child, remove_internal
            // will observe status == Destroying and return ObjectDestroyed,
            // which we safely ignore.
            let _ = self
                .registry
                .expect("namespace object missing registry")
                .remove_internal(child_arc);
            return Err(ObjectError::ObjectDestroyed);
        }

        drop(child_arc);
        Ok(handle)
    }

    /// Remove a child object from this object by its name.
    fn remove_child(&self, name: &str, is_kernel: bool) -> Result<Index, ObjectError> {
        if !is_kernel && self.is_sticky() {
            // Sticky objects can only be removed by the kernel.
            return Err(ObjectError::InvalidArgument);
        }

        let child_id = *self
            .children
            .get(name)
            .ok_or(ObjectError::ObjectNotFound)?
            .value();
        let child_object = self
            .registry
            .expect("namespace object missing registry")
            .map
            .get(&child_id)
            .ok_or(ObjectError::ObjectNotFound)?
            .value();
        if child_object
            .object
            .masked_caps
            .load(Ordering::Acquire)
            .contains(Capability::WRITE)
        {
            return Err(ObjectError::InsufficientCapabilities);
        }

        self.children
            .remove(name)
            .ok_or(ObjectError::ObjectNotFound)?;

        let child_object = child_object.object.clone();

        self.registry
            .expect("namespace object missing registry")
            .remove_internal(child_object)?;

        Ok(child_id)
    }
}

/// A trait for control planes that can create user-level guards with specific
/// capabilities.
pub trait ControlPlane {
    /// The guard type that will be returned by the `read` method.
    ///
    /// A guard should verify interface capabilities and provide read access.
    type ReadGuard<'a>: GenericSyscallDispatch<ObjectSyscallContext>
    where
        Self: 'a;

    /// The guard type that will be returned by the `write` method.
    ///
    /// A guard should verify interface capabilities and provide write access.
    type WriteGuard<'a>: GenericSyscallDispatch<ObjectSyscallContext>
    where
        Self: 'a;

    /// The guard type that will be returned by the `execute` method.
    ///
    /// A guard should verify interface capabilities and provide execute access.
    type ExecuteGuard<'a>: GenericSyscallDispatch<ObjectSyscallContext>
    where
        Self: 'a;

    /// The guard type that will be returned by the `agent` method.
    ///
    /// A guard should verify interface capabilities and provide full access,
    /// and the ability to create other agent handles. This is typically
    /// used for long-lived manager objects that need to delegate work to
    /// short-lived agent.
    type AgentGuard<'a>: GenericSyscallDispatch<ObjectSyscallContext>
    where
        Self: 'a;

    /// The guard type that will be returned by the `admin` method.
    ///
    /// A guard should verify interface capabilities and provide full access,
    /// and the ability to create other admin handles. This is typically used
    /// for privileged management tasks that require the highest level of
    /// access and control over the object, such as modifying its structure or
    /// capabilities, and managing its lifecycle. Admin handles should be
    /// granted only to trusted components, as they have the potential to
    /// significantly impact the behavior and security of the object.
    type AdminGuard<'a>: GenericSyscallDispatch<ObjectSyscallContext>
    where
        Self: 'a;

    /// Create a read guard for this control plane.
    fn read(&self, interface_caps: u32) -> Self::ReadGuard<'_>;

    /// Create a write guard for this control plane.
    fn write(&self, interface_caps: u32) -> Self::WriteGuard<'_>;

    /// Create an execute guard for this control plane.
    fn execute(&self, interface_caps: u32) -> Self::ExecuteGuard<'_>;

    /// Create an agent guard for this control plane.
    fn agent(&self, interface_caps: u32) -> Self::AgentGuard<'_>;

    /// Create an admin guard for this control plane.
    fn admin(&self, interface_caps: u32) -> Self::AdminGuard<'_>;
}

#[abstraction(AdminOperation, visibility = "public")]
impl AdminOperation for Object {
    /// Remove a child object from this object by its name.
    fn remove_child(&self, name: &str, is_kernel: bool) -> Result<Index, ObjectError> {
        if is_kernel && self.is_sticky() {
            // Sticky objects can only be removed by the kernel.
            return Err(ObjectError::InvalidArgument);
        }

        let child_id = *self
            .children
            .remove(name)
            .ok_or(ObjectError::ObjectNotFound)?
            .value();
        let child_object = self
            .registry
            .expect("namespace object missing registry")
            .map
            .get(&child_id)
            .ok_or(ObjectError::ObjectNotFound)?
            .value();
        self.registry
            .expect("namespace object missing registry")
            .remove_internal(child_object.object.clone())?;

        Ok(child_id)
    }
}

impl ElememtOf<Self, DoubleLink> for Object {
    fn element(link: &DoubleLink) -> &Self {
        let ptr = (link as *const DoubleLink).cast::<u8>();
        let offset = offset_of!(Object, link);
        unsafe { &*(ptr.sub(offset).cast::<Object>()) }
    }

    fn element_mut(link: &mut DoubleLink) -> &mut Self {
        let ptr = (link as *mut DoubleLink).cast::<u8>();
        let offset = offset_of!(Object, link);
        unsafe { &mut *(ptr.sub(offset).cast::<Object>()) }
    }

    fn link(node: &Self) -> &DoubleLink {
        &node.link
    }

    fn link_mut(node: &mut Self) -> &mut DoubleLink {
        &mut node.link
    }
}
