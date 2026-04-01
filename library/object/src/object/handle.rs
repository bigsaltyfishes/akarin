use alloc::sync::{Arc, Weak};
use core::{
    fmt::Debug,
    sync::atomic::{AtomicU32, Ordering},
};

use atomig::Atomic;
use libakarin_syscall::{ObjectLifecycleFlags, SyscallResult};

use crate::{
    AdminOperation, ControlPlane, ObjectError, ObjectPath, ObjectStatus, Registry,
    object::{
        CpAccessMode, Object, ObjectSyscallContext, ReadOperation, WriteOperation,
        cap::{Capability, Token},
        container::ObjectContainer,
        cp_mode_allows,
    },
    registry::Index,
};

/// A concrete reference form used by [`Handle`].
pub enum ObjectRef {
    /// Shared global object reference.
    Shared(Weak<Object>),
    /// Owned anonymous object reference.
    Owned(Arc<Object>),
}

impl ObjectRef {
    /// Upgrade to one strong [`Arc`] reference.
    pub fn upgrade(&self) -> Option<Arc<Object>> {
        match self {
            Self::Shared(weak) => weak.upgrade(),
            Self::Owned(arc) => Some(arc.clone()),
        }
    }

    fn into_shared(&self) -> Self {
        match self {
            Self::Shared(weak) => Self::Shared(weak.clone()),
            Self::Owned(arc) => Self::Shared(Arc::downgrade(arc)),
        }
    }
}

/// A handle to an object, providing controlled access based on its token.
pub struct Handle {
    /// The token associated with the handle, defining the permissions and
    /// capabilities that the handle grants to its holder. This includes the
    /// index of the object associated with the handle and the specific
    /// capabilities that determine what operations can be performed on the
    /// object through this handle.
    token: Token,
    /// The reference to the registry that manages the lifecycle of the object
    /// associated with this handle. This allows the handle to interact with the
    /// registry for operations.
    registry: Option<&'static Registry>,
    /// The reference to the object that this handle points to. This allows the
    /// handle to interact with the object and perform operations on it based on
    /// the capabilities defined in its token.
    pub(crate) object: ObjectRef,
}

/// One-shot capability elevation permit.
///
/// A permit wraps a token issued by an `ADMIN` or `AGENT` handle, and can be
/// consumed by [`Handle::upgrade`] to raise another handle on the same object.
#[derive(Debug)]
pub struct Permit {
    token: Token,
    kind: PermitKind,
}

/// Permit behavior mode.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermitKind {
    OneShot,
    Permanent,
}

impl Permit {
    /// Temporarily upgrade one handle for one closure call.
    ///
    /// This API consumes one one-shot permit, creates one temporary elevated
    /// handle, invokes `f`, and drops the temporary handle immediately after
    /// the closure returns.
    pub fn upgrade_oneshot<F, R>(self, handle: &Handle, f: F) -> Result<R, ObjectError>
    where
        F: FnOnce(&Handle) -> R,
    {
        if self.kind != PermitKind::OneShot {
            return Err(ObjectError::InvalidArgument);
        }
        if !handle.is_valid() {
            return Err(ObjectError::ObjectDestroyed);
        }
        if self.token.id != handle.token.id {
            return Err(ObjectError::InvalidArgument);
        }

        let capabilities = handle.token.capabilities | self.token.capabilities;
        let interface_caps = handle.token.interface_caps | self.token.interface_caps;
        let temp = Handle {
            token: Token::new(handle.token.id, capabilities, interface_caps),
            registry: handle.registry,
            object: handle.object.into_shared(),
        };
        Ok(f(&temp))
    }
}

impl Debug for Handle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Handle")
            .field("token", &self.token)
            .finish()
    }
}

