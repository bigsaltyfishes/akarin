use alloc::string::ToString;

use libakarin_syscall::ObjectLifecycleFlags;

use crate::{Capability, ControlPlane, Handle, NameSpace, ObjectError, Payload, WriteOperation};

/// Manager for objects under `/Resource`.
///
/// Direct plane:
/// - publish top-level resource namespaces and objects
///
/// Capability plane:
/// - derive delegated handles
/// - discover published resources by relative path
pub struct ResourceManager {
    namespace: Handle,
}

impl ResourceManager {
    /// Create a manager from a namespace handle with `WRITE` capability.
    pub fn new(namespace: Handle) -> Self {
        Self { namespace }
    }

    /// Insert a top-level resource object under `/Resource/{name}`.
    pub fn insert_top_level<T>(&self, name: &str, payload: T) -> Result<Handle, ObjectError>
    where
        T: ControlPlane + Send + Sync + 'static,
        for<'a> T::ReadGuard<'a>: Send,
        for<'a> T::WriteGuard<'a>: Send,
        for<'a> T::ExecuteGuard<'a>: Send,
    {
        let handle = self.namespace.write_with(|ns: &dyn WriteOperation| {
            ns.add_child(
                name.to_string(),
                Capability::ADMIN | Capability::AGENT,
                Payload::new(payload),
            )
        })??;
        handle.set_lifecycle_flags(ObjectLifecycleFlags::STICKY)?;
        Ok(handle)
    }

    /// Insert an empty namespace object under `/Resource/{name}`.
    pub fn create_namespace(&self, name: &str) -> Result<Handle, ObjectError> {
        self.insert_top_level(name, NameSpace)
    }

    /// Derive a read-only handle to `/Resource`.
    pub fn derive_readonly_handle(&self) -> Result<Handle, ObjectError> {
        self.namespace.derive_handle(Capability::READ, 0)
    }

    /// Lookup a published resource object through the discoverable namespace
    /// plane.
    pub fn lookup_handle(&self, path: &str) -> Result<Handle, ObjectError> {
        self.namespace.locate(path)
    }

    /// Request a derived handle from the manager-owned namespace handle.
    pub fn request_handle(
        &self,
        capability: Capability,
        interface_caps: u32,
    ) -> Result<Handle, ObjectError> {
        self.namespace.derive_handle(capability, interface_caps)
    }
}
