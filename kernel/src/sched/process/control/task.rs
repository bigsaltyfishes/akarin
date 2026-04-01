use alloc::sync::Arc;

use libakarin_object::{Handle, ObjectError};

use super::super::{Process, SpawnError, TaskId};

/// Capability-scoped task controller for one process.
#[derive(Clone)]
pub struct TaskControl {
    process: Arc<Process>,
    supervisor_only: bool,
}

impl TaskControl {
    /// Build one task controller for the supplied process.
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

    /// Spawn one additional userspace task in the target process.
    pub fn spawn_user_task(
        &self,
        entry: usize,
        stack_pointer: usize,
        tls_base: usize,
    ) -> Result<(TaskId, Handle), SpawnError> {
        if self.supervisor_only {
            return Err(ObjectError::InsufficientCapabilities.into());
        }
        self.process.spawn_user_task(entry, stack_pointer, tls_base)
    }
}
