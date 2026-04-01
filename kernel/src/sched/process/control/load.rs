use alloc::sync::Arc;

use libakarin_core::memory::Vmo;
use libakarin_object::{Handle, ObjectError, ObjectSyscallContext};
use libakarin_syscall::{
    ProcessHandleInstallError, ProcessInheritHandleArgs, ProcessLoadError, ProcessLoadInfo,
    ProcessSpawnError, ProcessVmSegment, ProcessVmarExtractError,
};

use super::super::{BootstrapLoadError, Process, TaskId};

/// Capability-scoped bootstrap/load controller for one process.
#[derive(Clone)]
pub struct LoadControl {
    process: Arc<Process>,
    creating_only: bool,
    supervisor_only: bool,
}

impl LoadControl {
    /// Build one load controller for the supplied process.
    pub fn new(process: Arc<Process>, creating_only: bool, supervisor_only: bool) -> Self {
        Self {
            process,
            creating_only,
            supervisor_only,
        }
    }

    /// Return the wrapped process runtime object.
    pub fn process(&self) -> &Arc<Process> {
        &self.process
    }

    /// Return whether this controller represents one anonymous bootstrap-only
    /// handle.
    pub fn creating_only(&self) -> bool {
        self.creating_only
    }

    /// Return whether this controller is restricted to supervisor-only
    /// lifecycle methods.
    pub fn supervisor_only(&self) -> bool {
        self.supervisor_only
    }

    /// Derive one writable segment VMAR handle for the bootstrap creator.
    pub fn derive_segment_vmar_handle(
        &self,
        segment: ProcessVmSegment,
    ) -> Result<Handle, ProcessVmarExtractError> {
        if !self.creating_only {
            return Err(ProcessVmarExtractError::InvalidState);
        }
        self.process.derive_creating_segment_vmar_handle(segment)
    }

    /// Install one delegated handle into the target process before first task
    /// start.
    pub fn inherit_handle_from_caller(
        &self,
        caller: &ObjectSyscallContext,
        args: ProcessInheritHandleArgs,
    ) -> Result<u32, ProcessHandleInstallError> {
        if !self.creating_only {
            return Err(ProcessHandleInstallError::InvalidState);
        }
        self.process.inherit_handle_from_caller(caller, args)
    }

    /// Load one userspace image from the supplied Mach-O VMO.
    pub fn load_image_vmo(
        &self,
        image_vmo: &Arc<Vmo>,
        _flags: usize,
    ) -> Result<ProcessLoadInfo, ProcessLoadError> {
        if !self.creating_only {
            return Err(ProcessLoadError::InvalidState);
        }

        let info = self
            .process
            .load_image_from_vmo(image_vmo)
            .map_err(|error| match error {
                BootstrapLoadError::Image(_)
                | BootstrapLoadError::Link(_)
                | BootstrapLoadError::InvalidExecutable(_) => ProcessLoadError::InvalidImage,
                BootstrapLoadError::Object(_) | BootstrapLoadError::Vm(_) => {
                    ProcessLoadError::MappingFailed
                }
            })?;
        self.process.stage_loaded_image(info);
        Ok(info)
    }

    /// Consume bootstrap state and create the first userspace task.
    pub fn spawn_initial_task(
        &self,
        entry: usize,
        stack_pointer: usize,
        tls_base: usize,
    ) -> Result<(TaskId, Handle), ProcessSpawnError> {
        if !self.creating_only {
            return Err(ProcessSpawnError::InvalidState);
        }
        self.process
            .spawn_initial_task(entry, stack_pointer, tls_base)
    }

    /// Abort one unpublished bootstrap process.
    pub fn abort_creation(&self) -> Result<(), ObjectError> {
        if !self.creating_only {
            return Err(ObjectError::InsufficientCapabilities);
        }
        self.process.abort_creation()
    }
}
