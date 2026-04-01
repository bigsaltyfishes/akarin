use alloc::{boxed::Box, sync::Arc};
use core::{mem::MaybeUninit, ops::Deref};

use async_trait::async_trait;
use libakarin_object::{ControlPlane, Handle, ObjectError, ObjectSyscallContext, SyscallDispatch};
use libakarin_syscall::{
    PROCESS_ABORT, PROCESS_DERIVE_SEGMENT_VMAR, PROCESS_INHERIT_HANDLE, PROCESS_QUERY,
    PROCESS_WAIT, ProcessInheritHandleArgs, ProcessLoadError, ProcessLoadInfo, ProcessMethod,
    ProcessObjectPhase, ProcessSpawnError, ProcessVmSegment, ProcessWaitError, SYSCALL_STATUS_OK,
    SyscallResult,
};

use super::{Process, SpawnError, TaskId};
use crate::syscall::IntoSyscallResult;

mod handle;
mod load;
mod mailbox;
mod task;

pub use handle::HandleControl;
pub use load::LoadControl;
pub use mailbox::MailboxControl;
pub use task::TaskControl;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProcessControlMode {
    Creating,
    Full,
    Supervisor,
}

/// Capability-plane wrapper for one process runtime object.
///
/// `ProcessControl` is the only public object wrapper for [`Process`]. It does
/// not expose runtime internals directly; instead it returns focused control
/// views for each mutable process subsystem.
#[derive(Clone)]
pub struct ProcessControl {
    process: Arc<Process>,
    mode: ProcessControlMode,
}

impl ProcessControl {
    /// Wrap one published process runtime with full process control access.
    pub fn new_live(process: Arc<Process>) -> Self {
        Self {
            process,
            mode: ProcessControlMode::Full,
        }
    }

    /// Wrap one bootstrap-only process control handle.
    pub fn new_creating(process: Arc<Process>) -> Self {
        Self {
            process,
            mode: ProcessControlMode::Creating,
        }
    }

    /// Wrap one supervisor-only process handle.
    pub fn new_supervisor(process: Arc<Process>) -> Self {
        Self {
            process,
            mode: ProcessControlMode::Supervisor,
        }
    }

    /// Return the wrapped process runtime object.
    pub fn process(&self) -> &Arc<Process> {
        &self.process
    }

    /// Return whether this handle is the anonymous bootstrap creation handle.
    pub fn creating_only(&self) -> bool {
        self.mode == ProcessControlMode::Creating
    }

    /// Return whether this control object represents one supervisor-only
    /// process handle.
    pub fn supervisor_only(&self) -> bool {
        self.mode == ProcessControlMode::Supervisor
    }

    /// Return the externally visible lifecycle phase of the wrapped process.
    pub fn object_phase(&self) -> ProcessObjectPhase {
        if self.creating_only() {
            return ProcessObjectPhase::Creating;
        }
        self.process.object_phase()
    }

    /// Return the stable process identifier of the wrapped process.
    pub fn process_id(&self) -> super::ProcessId {
        self.process.pid()
    }

    /// Derive one writable segment VMAR handle while the process is still in
    /// the creating phase.
    pub fn derive_segment_vmar_handle(
        &self,
        segment: ProcessVmSegment,
    ) -> Result<Handle, ObjectError> {
        self.load_control()
            .derive_segment_vmar_handle(segment)
            .map_err(|_| ObjectError::InvalidArgument)
    }

    /// Install one delegated handle into the wrapped process during the
    /// bootstrap creation phase.
    pub fn inherit_handle_from_caller(
        &self,
        caller: &ObjectSyscallContext,
        args: ProcessInheritHandleArgs,
    ) -> Result<u32, ObjectError> {
        self.load_control()
            .inherit_handle_from_caller(caller, args)
            .map_err(|_| ObjectError::InvalidArgument)
    }

    /// Load one userspace image into this process from the supplied Mach-O
    /// `VMO`.
    pub fn load_image_vmo(
        &self,
        image_vmo: &Arc<libakarin_core::memory::Vmo>,
        flags: usize,
    ) -> Result<ProcessLoadInfo, ProcessLoadError> {
        self.load_control().load_image_vmo(image_vmo, flags)
    }

    /// Consume bootstrap creation state and create the first userspace task.
    pub fn spawn_initial_task(
        &self,
        entry: usize,
        stack_pointer: usize,
        tls_base: usize,
    ) -> Result<(TaskId, Handle), ProcessSpawnError> {
        self.load_control()
            .spawn_initial_task(entry, stack_pointer, tls_base)
    }

    /// Wait until the wrapped process reports one terminal exit code.
    pub async fn wait(&self, timeout_ns: usize) -> Result<usize, ProcessWaitError> {
        if self.creating_only() {
            return Err(ProcessWaitError::InvalidState);
        }
        self.process.wait_for_exit(timeout_ns).await
    }

