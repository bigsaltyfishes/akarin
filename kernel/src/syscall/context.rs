use alloc::sync::Arc;

use libakarin_object::{Capability, Handle, ObjectError, Payload, SyscallContext};
use libakarin_syscall::UserCopyError;

use super::UserSlice;
use crate::{Scheduler, sched::process::Process};

pub struct ProcessContext {
    process: Arc<Process>,
}

impl ProcessContext {
    pub fn new(process: Arc<Process>) -> Self {
        Self { process }
    }

    pub fn process(&self) -> &Arc<Process> {
        &self.process
    }
}

impl SyscallContext for ProcessContext {
    type ObjectError = ObjectError;
    type UserError = UserCopyError;
    type Handle = Handle;
    type Payload = Payload;
    type Capability = Capability;

    fn current_process(&self) -> Result<Handle, ObjectError> {
        self.process.task_process_handle()
    }

    fn install_handle(&self, handle: Handle) -> Result<u32, ObjectError> {
        Ok(self.process.install_handle_auto(handle))
    }

    fn create_anonymous_object(
        &self,
        payload: Payload,
        capability: Capability,
        interface_caps: u32,
    ) -> Result<u32, ObjectError> {
        let handle = self
            .process
            .create_anonymous_object(payload, capability, interface_caps)?;
        self.install_handle(handle)
    }

    fn destroy_anonymous_object(&self, slot: u32) -> Result<(), ObjectError> {
        self.process.destroy_anonymous_object(slot)
    }

    fn close_handle(&self, slot: u32) -> Result<(), ObjectError> {
        self.process.close_handle(slot)
    }

    fn acquire_handle(&self, slot: u32) -> Result<Handle, ObjectError> {
        self.process.acquire_handle(slot)
    }

    fn take_handle(&self, slot: u32) -> Result<Handle, ObjectError> {
        self.process.take_handle(slot)
    }

    fn copy_from_user(&self, src: usize, out: &mut [u8]) -> Result<(), UserCopyError> {
        UserSlice::<u8>::new(src, out.len())
            .copy_into_slice(&self.process, out)
            .map_err(|_| UserCopyError::Fault)
    }

    fn copy_to_user(&self, dst: usize, input: &[u8]) -> Result<(), UserCopyError> {
        UserSlice::<u8>::new(dst, input.len())
            .copy_from_slice(&self.process, input)
            .map_err(|_| UserCopyError::Fault)
    }
}

pub struct SyscallEnvironment;

impl SyscallEnvironment {
    pub fn current_process() -> Result<Handle, ObjectError> {
        Scheduler::current_process_handle()
    }

    pub fn current_process_ref() -> Result<Arc<Process>, ObjectError> {
        Scheduler::current_process()
    }
}
