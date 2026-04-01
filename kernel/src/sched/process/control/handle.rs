use alloc::sync::Arc;

use libakarin_object::{Capability, ObjectError};

use super::super::Process;

/// Capability-scoped handle-table controller for one process.
#[derive(Clone)]
pub struct HandleControl {
    process: Arc<Process>,
    supervisor_only: bool,
}

impl HandleControl {
    /// Build one handle-table controller for the supplied process.
    pub fn new(process: Arc<Process>, supervisor_only: bool) -> Self {
        Self {
            process,
            supervisor_only,
        }
    }

    /// Return the wrapped process runtime object.
    pub fn process(&self) -> &Arc<Process> {
        &self.process
    }

    /// Return whether this controller is restricted to supervisor-only
    /// lifecycle methods.
    pub fn supervisor_only(&self) -> bool {
        self.supervisor_only
    }

    /// Close one handle table slot in the controlled process.
    pub fn close_handle(&self, slot: u32) -> Result<(), ObjectError> {
        if self.supervisor_only {
            return Err(ObjectError::InsufficientCapabilities);
        }
        self.process.close_handle(slot)
    }

    /// Derive one lower-privilege handle into a fresh process-local slot.
    pub fn derive_handle(
        &self,
        slot: u32,
        capability: Capability,
        interface_caps: u32,
    ) -> Result<u32, ObjectError> {
        if self.supervisor_only {
            return Err(ObjectError::InsufficientCapabilities);
        }
        self.process.derive_handle(slot, capability, interface_caps)
    }
}