    /// Abort one process that never reached first userspace entry.
    pub fn abort_creation(&self) -> Result<(), ObjectError> {
        self.load_control().abort_creation()
    }

    /// Return the handle-table controller.
    pub fn handle_control(&self) -> HandleControl {
        HandleControl::new(Arc::clone(&self.process), self.supervisor_only())
    }

    /// Return the mailbox controller.
    pub fn mailbox_control(&self) -> MailboxControl {
        MailboxControl::new(Arc::clone(&self.process), self.supervisor_only())
    }

    /// Return the task controller.
    pub fn task_control(&self) -> TaskControl {
        TaskControl::new(Arc::clone(&self.process), self.supervisor_only())
    }

    /// Return the image/load controller.
    pub fn load_control(&self) -> LoadControl {
        LoadControl::new(
            Arc::clone(&self.process),
            self.creating_only(),
            self.supervisor_only(),
        )
    }
}

/// Capability-denied guard used for process control modes that do not expose
/// one given access plane.
pub struct ProcessControlUnsupportedGuard;

#[async_trait]
impl SyscallDispatch<ObjectSyscallContext> for ProcessControlUnsupportedGuard {
    async fn dispatch(
        &self,
        _caller: &ObjectSyscallContext,
        _method_id: usize,
        _arg1: usize,
        _arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        Err(ObjectError::InsufficientCapabilities)
    }
}

/// Read-only guard for [`ProcessControl`].
/// Read-only control-plane guard for one [`ProcessControl`] handle.
pub struct ProcessControlReadGuard<'a> {
    process: &'a ProcessControl,
    interface_caps: u32,
}

impl Deref for ProcessControlReadGuard<'_> {
    type Target = ProcessControl;

    fn deref(&self) -> &Self::Target {
        self.process
    }
}

#[async_trait]
impl SyscallDispatch<ObjectSyscallContext> for ProcessControlReadGuard<'_> {
    async fn dispatch(
        &self,
        _caller: &ObjectSyscallContext,
        method_id: usize,
        _arg1: usize,
        _arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let method =
            ProcessMethod::try_from(method_id).map_err(|_| ObjectError::InvalidArgument)?;
        match method {
            ProcessMethod::QueryPhase => {
                if self.interface_caps != u32::MAX
                    && (self.interface_caps & PROCESS_QUERY) != PROCESS_QUERY
                {
                    return Err(ObjectError::InsufficientCapabilities);
                }
                Ok(SyscallResult::new(
                    SYSCALL_STATUS_OK,
                    [self.process.object_phase() as usize, 0, 0, 0, 0],
                ))
            }
            _ => Err(ObjectError::InsufficientCapabilities),
        }
    }
}

/// Write guard for [`ProcessControl`].
/// Write control-plane guard for one [`ProcessControl`] handle.
pub struct ProcessControlWriteGuard<'a> {
    process: &'a ProcessControl,
    interface_caps: u32,
}

impl ProcessControlWriteGuard<'_> {
    fn read_inherit_args(
        &self,
        caller: &ObjectSyscallContext,
        address: usize,
    ) -> Result<ProcessInheritHandleArgs, ObjectError> {
        if self.interface_caps != u32::MAX
            && (self.interface_caps & PROCESS_INHERIT_HANDLE) != PROCESS_INHERIT_HANDLE
        {
            return Err(ObjectError::InsufficientCapabilities);
        }

        let mut value = MaybeUninit::<ProcessInheritHandleArgs>::uninit();
        let bytes = unsafe {
            core::slice::from_raw_parts_mut(
                value.as_mut_ptr().cast::<u8>(),
                core::mem::size_of::<ProcessInheritHandleArgs>(),
            )
        };
        caller
            .copy_from_user(address, bytes)
            .map_err(|_| ObjectError::InvalidArgument)?;
        Ok(unsafe { value.assume_init() })
    }

    fn install_segment_vmar_handle(
        &self,
        caller: &ObjectSyscallContext,
        segment: usize,
    ) -> Result<u32, ObjectError> {
        if self.interface_caps != u32::MAX
            && (self.interface_caps & PROCESS_DERIVE_SEGMENT_VMAR) != PROCESS_DERIVE_SEGMENT_VMAR
        {
            return Err(ObjectError::InsufficientCapabilities);
        }

        let segment =
            ProcessVmSegment::try_from(segment).map_err(|_| ObjectError::InvalidArgument)?;
        let handle = self.process.derive_segment_vmar_handle(segment)?;
        caller.install_handle(handle)
    }
}

impl Deref for ProcessControlWriteGuard<'_> {
    type Target = ProcessControl;

    fn deref(&self) -> &Self::Target {
        self.process
    }
}