impl Handle {
    /// Create one anonymous owner handle.
    pub fn new_anonymous(payload: crate::Payload, masked_caps: Capability) -> Self {
        let object = Arc::new(Object {
            link: libakarin_collections::intrusive::DoubleLink::new(),
            registry: None,
            id: 0,
            parent: None,
            name: "#anonymous".into(),
            status: Atomic::new(ObjectStatus::Active),
            masked_caps: Atomic::new(masked_caps),
            lifecycle_flags: AtomicU32::new(ObjectLifecycleFlags::NONE.bits()),
            public_interface_caps: AtomicU32::new(0),
            payload,
            children: crate::Map::new(),
        });

        Self {
            token: Token::new(None, Capability::ADMIN_GRP, u32::MAX),
            registry: None,
            object: ObjectRef::Owned(object),
        }
    }

    pub(crate) const fn new(
        token: Token,
        registry: Option<&'static Registry>,
        object: ObjectRef,
    ) -> Self {
        Self {
            token,
            registry,
            object,
        }
    }

    /// Check if the handle is still valid, meaning that the object it points to
    /// has not been destroyed and the handle's token are still intact.
    pub fn is_valid(&self) -> bool {
        self.object
            .upgrade()
            .map(|underlying| underlying.status.load(Ordering::Acquire) == ObjectStatus::Active)
            .unwrap_or(false)
    }

    /// Check if the object is an anonymous object owned by this handle.
    pub fn is_anonymous(&self) -> bool {
        matches!(self.object, ObjectRef::Owned(_))
    }

    /// Get the index of the object associated with this handle.
    pub fn object_id(&self) -> Option<Index> {
        self.token.id
    }

    /// Return a stable in-process identity key for the referenced object.
    ///
    /// This key is based on the current object allocation address and is
    /// useful for tracking anonymous objects that have no registry id.
    pub fn object_key(&self) -> Result<usize, ObjectError> {
        let object = self.object.upgrade().ok_or(ObjectError::ObjectDestroyed)?;
        Ok(Arc::as_ptr(&object) as usize)
    }

    /// Get the capabilities associated with this handle.
    pub fn capabilities(&self) -> Capability {
        self.token.capabilities
    }

    /// Get the interface capabilities associated with this handle.
    pub fn interface_caps(&self) -> u32 {
        self.token.interface_caps
    }

    /// Read the lifecycle flags attached to the underlying object.
    pub fn lifecycle_flags(&self) -> Result<ObjectLifecycleFlags, ObjectError> {
        let object = self.object.upgrade().ok_or(ObjectError::ObjectDestroyed)?;
        Ok(object.lifecycle_flags())
    }

    /// Update the lifecycle flags attached to the underlying object.
    ///
    /// This bypasses the public object metadata traits and is intended for
    /// kernel-owned setup paths.
    pub fn set_lifecycle_flags(&self, flags: ObjectLifecycleFlags) -> Result<(), ObjectError> {
        let object = self.object.upgrade().ok_or(ObjectError::ObjectDestroyed)?;
        object.set_lifecycle_flags(flags);
        Ok(())
    }

    /// Downgrade the capabilities of this handle by removing the specified
    /// capabilities.
    ///
    /// # Arguments
    ///
    /// * `caps` - The capabilities to remove from the handle's token.
    /// * `interface_caps` - The interface capabilities to remove from the
    ///   handle's token.
    pub fn downgrade(&mut self, caps: Capability, interface_caps: u32) {
        self.token.downgrade(caps, interface_caps);
    }

    /// Issue one permanent permit from this handle.
    ///
    /// Only `ADMIN` or `AGENT` handles may issue permits. Permanent permits
    /// are consumed by [`Handle::upgrade`] and keep the elevated state.
    /// One-shot permits are consumed by [`Permit::upgrade_oneshot`] and only
    /// elevate for one closure call.
    pub fn issue_permit(
        &self,
        cap: Capability,
        oneshot: bool,
        interface_caps: u32,
    ) -> Result<Permit, ObjectError> {
        if !self.is_valid() {
            return Err(ObjectError::ObjectDestroyed);
        }
        if !self.capabilities().contains(Capability::ADMIN)
            && !self.capabilities().contains(Capability::AGENT)
        {
            return Err(ObjectError::InsufficientCapabilities);
        }

        if cap.contains(Capability::ADMIN) || cap.contains(Capability::AGENT) {
            return Err(ObjectError::InvalidArgument);
        }

        Ok(Permit {
            token: Token::new(self.token.id, cap, interface_caps),
            kind: if oneshot {
                PermitKind::OneShot
            } else {
                PermitKind::Permanent
            },
        })
    }

