use alloc::sync::Arc;
use core::{ops::Deref, sync::atomic::Ordering};

use libakarin_syscall::SyscallResult;

use crate::{
    ControlPlane, ObjectError, ObjectStatus,
    object::{
        AdminOperation, CpAccessMode, Object, ObjectSyscallContext, ReadOperation, WriteOperation,
        cap::{Capability, Token},
        cp_mode_allows,
        handle::{Handle, ObjectRef},
    },
};

/// A container holding a reference to an object.
///
/// This struct is used to manage access to the object and its metadata,
/// ensuring that the appropriate checks are performed when acquiring handles or
/// accessing the object's payload. It serves as an intermediary between the
/// handle and the object, allowing for controlled access to the object's data
/// and functionality while enforcing the necessary permissions and capabilities
/// defined in the handle's token.
pub struct ObjectContainer {
    pub(crate) object: Arc<Object>,
}

impl From<Arc<Object>> for ObjectContainer {
    fn from(object: Arc<Object>) -> Self {
        Self { object }
    }
}

impl ObjectContainer {
    /// Create a new object container by invoking the provided closure to
    /// generate the object.
    ///
    /// # Arguments
    ///
    /// * `f` - A closure that generates the object to be contained within this
    ///   container.
    ///
    /// # Returns
    ///
    /// * `Self` - A new instance of `ObjectContainer` containing the generated
    ///   object.
    pub fn new<F>(f: F) -> Self
    where
        F: FnOnce() -> Object,
    {
        Self {
            object: Arc::new(f()),
        }
    }

    /// Verify that the provided token has the required capabilities to access
    /// the object.
    fn verify_capabilities(&self, token: &Token, required: Capability) -> Result<(), ObjectError> {
        let status = self.object.status.load(Ordering::Acquire);
        if status != ObjectStatus::Active {
            return Err(ObjectError::ObjectDestroyed);
        }

        if let Some(id) = token.id {
            if id != self.object.id {
                return Err(ObjectError::InvalidArgument);
            }
        }

        // Check if the token has the required capabilities.
        // Since ADMIN and AGENT are composite capabilities that include the base
        // permission bits (READ, WRITE, EXECUTE), no special bypass is needed.
        if !token.contains(required) {
            return Err(ObjectError::InsufficientCapabilities);
        }

        Ok(())
    }

    /// Derive a new token for this object from the given parent token.
    ///
    /// This method performs the same validation as
    /// [`acquire_handle`](Self::acquire_handle), checking that:
    /// - The object is in [`Active`](ObjectStatus::Active) state.
    /// - The parent token's ID matches this object's parent ID.
    /// - The parent token has [`READ`](Capability::READ) or
    ///   [`ADMIN_BIT`](Capability::ADMIN_BIT) capability.
    ///
    /// The derived token's capabilities are computed as the parent token's
    /// capabilities with this object's masked capabilities removed.
    ///
    /// Unlike [`acquire_handle`](Self::acquire_handle), this method only
    /// returns a [`Token`] without constructing a [`Handle`], avoiding the
    /// overhead of `Arc`/`Weak` reference counting operations. This is
    /// intended for internal use in path traversal where intermediate
    /// handles are unnecessary.
    ///
    /// # Arguments
    ///
    /// * `parent_token` - The token of the parent object. Its `id` must match
    ///   this object's parent index.
    ///
    /// # Returns
    ///
    /// * `Ok(Token)` - A new token for this object with derived capabilities.
    /// * `Err(ObjectError)` - If the object is destroyed, the parent ID does
    ///   not match, or the parent token lacks required capabilities.
    pub(crate) fn derive_token<R>(&self, parent_token: R) -> Result<Token, ObjectError>
    where
        R: AsRef<Token>,
    {
        let parent_token = parent_token.as_ref();
        let status = self.object.status.load(Ordering::Acquire);
        if status != ObjectStatus::Active {
            return Err(ObjectError::ObjectDestroyed);
        }

        if parent_token.id != self.object.parent {
            return Err(ObjectError::InvalidArgument);
        }

        if !parent_token.contains(Capability::READ) {
            return Err(ObjectError::InsufficientCapabilities);
        }

        let masked_caps = self.object.masked_caps.load(Ordering::Acquire);
        let final_caps = parent_token.capabilities & !masked_caps;
        let interface_caps =
            if final_caps.contains(Capability::ADMIN) || final_caps.contains(Capability::AGENT) {
                u32::MAX
            } else {
                self.object.public_interface_caps.load(Ordering::Acquire)
            };
        let append_caps = if (masked_caps.contains(Capability::ADMIN)
            || masked_caps.contains(Capability::AGENT))
            && (parent_token.contains(Capability::ADMIN)
                || parent_token.contains(Capability::AGENT))
        {
            Capability::CLONE | Capability::SEND
        } else {
            Capability::empty()
        };
        Ok(Token::new(
            Some(self.object.id),
            parent_token.capabilities & !masked_caps | append_caps,
            interface_caps,
        ))
    }