#[async_trait]
impl SyscallDispatch<ObjectSyscallContext> for ProcessControlWriteGuard<'_> {
    async fn dispatch(
        &self,
        caller: &ObjectSyscallContext,
        method_id: usize,
        arg1: usize,
        _arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let method =
            ProcessMethod::try_from(method_id).map_err(|_| ObjectError::InvalidArgument)?;
        match method {
            ProcessMethod::InheritHandle => {
                let args = self.read_inherit_args(caller, arg1)?;
                let slot = self.process.inherit_handle_from_caller(caller, args)?;
                Ok(SyscallResult::new(
                    SYSCALL_STATUS_OK,
                    [slot as usize, 0, 0, 0, 0],
                ))
            }
            ProcessMethod::DeriveSegmentVmar => {
                let slot = self.install_segment_vmar_handle(caller, arg1)?;
                Ok(SyscallResult::new(
                    SYSCALL_STATUS_OK,
                    [slot as usize, 0, 0, 0, 0],
                ))
            }
            _ => Err(ObjectError::InsufficientCapabilities),
        }
    }
}

/// Admin guard for [`ProcessControl`].
/// Administrative control-plane guard for one [`ProcessControl`] handle.
pub struct ProcessControlAdminGuard<'a> {
    process: &'a ProcessControl,
    interface_caps: u32,
}

impl ProcessControlAdminGuard<'_> {
    /// Spawn one additional userspace task inside the target process.
    pub fn spawn_user_task(
        &self,
        entry: usize,
        stack_pointer: usize,
        tls_base: usize,
    ) -> Result<(TaskId, Handle), SpawnError> {
        self.process
            .task_control()
            .spawn_user_task(entry, stack_pointer, tls_base)
    }
}

impl Deref for ProcessControlAdminGuard<'_> {
    type Target = ProcessControl;

    fn deref(&self) -> &Self::Target {
        self.process
    }
}

#[async_trait]
impl SyscallDispatch<ObjectSyscallContext> for ProcessControlAdminGuard<'_> {
    async fn dispatch(
        &self,
        _caller: &ObjectSyscallContext,
        method_id: usize,
        _arg1: usize,
        _arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let method =
            ProcessMethod::try_from(method_id).map_err(|_| ObjectError::InvalidArgument)?;
        match method {
            ProcessMethod::AbortCreation => {
                if self.interface_caps != u32::MAX
                    && (self.interface_caps & PROCESS_ABORT) != PROCESS_ABORT
                {
                    return Err(ObjectError::InsufficientCapabilities);
                }
                self.process.abort_creation()?;
                Ok(SyscallResult::new(SYSCALL_STATUS_OK, [0, 0, 0, 0, 0]))
            }
            ProcessMethod::Wait => {
                if self.interface_caps != u32::MAX
                    && (self.interface_caps & PROCESS_WAIT) != PROCESS_WAIT
                {
                    return Err(ObjectError::InsufficientCapabilities);
                }
                match self.process.wait(_arg1).await {
                    Ok(code) => Ok(SyscallResult::new(SYSCALL_STATUS_OK, [code, 0, 0, 0, 0])),
                    Err(error) => Ok(error.into_syscall_result()),
                }
            }
            _ => Err(ObjectError::InsufficientCapabilities),
        }
    }
}

impl ControlPlane for ProcessControl {
    type ReadGuard<'a>
        = ProcessControlReadGuard<'a>
    where
        Self: 'a;
    type WriteGuard<'a>
        = ProcessControlWriteGuard<'a>
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = ProcessControlUnsupportedGuard
    where
        Self: 'a;
    type AgentGuard<'a>
        = ProcessControlUnsupportedGuard
    where
        Self: 'a;
    type AdminGuard<'a>
        = ProcessControlAdminGuard<'a>
    where
        Self: 'a;

    fn read(&self, interface_caps: u32) -> Self::ReadGuard<'_> {
        ProcessControlReadGuard {
            process: self,
            interface_caps,
        }
    }

    fn write(&self, interface_caps: u32) -> Self::WriteGuard<'_> {
        ProcessControlWriteGuard {
            process: self,
            interface_caps,
        }
    }

    fn execute(&self, _interface_caps: u32) -> Self::ExecuteGuard<'_> {
        ProcessControlUnsupportedGuard
    }

    fn agent(&self, _interface_caps: u32) -> Self::AgentGuard<'_> {
        ProcessControlUnsupportedGuard
    }

    fn admin(&self, interface_caps: u32) -> Self::AdminGuard<'_> {
        ProcessControlAdminGuard {
            process: self,
            interface_caps,
        }
    }
}

impl SyscallDispatch<ObjectSyscallContext> for ProcessControl {}