    /// Upgrade this handle using a consumed permit.
    ///
    /// The permit must target the same object.
    pub fn upgrade(&mut self, permit: Permit) -> Result<(), ObjectError> {
        if permit.kind != PermitKind::Permanent {
            return Err(ObjectError::InvalidArgument);
        }
        if !self.is_valid() {
            return Err(ObjectError::ObjectDestroyed);
        }
        if permit.token.id != self.token.id {
            return Err(ObjectError::InvalidArgument);
        }
        let capabilities = self.token.capabilities | permit.token.capabilities;
        let interface_caps = self.token.interface_caps | permit.token.interface_caps;
        self.token = Token::new(self.token.id, capabilities, interface_caps);
        Ok(())
    }

    /// Read the metadata of the object through this handle.
    ///
    /// # Arguments
    ///
    /// * `f` - A closure that takes a reference to the object's metadata and
    ///   returns a result.
    ///
    /// # Returns
    ///
    /// * `Ok(R)` - The result of the closure if the read operation is
    ///   successful.
    /// * `Err(ObjectError)` - An error indicating why the read operation
    ///   failed, such as if the object has been destroyed, if the handle does
    ///   not have the required capabilities, or if the handle's index does not
    ///   match the object's index.
    pub fn read_with<F, R>(&self, f: F) -> Result<R, ObjectError>
    where
        F: FnOnce(&dyn ReadOperation) -> R,
    {
        let object = self.object.upgrade().ok_or(ObjectError::ObjectDestroyed)?;
        let container = ObjectContainer::from(object);
        container.read_with(&self.token, f)
    }

    /// Write to the metadata of the object through this handle.
    ///
    /// # Arguments
    ///
    /// * `f` - A closure that takes a mutable reference to the object's
    ///   metadata and returns a result.
    ///
    /// # Returns
    ///
    /// * `Ok(R)` - The result of the closure if the write operation is
    ///   successful.
    /// * `Err(ObjectError)` - An error indicating why the write operation
    ///   failed, such as if the object has been destroyed, if the handle does
    ///   not have the required capabilities, or if the handle's index does not
    ///   match the object's index.
    pub fn write_with<F, R>(&self, f: F) -> Result<R, ObjectError>
    where
        F: FnOnce(&dyn WriteOperation) -> R,
    {
        let object = self.object.upgrade().ok_or(ObjectError::ObjectDestroyed)?;
        let container = ObjectContainer::from(object);
        container.write_with(&self.token, f)
    }

    /// Perform an administrative operation on the object through this handle.
    pub fn admin_with<F, R>(&self, f: F) -> Result<R, ObjectError>
    where
        F: FnOnce(&dyn AdminOperation) -> R,
    {
        let object = self.object.upgrade().ok_or(ObjectError::ObjectDestroyed)?;
        let container = ObjectContainer::from(object);
        container.admin_with(&self.token, f)
    }