    /// Acquire a handle for this object with the specified parent token.
    ///
    /// This method checks the status of the object and the capabilities of the
    /// parent handle to determine if a new handle can be acquired for this
    /// object. If the object is active and the parent handle has the required
    /// capabilities, a new handle is created with capabilities derived from the
    /// parent handle's capabilities and the object's masked capabilities. The
    /// new handle's capabilities will be the intersection of the parent
    /// handle's capabilities and the object's capabilities, excluding any
    /// capabilities that are masked by the object. This ensures that the new
    /// handle does not have any capabilities that are explicitly restricted by
    /// the object, while still allowing it to inherit any capabilities that the
    /// parent handle has and are not masked by the object.
    ///
    /// # Arguments
    ///
    /// * `parent_right` - The token of the parent handle that is requesting to
    ///   acquire a handle for this object. This includes the index of the
    ///   parent object and the capabilities associated with the parent handle.
    ///
    /// # Returns
    ///
    /// * `Ok(Handle)` - A new handle for this object if the acquisition is
    ///   successful.
    /// * `Err(ObjectError)` - An error indicating why the handle acquisition
    ///   failed, such as if the object has been destroyed, if the parent handle
    ///   does not have the required capabilities, or if the parent handle's
    ///   index does not match the object's parent index.
    pub fn acquire_handle<R>(&self, parent_right: R) -> Result<Handle, ObjectError>
    where
        R: AsRef<Token>,
    {
        let token = self.derive_token(parent_right.as_ref())?;
        Ok(Handle::new(
            token,
            self.object.registry,
            ObjectRef::Shared(Arc::downgrade(&self.object)),
        ))
    }

    /// Read the metadata of the object through this container.
    ///
    /// # Arguments
    ///
    /// * `token` - The token of the handle that is requesting to read the
    ///   metadata. This includes the index of the object and the capabilities
    ///   associated with the handle.
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
    pub fn read_with<T, F, R>(&self, token: T, f: F) -> Result<R, ObjectError>
    where
        T: AsRef<Token>,
        F: FnOnce(&dyn ReadOperation) -> R,
    {
        let token = token.as_ref();
        self.verify_capabilities(token, Capability::READ)?;

        Ok(f(self.object.deref()))
    }

    /// Write to the metadata of the object through this container.
    ///
    /// # Arguments
    ///
    /// * `token` - The token of the handle that is requesting to write to the
    ///   metadata. This includes the index of the object and the capabilities
    ///   associated with the handle.
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
    pub fn write_with<T, F, R>(&self, token: T, f: F) -> Result<R, ObjectError>
    where
        T: AsRef<Token>,
        F: FnOnce(&dyn WriteOperation) -> R,
    {
        let token = token.as_ref();
        self.verify_capabilities(token, Capability::WRITE)?;

        Ok(f(self.object.deref()))
    }

    /// Perform an administrative operation on the object through this
    /// container.
    ///
    /// # Arguments
    ///
    /// * `token` - The token of the handle that is requesting to perform the
    ///   administrative operation. This includes the index of the object and
    ///   the capabilities associated with the handle.
    /// * `f` - A closure that takes a reference to the object's administrative
    ///   interface and returns a result.
    ///
    /// # Returns
    ///
    /// * `Ok(R)` - The result of the closure if the administrative operation is
    ///   successful.
    /// * `Err(ObjectError)` - An error indicating why the administrative
    ///   operation failed, such as if the object has been destroyed, if the
    ///   handle does not have the required capabilities, or if the handle's
    ///   index does not match the object's index.
    pub fn admin_with<T, F, R>(&self, token: T, f: F) -> Result<R, ObjectError>
    where
        T: AsRef<Token>,
        F: FnOnce(&dyn AdminOperation) -> R,
    {
        let token = token.as_ref();
        self.verify_capabilities(token, Capability::ADMIN_GRP)?;

        Ok(f(self.object.deref()))
    }

