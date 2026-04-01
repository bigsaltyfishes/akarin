mod object;

use alloc::string::ToString;

use libakarin_object::{Capability, ControlPlane, Handle, ObjectError, Payload, WriteOperation};
use libakarin_syscall::ObjectLifecycleFlags;
pub use object::{InterruptController, IrqSessionObject, IrqSyscallCodec, SharedIrqSessionObject};

/// Manager for IRQ line resource objects published under `/Resource/IRQ`.
pub struct IrqResourceManager {
    namespace: Handle,
}

impl IrqResourceManager {
    /// Create a manager from one writable `/Resource/IRQ` namespace handle.
    pub fn new(namespace: Handle) -> Self {
        Self { namespace }
    }

    /// Register one discoverable IRQ line resource under `/Resource/IRQ/{irq}`.
    pub fn register_irq<T>(&self, irq: usize, payload: T) -> Result<Handle, ObjectError>
    where
        T: ControlPlane + Send + Sync + 'static,
        for<'a> T::ReadGuard<'a>: Send,
        for<'a> T::WriteGuard<'a>: Send,
        for<'a> T::ExecuteGuard<'a>: Send,
    {
        let handle = self.namespace.write_with(|ns: &dyn WriteOperation| {
            ns.add_child(
                irq.to_string(),
                Capability::ADMIN | Capability::AGENT,
                Payload::new(payload),
            )
        })??;
        handle.set_lifecycle_flags(ObjectLifecycleFlags::STICKY)?;
        Ok(handle)
    }

    /// Lookup one IRQ line resource through the discoverable namespace plane.
    pub fn lookup_handle(&self, irq: usize) -> Result<Handle, ObjectError> {
        self.namespace.locate(&irq.to_string())
    }
}