    /// Read with Control Plane access to the object through this handle.
    ///
    /// # Arguments
    ///
    /// * `f` - A closure that takes a reference to the object's control plane
    ///   and returns a result.
    ///
    /// # Returns
    ///
    /// * `Ok(R)` - The result of the closure if the control plane read
    ///   operation is successful.
    /// * `Err(ObjectError)` - An error indicating why the control plane read
    ///   operation failed, such as if the object has been destroyed, if the
    ///   handle does not have the required capabilities, or if the handle's
    ///   index does not match the object's index.
    pub fn read_cp_with<T, F, R>(&self, f: F) -> Result<R, ObjectError>
    where
        T: ControlPlane + 'static,
        F: FnOnce(<T as ControlPlane>::ReadGuard<'_>) -> R,
    {
        let object = self.object.upgrade().ok_or(ObjectError::ObjectDestroyed)?;
        let container = ObjectContainer::from(object);
        container.read_cp_with(&self.token, f)
    }

    /// Write with Control Plane access to the object through this handle.
    ///
    /// # Arguments
    ///
    /// * `f` - A closure that takes a mutable reference to the object's control
    ///   plane and returns a result.
    ///
    /// # Returns
    ///
    /// * `Ok(R)` - The result of the closure if the control plane write
    ///   operation is successful.
    /// * `Err(ObjectError)` - An error indicating why the control plane write
    ///   operation failed, such as if the object has been destroyed, if the
    ///   handle does not have the required capabilities, or if the handle's
    ///   index does not match the object's index.
    pub fn write_cp_with<T, F, R>(&self, f: F) -> Result<R, ObjectError>
    where
        T: ControlPlane + 'static,
        F: FnOnce(<T as ControlPlane>::WriteGuard<'_>) -> R,
    {
        let object = self.object.upgrade().ok_or(ObjectError::ObjectDestroyed)?;
        let container = ObjectContainer::from(object);
        container.write_cp_with(&self.token, f)
    }

    /// Execute with Control Plane access to the object through this handle.
    ///
    /// # Arguments
    ///
    /// * `f` - A closure that takes a reference to the object's control plane
    ///   and returns a result.
    ///
    /// # Returns
    ///
    /// * `Ok(R)` - The result of the closure if the control plane execute
    ///   operation is successful.
    /// * `Err(ObjectError)` - An error indicating why the control plane execute
    ///   operation failed, such as if the object has been destroyed, if the
    ///   handle does not have the required capabilities, or if the handle's
    ///   index does not match the object's index.
    pub fn execute_cp_with<T, F, R>(&self, f: F) -> Result<R, ObjectError>
    where
        T: ControlPlane + 'static,
        F: FnOnce(<T as ControlPlane>::ExecuteGuard<'_>) -> R,
    {
        let object = self.object.upgrade().ok_or(ObjectError::ObjectDestroyed)?;
        let container = ObjectContainer::from(object);
        container.execute_cp_with(&self.token, f)
    }

    /// Agent with Control Plane access to the object through this handle.
    pub fn agent_cp_with<T, F, R>(&self, f: F) -> Result<R, ObjectError>
    where
        T: ControlPlane + 'static,
        F: FnOnce(<T as ControlPlane>::AgentGuard<'_>) -> R,
    {
        let object = self.object.upgrade().ok_or(ObjectError::ObjectDestroyed)?;
        let container = ObjectContainer::from(object);
        container.agent_cp_with(&self.token, f)
    }

    /// Admin with Control Plane access to the object through this handle.
    pub fn admin_cp_with<T, F, R>(&self, f: F) -> Result<R, ObjectError>
    where
        T: ControlPlane + 'static,
        F: FnOnce(<T as ControlPlane>::AdminGuard<'_>) -> R,
    {
        let object = self.object.upgrade().ok_or(ObjectError::ObjectDestroyed)?;
        let container = ObjectContainer::from(object);
        container.admin_cp_with(&self.token, f)
    }

    /// Read the metadata of the object through this handle.
    ///
    /// # Arguments
    ///
    /// * `f` - A closure that takes a reference to the object's metadata and
    ///   returns a result.
    ///
    /// # Returns
    ///
    /// * `Ok(R)` - The result of the closure if the read operation is
    ///   successful.
    /// * `Err(ObjectError)` - An error indicating why the read operation
    ///   failed, such as if the object has been destroyed, if the handle does
    ///   not have the required capabilities, or if the handle's index does not
    ///   match the object's index.
    pub async fn read_async_with<F, R>(&self, f: F) -> Result<R, ObjectError>
    where
        F: AsyncFnOnce(&dyn ReadOperation) -> R,
    {
        let object = self.object.upgrade().ok_or(ObjectError::ObjectDestroyed)?;
        let container = ObjectContainer::from(object);
        container.read_async_with(&self.token, f).await
    }

    /// Write to the metadata of the object through this handle.
    ///
    /// # Arguments
    ///
    /// * `f` - A closure that takes a mutable reference to the object's
    ///   metadata and returns a result.
    ///
    /// # Returns
    ///
    /// * `Ok(R)` - The result of the closure if the write operation is
    ///   successful.
    /// * `Err(ObjectError)` - An error indicating why the write operation
    ///   failed, such as if the object has been destroyed, if the handle does
    ///   not have the required capabilities, or if the handle's index does not
    ///   match the object's index.
    pub async fn write_async_with<F, R>(&self, f: F) -> Result<R, ObjectError>
    where
        F: AsyncFnOnce(&dyn WriteOperation) -> R,
    {
        let object = self.object.upgrade().ok_or(ObjectError::ObjectDestroyed)?;
        let container = ObjectContainer::from(object);
        container.write_async_with(&self.token, f).await
    }

    /// Perform an administrative operation on the object through this handle.
    pub async fn admin_async_with<F, R>(&self, f: F) -> Result<R, ObjectError>
    where
        F: AsyncFnOnce(&dyn AdminOperation) -> R,
    {
        let object = self.object.upgrade().ok_or(ObjectError::ObjectDestroyed)?;
        let container = ObjectContainer::from(object);
        container.admin_async_with(&self.token, f).await
    }

    /// Read with Control Plane access to the object through this handle.
    ///
    /// # Arguments
    ///
    /// * `f` - A closure that takes a reference to the object's control plane
    ///   and returns a result.
    ///
    /// # Returns
    ///
    /// * `Ok(R)` - The result of the closure if the control plane read
    ///   operation is successful.
    /// * `Err(ObjectError)` - An error indicating why the control plane read
    ///   operation failed, such as if the object has been destroyed, if the
    ///   handle does not have the required capabilities, or if the handle's
    ///   index does not match the object's index.
    pub async fn read_cp_async_with<T, F, R>(&self, f: F) -> Result<R, ObjectError>
    where
        T: ControlPlane + 'static,
        F: for<'a> AsyncFnOnce(<T as ControlPlane>::ReadGuard<'a>) -> R,
    {
        let object = self.object.upgrade().ok_or(ObjectError::ObjectDestroyed)?;
        let container = ObjectContainer::from(object);
        container.read_cp_async_with(&self.token, f).await
    }

    /// Write with Control Plane access to the object through this handle.
    ///
    /// # Arguments
    ///
    /// * `f` - A closure that takes a mutable reference to the object's control
    ///   plane and returns a result.
    ///
    /// # Returns
    ///
    /// * `Ok(R)` - The result of the closure if the control plane write
    ///   operation is successful.
    /// * `Err(ObjectError)` - An error indicating why the control plane write
    ///   operation failed, such as if the object has been destroyed, if the
    ///   handle does not have the required capabilities, or if the handle's
    ///   index does not match the object's index.
    pub async fn write_cp_async_with<T, F, R>(&self, f: F) -> Result<R, ObjectError>
    where
        T: ControlPlane + 'static,
        F: for<'a> AsyncFnOnce(<T as ControlPlane>::WriteGuard<'a>) -> R,
    {
        let object = self.object.upgrade().ok_or(ObjectError::ObjectDestroyed)?;
        let container = ObjectContainer::from(object);
        container.write_cp_async_with(&self.token, f).await
    }

    /// Execute with Control Plane access to the object through this handle.
    ///
    /// # Arguments
    ///
    /// * `f` - A closure that takes a reference to the object's control plane
    ///   and returns a result.
    ///
    /// # Returns
    ///
    /// * `Ok(R)` - The result of the closure if the control plane execute
    ///   operation is successful.
    /// * `Err(ObjectError)` - An error indicating why the control plane execute
    ///   operation failed, such as if the object has been destroyed, if the
    ///   handle does not have the required capabilities, or if the handle's
    ///   index does not match the object's index.
    pub async fn execute_cp_async_with<T, F, R>(&self, f: F) -> Result<R, ObjectError>
    where
        T: ControlPlane + 'static,
        F: for<'a> AsyncFnOnce(<T as ControlPlane>::ExecuteGuard<'a>) -> R,
    {
        let object = self.object.upgrade().ok_or(ObjectError::ObjectDestroyed)?;
        let container = ObjectContainer::from(object);
        container.execute_cp_async_with(&self.token, f).await
    }

    /// Agent with Control Plane access to the object through this handle.
    pub async fn agent_cp_async_with<T, F, R>(&self, f: F) -> Result<R, ObjectError>
    where
        T: ControlPlane + 'static,
        F: for<'a> AsyncFnOnce(<T as ControlPlane>::AgentGuard<'a>) -> R,
    {
        let object = self.object.upgrade().ok_or(ObjectError::ObjectDestroyed)?;
        let container = ObjectContainer::from(object);
        container.agent_cp_async_with(&self.token, f).await
    }

    /// Admin with Control Plane access to the object through this handle.
    pub async fn admin_cp_async_with<T, F, R>(&self, f: F) -> Result<R, ObjectError>
    where
        T: ControlPlane + 'static,
        F: for<'a> AsyncFnOnce(<T as ControlPlane>::AdminGuard<'a>) -> R,
    {
        let object = self.object.upgrade().ok_or(ObjectError::ObjectDestroyed)?;
        let container = ObjectContainer::from(object);
        container.admin_cp_async_with(&self.token, f).await
    }