    /// Read with the control plane of the object through this container.
    ///
    /// # Arguments
    ///
    /// * `token` - The token of the handle that is requesting to read with the
    ///   control plane. This includes the index of the object and the
    ///   capabilities associated with the handle.
    /// * `f` - A closure that takes a reference to the control plane's read
    ///   guard and returns a result.
    ///
    /// # Returns
    ///
    /// * `Ok(R)` - The result of the closure if the read operation is
    ///   successful.
    /// * `Err(ObjectError)` - An error indicating why the read operation
    ///   failed, such as if the object has been destroyed, if the handle does
    ///   not have the required capabilities, or if the handle's index does not
    ///   match the object's index.
    pub fn read_cp_with<O, T, F, R>(&self, token: T, f: F) -> Result<R, ObjectError>
    where
        T: AsRef<Token>,
        O: ControlPlane + 'static,
        F: FnOnce(<O as ControlPlane>::ReadGuard<'_>) -> R,
    {
        let token = token.as_ref();
        self.verify_capabilities(token, Capability::READ)?;
        let object = self.object.payload::<O>()?;

        Ok(f(object.read(token.interface_caps())))
    }

    /// Write with the control plane of the object through this container.
    ///
    /// # Arguments
    ///
    /// * `token` - The token of the handle that is requesting to write with the
    ///   control plane. This includes the index of the object and the
    ///   capabilities associated with the handle.
    /// * `f` - A closure that takes a reference to the control plane's write
    ///   guard and returns a result.
    ///
    /// # Returns
    ///
    /// * `Ok(R)` - The result of the closure if the write operation is
    ///   successful.
    /// * `Err(ObjectError)` - An error indicating why the write operation
    ///   failed, such as if the object has been destroyed, if the handle does
    ///   not have the required capabilities, or if the handle's index does not
    ///   match the object's index.
    pub fn write_cp_with<O, T, F, R>(&self, token: T, f: F) -> Result<R, ObjectError>
    where
        T: AsRef<Token>,
        O: ControlPlane + 'static,
        F: FnOnce(<O as ControlPlane>::WriteGuard<'_>) -> R,
    {
        let token = token.as_ref();
        self.verify_capabilities(token, Capability::WRITE)?;
        let object = self.object.payload::<O>()?;

        Ok(f(object.write(token.interface_caps())))
    }

    /// Execute with the control plane of the object through this container.
    ///
    /// # Arguments
    ///
    /// * `token` - The token of the handle that is requesting to execute with
    ///   the control plane. This includes the index of the object and the
    ///   capabilities associated with the handle.
    /// * `f` - A closure that takes a reference to the control plane's execute
    ///   guard and returns a result.
    ///
    /// # Returns
    ///
    /// * `Ok(R)` - The result of the closure if the execute operation is
    ///   successful.
    /// * `Err(ObjectError)` - An error indicating why the execute operation
    ///   failed, such as if the object has been destroyed, if the handle does
    ///   not have the required capabilities, or if the handle's index does not
    ///   match the object's index.
    pub fn execute_cp_with<O, T, F, R>(&self, token: T, f: F) -> Result<R, ObjectError>
    where
        T: AsRef<Token>,
        O: ControlPlane + 'static,
        F: FnOnce(<O as ControlPlane>::ExecuteGuard<'_>) -> R,
    {
        let token = token.as_ref();
        self.verify_capabilities(token, Capability::EXECUTE)?;
        let object = self.object.payload::<O>()?;

        Ok(f(object.execute(token.interface_caps())))
    }

    /// Agent with the control plane of the object through this container.
    pub fn agent_cp_with<O, T, F, R>(&self, token: T, f: F) -> Result<R, ObjectError>
    where
        T: AsRef<Token>,
        O: ControlPlane + 'static,
        F: FnOnce(<O as ControlPlane>::AgentGuard<'_>) -> R,
    {
        let token = token.as_ref();
        self.verify_capabilities(token, Capability::AGENT)?;
        let object = self.object.payload::<O>()?;

        Ok(f(object.agent(token.interface_caps())))
    }

    /// Admin with the control plane of the object through this container.
    pub fn admin_cp_with<O, T, F, R>(&self, token: T, f: F) -> Result<R, ObjectError>
    where
        T: AsRef<Token>,
        O: ControlPlane + 'static,
        F: FnOnce(<O as ControlPlane>::AdminGuard<'_>) -> R,
    {
        let token = token.as_ref();
        self.verify_capabilities(token, Capability::ADMIN)?;
        let object = self.object.payload::<O>()?;

        Ok(f(object.admin(token.interface_caps())))
    }

    /// Read the metadata of the object through this container.
    ///
    /// # Arguments
    ///
    /// * `token` - The token of the handle that is requesting to read the
    ///   metadata. This includes the index of the object and the capabilities
    ///   associated with the handle.
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
    pub async fn read_async_with<T, F, R>(&self, token: T, f: F) -> Result<R, ObjectError>
    where
        T: AsRef<Token>,
        F: AsyncFnOnce(&dyn ReadOperation) -> R,
    {
        let token = token.as_ref();
        self.verify_capabilities(token, Capability::READ)?;

        Ok(f(self.object.deref()).await)
    }

    /// Write to the metadata of the object through this container.
    ///
    /// # Arguments
    ///
    /// * `token` - The token of the handle that is requesting to write to the
    ///   metadata. This includes the index of the object and the capabilities
    ///   associated with the handle.
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
    pub async fn write_async_with<T, F, R>(&self, token: T, f: F) -> Result<R, ObjectError>
    where
        T: AsRef<Token>,
        F: AsyncFnOnce(&dyn WriteOperation) -> R,
    {
        let token = token.as_ref();
        self.verify_capabilities(token, Capability::WRITE)?;

        Ok(f(self.object.deref()).await)
    }

    /// Perform an administrative operation on the object through this
    /// container.
    ///
    /// # Arguments
    ///
    /// * `token` - The token of the handle that is requesting to perform the
    ///   administrative operation. This includes the index of the object and
    ///   the capabilities associated with the handle.
    /// * `f` - A closure that takes a reference to the object's administrative
    ///   interface and returns a result.
    ///
    /// # Returns
    ///
    /// * `Ok(R)` - The result of the closure if the administrative operation is
    ///   successful.
    /// * `Err(ObjectError)` - An error indicating why the administrative
    ///   operation failed, such as if the object has been destroyed, if the
    ///   handle does not have the required capabilities, or if the handle's
    ///   index does not match the object's index.
    pub async fn admin_async_with<T, F, R>(&self, token: T, f: F) -> Result<R, ObjectError>
    where
        T: AsRef<Token>,
        F: AsyncFnOnce(&dyn AdminOperation) -> R,
    {
        let token = token.as_ref();
        self.verify_capabilities(token, Capability::ADMIN_GRP)?;

        Ok(f(self.object.deref()).await)
    }

    /// Read with the control plane of the object through this container.
    ///
    /// # Arguments
    ///
    /// * `token` - The token of the handle that is requesting to read with the
    ///   control plane. This includes the index of the object and the
    ///   capabilities associated with the handle.
    /// * `f` - A closure that takes a reference to the control plane's read
    ///   guard and returns a result.
    ///
    /// # Returns
    ///
    /// * `Ok(R)` - The result of the closure if the read operation is
    ///   successful.
    /// * `Err(ObjectError)` - An error indicating why the read operation
    ///   failed, such as if the object has been destroyed, if the handle does
    ///   not have the required capabilities, or if the handle's index does not
    ///   match the object's index.
    pub async fn read_cp_async_with<O, T, F, R>(&self, token: T, f: F) -> Result<R, ObjectError>
    where
        T: AsRef<Token>,
        O: ControlPlane + 'static,
        F: for<'a> AsyncFnOnce(<O as ControlPlane>::ReadGuard<'a>) -> R,
    {
        let token = token.as_ref();
        self.verify_capabilities(token, Capability::READ)?;
        let object = self.object.payload::<O>()?;

        Ok(f(object.read(token.interface_caps())).await)
    }

    /// Write with the control plane of the object through this container.
    ///
    /// # Arguments
    ///
    /// * `token` - The token of the handle that is requesting to write with the
    ///   control plane. This includes the index of the object and the
    ///   capabilities associated with the handle.
    /// * `f` - A closure that takes a reference to the control plane's write
    ///   guard and returns a result.
    ///
    /// # Returns
    ///
    /// * `Ok(R)` - The result of the closure if the write operation is
    ///   successful.
    /// * `Err(ObjectError)` - An error indicating why the write operation
    ///   failed, such as if the object has been destroyed, if the handle does
    ///   not have the required capabilities, or if the handle's index does not
    ///   match the object's index.
    pub async fn write_cp_async_with<O, T, F, R>(&self, token: T, f: F) -> Result<R, ObjectError>
    where
        T: AsRef<Token>,
        O: ControlPlane + 'static,
        F: for<'a> AsyncFnOnce(<O as ControlPlane>::WriteGuard<'a>) -> R,
    {
        let token = token.as_ref();
        self.verify_capabilities(token, Capability::WRITE)?;
        let object = self.object.payload::<O>()?;

        Ok(f(object.write(token.interface_caps())).await)
    }

    /// Execute with the control plane of the object through this container.
    ///
    /// # Arguments
    ///
    /// * `token` - The token of the handle that is requesting to execute with
    ///   the control plane. This includes the index of the object and the
    ///   capabilities associated with the handle.
    /// * `f` - A closure that takes a reference to the control plane's execute
    ///   guard and returns a result.
    ///
    /// # Returns
    ///
    /// * `Ok(R)` - The result of the closure if the execute operation is
    ///   successful.
    /// * `Err(ObjectError)` - An error indicating why the execute operation
    ///   failed, such as if the object has been destroyed, if the handle does
    ///   not have the required capabilities, or if the handle's index does not
    ///   match the object's index.
    pub async fn execute_cp_async_with<O, T, F, R>(&self, token: T, f: F) -> Result<R, ObjectError>
    where
        T: AsRef<Token>,
        O: ControlPlane + 'static,
        F: for<'a> AsyncFnOnce(<O as ControlPlane>::ExecuteGuard<'a>) -> R,
    {
        let token = token.as_ref();
        self.verify_capabilities(token, Capability::EXECUTE)?;
        let object = self.object.payload::<O>()?;

        Ok(f(object.execute(token.interface_caps())).await)
    }

    /// Agent with the control plane of the object through this container.
    pub async fn agent_cp_async_with<O, T, F, R>(&self, token: T, f: F) -> Result<R, ObjectError>
    where
        T: AsRef<Token>,
        O: ControlPlane + 'static,
        F: for<'a> AsyncFnOnce(<O as ControlPlane>::AgentGuard<'a>) -> R,
    {
        let token = token.as_ref();
        self.verify_capabilities(token, Capability::AGENT)?;
        let object = self.object.payload::<O>()?;

        Ok(f(object.agent(token.interface_caps())).await)
    }

    /// Admin with the control plane of the object through this container.
    pub async fn admin_cp_async_with<O, T, F, R>(&self, token: T, f: F) -> Result<R, ObjectError>
    where
        T: AsRef<Token>,
        O: ControlPlane + 'static,
        F: for<'a> AsyncFnOnce(<O as ControlPlane>::AdminGuard<'a>) -> R,
    {
        let token = token.as_ref();
        self.verify_capabilities(token, Capability::ADMIN)?;
        let object = self.object.payload::<O>()?;

        Ok(f(object.admin(token.interface_caps())).await)
    }

    /// Dynamically dispatch one object-specific syscall through the selected
    /// control-plane guard.
    pub async fn invoke_cp<T>(
        &self,
        token: T,
        caller: &ObjectSyscallContext,
        mode: CpAccessMode,
        method_id: usize,
        arg1: usize,
        arg2: usize,
    ) -> Result<SyscallResult, ObjectError>
    where
        T: AsRef<Token>,
    {
        let token = token.as_ref();

        self.verify_capabilities(token, token.capabilities())?;
        cp_mode_allows(mode, token.capabilities())?;

        self.object
            .payload
            .dispatch(caller, mode, token.interface_caps(), method_id, arg1, arg2)
            .await
    }
}
