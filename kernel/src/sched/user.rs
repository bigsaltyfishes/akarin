use alloc::sync::Arc;

use libakarin_core::memory::{VmFlags, VmLayoutSegment};
use libakarin_machine_core::{context::TrapContextTrait, memory::VirtAddr};
use libakarin_object::{Handle, ObjectError};
use libakarin_syscall::ProcessSpawnError;

use super::process::Process;
use crate::{Scheduler, SpawnError, TaskId};

impl Process {
    /// Spawn one userspace task using the fixed userspace layout owned by this
    /// process.
    pub fn spawn_user_task(
        self: &Arc<Self>,
        entry: usize,
        stack_pointer: usize,
        tls_base: usize,
    ) -> Result<(TaskId, Handle), SpawnError> {
        if self.object_phase() == libakarin_syscall::ProcessObjectPhase::Terminating {
            return Err(ObjectError::InvalidArgument.into());
        }

        self.validate_initial_instruction_pointer(entry)
            .map_err(|_| SpawnError::from(ObjectError::InvalidArgument))?;
        self.validate_initial_stack_pointer(stack_pointer)
            .map_err(|_| SpawnError::from(ObjectError::InvalidArgument))?;
        self.validate_initial_tls_base(tls_base)
            .map_err(|_| SpawnError::from(ObjectError::InvalidArgument))?;

        let mut user_ctx = crate::arch::TrapContext::new_user();
        user_ctx.set_instruction_pointer(entry);
        user_ctx.set_stack_pointer(stack_pointer);
        user_ctx.set_tls_base(tls_base);

        let (task_id, task) =
            Scheduler::spawn_user_task_ref(Scheduler::least_loaded_cpu(), self.clone(), user_ctx)?;
        Ok((task_id, self.create_task_control_handle(task)?))
    }

    pub(crate) fn validate_initial_instruction_pointer(
        &self,
        entry: usize,
    ) -> Result<(), ProcessSpawnError> {
        if entry == 0 {
            return Err(ProcessSpawnError::InvalidInstructionPointer);
        }

        let addr = VirtAddr::new(entry);
        if VmLayoutSegment::classify(addr) != Some(VmLayoutSegment::UserImage) {
            return Err(ProcessSpawnError::InvalidInstructionPointer);
        }

        let mapping = self
            .mapping_at(addr)
            .map_err(|_| ProcessSpawnError::InvalidInstructionPointer)?;
        let Some(mapping) = mapping else {
            return Err(ProcessSpawnError::InvalidInstructionPointer);
        };
        if !mapping.flags.contains(VmFlags::USER | VmFlags::EXECUTE) {
            return Err(ProcessSpawnError::InvalidInstructionPointer);
        }
        Ok(())
    }

    pub(crate) fn validate_initial_stack_pointer(
        &self,
        stack_pointer: usize,
    ) -> Result<(), ProcessSpawnError> {
        if stack_pointer == 0 || !stack_pointer.is_multiple_of(16) {
            return Err(ProcessSpawnError::InvalidStackPointer);
        }

        let probe = VirtAddr::new(stack_pointer.saturating_sub(1));
        if VmLayoutSegment::classify(probe) != Some(VmLayoutSegment::UserStack) {
            return Err(ProcessSpawnError::InvalidStackPointer);
        }

        let mapping = self
            .mapping_at(probe)
            .map_err(|_| ProcessSpawnError::InvalidStackPointer)?;
        let Some(mapping) = mapping else {
            return Err(ProcessSpawnError::InvalidStackPointer);
        };
        if !mapping.flags.contains(VmFlags::USER | VmFlags::WRITE) {
            return Err(ProcessSpawnError::InvalidStackPointer);
        }
        Ok(())
    }

    pub(crate) fn validate_initial_tls_base(
        &self,
        tls_base: usize,
    ) -> Result<(), ProcessSpawnError> {
        if tls_base == 0 {
            return Ok(());
        }

        let addr = VirtAddr::new(tls_base);
        match VmLayoutSegment::classify(addr) {
            Some(segment) if segment.is_user() => Ok(()),
            _ => Err(ProcessSpawnError::InvalidTlsBase),
        }
    }
}