    /// Dynamically dispatch one object-specific syscall through the selected
    /// control-plane guard.
    pub async fn invoke_cp(
        &self,
        caller: &ObjectSyscallContext,
        mode: CpAccessMode,
        method_id: usize,
        arg1: usize,
        arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let object = self.object.upgrade().ok_or(ObjectError::ObjectDestroyed)?;
        let container = ObjectContainer::from(object);
        cp_mode_allows(mode, self.capabilities())?;

        container
            .invoke_cp(&self.token, caller, mode, method_id, arg1, arg2)
            .await
    }

    /// Prevents the handle from being dropped, effectively leaking it.
    ///
    /// If the handle has ADMIN capabilities, the associated object will not be
    /// dropped when the handle is forgotten.
    ///
    /// # Safety
    ///
    /// This method leaks the handle, meaning that its resources will not be
    /// released. The caller is responsible for ensuring that this is the
    /// desired behavior and that it does not lead to resource leaks in the
    /// system.
    pub const unsafe fn forget(self) {
        core::mem::forget(self);
    }

    /// Derive a new handle with the specified capabilities from this handle.
    ///
    /// This method allows a privileged handle (with
    /// [`ADMIN_BIT`](Capability::ADMIN_BIT)
    /// or [`AGENT_BIT`](Capability::AGENT_BIT)) to create a lower-privilege
    /// handle with the specified capabilities. The derivable capabilities
    /// depend on the handle's privilege level:
    ///
    /// - **ADMIN** handles can derive all capabilities except
    ///   [`ADMIN`](Capability::ADMIN).
    /// - **AGENT** handles can derive all capabilities except
    ///   [`ADMIN`](Capability::ADMIN) and [`AGENT`](Capability::AGENT).
    ///
    /// The requested `cap` must be a subset of the derivable capabilities;
    /// otherwise, the derivation will fail.
    ///
    /// # Arguments
    ///
    /// * `cap` - The capabilities to grant to the derived handle.
    ///
    /// # Returns
    ///
    /// * `Ok(Handle)` - A new handle with the specified capabilities.
    /// * `Err(ObjectError)` - If the handle lacks the privilege to derive, or
    ///   the requested capabilities are not a subset of the derivable set.
    pub fn derive_handle(
        &self,
        cap: Capability,
        interface_caps: u32,
    ) -> Result<Handle, ObjectError> {
        if !self.is_valid() {
            return Err(ObjectError::ObjectDestroyed);
        }

        let (derivable, effective_interface_caps) =
            if self.capabilities().contains(Capability::ADMIN) {
                // ADMIN can derive everything except ADMIN itself
                (
                    Capability::CLONE
                        | Capability::SEND
                        | Capability::READ
                        | Capability::WRITE
                        | Capability::EXECUTE
                        | Capability::AGENT,
                    u32::MAX,
                )
            } else if self.capabilities().contains(Capability::AGENT) {
                // AGENT can derive everything except ADMIN and AGENT
                (
                    Capability::CLONE
                        | Capability::SEND
                        | Capability::READ
                        | Capability::WRITE
                        | Capability::EXECUTE,
                    u32::MAX,
                )
            } else if self.capabilities().contains(Capability::CLONE) {
                (self.token.capabilities, self.token.interface_caps)
            } else {
                return Err(ObjectError::InsufficientCapabilities);
            };

        if !derivable.contains(cap)
            || (interface_caps != 0 && !self.capabilities().contains(Capability::EXECUTE))
            || (effective_interface_caps & interface_caps) != interface_caps
        {
            return Err(ObjectError::InsufficientCapabilities);
        }

        Ok(Self {
            token: Token::new(self.token.id, cap, interface_caps),
            registry: self.registry,
            object: self.object.into_shared(),
        })
    }

    /// Acquire one kernel-internal reference to this handle without requiring
    /// `CLONE`.
    ///
    /// This is not a capability transfer primitive. It only creates an
    /// equivalent in-kernel reference so syscall and object code can hold a
    /// temporary owned handle across async boundaries without requiring public
    /// handles to be cloneable.
    pub fn acquire_ref(&self) -> Result<Handle, ObjectError> {
        if !self.is_valid() {
            return Err(ObjectError::ObjectDestroyed);
        }

        Ok(Self {
            token: self.token,
            registry: self.registry,
            object: self.object.into_shared(),
        })
    }

    /// Locate a child object relative to this handle.
    pub fn locate(&self, path: &str) -> Result<Handle, ObjectError> {
        self.registry
            .ok_or(ObjectError::InvalidArgument)?
            .locate_and_then(Some(self), &ObjectPath::new(path), |h| h)
    }

    /// Clone the handle with the same token and registry, but pointing to the
    /// same object.
    ///
    /// This method creates a new handle that shares the same token and registry
    /// as the original handle, but it does not clone the underlying object.
    /// The new handle will point to the same object as the original handle, and
    /// it will have the same capabilities and permissions defined in the token.
    ///
    /// # Returns
    ///
    /// * `Ok(Handle)` - A new handle that shares the same token and registry as
    ///   the original handle, pointing to the same object.
    /// * `Err(ObjectError)` - An error indicating why the cloning operation
    ///   failed, such as if the original handle has been destroyed or if the
    ///   original handle does not have the required capabilities to clone
    ///   itself.
    pub fn try_clone(&self) -> Result<Handle, ObjectError> {
        if !self.capabilities().contains(Capability::CLONE) {
            return Err(ObjectError::InsufficientCapabilities);
        }
        self.acquire_ref()
    }
}

impl AsRef<Token> for Handle {
    fn as_ref(&self) -> &Token {
        &self.token
    }
}

impl AsRef<Handle> for Handle {
    fn as_ref(&self) -> &Handle {
        self
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if self.capabilities().contains(Capability::ADMIN) {
            if let Some(registry) = self.registry {
                if self.object_id().is_some() {
                    match registry.remove(self, true) {
                        Ok(_) | Err(ObjectError::ObjectDestroyed) => {}
                        e => e.unwrap(),
                    }
                }
            }
        }
    }
}
