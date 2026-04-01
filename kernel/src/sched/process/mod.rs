use alloc::{
    collections::VecDeque,
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};

use hashbrown::HashMap;
use libakarin_core::memory::{
    GuardedStackLayout, RegionPurpose, VMAR_DEFAULT_INTERFACE_CAPS, VSpace, VSpaceInfo,
    VmControlError, VmFaultResolution, VmFlags, VmLayoutSegment, VmPointerRegion, VmRange, Vmar,
    VmarEntry, VmarMapping, Vmo, VmoBacking, VmoPageMetadata, VmoPagePurpose,
};
use libakarin_machine_core::{
    context::{TrapContextTrait, TrapReason},
    cpu::PerCpuTrait,
    memory::{
        AddressSpaceTrait, FrameZone, PageTableEntryTrait, PageTableTrait, PhysAddr, PhysRun,
        VirtAddr,
        paging::{MMUFlags, PagingError, PhysFrameTrait},
    },
    sync::{NoOp, ScopedGuard},
};
use libakarin_object::{
    Capability, Handle, ObjectError, ObjectSyscallContext, Payload, WriteOperation,
};
use libakarin_sync::{
    asynchronous::{Event, RecvError},
    collections::IdAllocator,
    spin::{Once, SpinLock, SpinRwLock},
};
use libakarin_syscall::{
    INVALID_HANDLE_SLOT, ProcessHandleInstallError, ProcessInheritHandleArgs, ProcessLoadInfo,
    ProcessObjectPhase, ProcessSpawnError, ProcessVmSegment, ProcessVmarExtractError,
    ProcessWaitError,
};

use super::{
    scheduler::SchedulerError,
    task::{KernelStack, Task, TaskControl as RuntimeTaskControl, TaskExit, TaskId},
};
use crate::{
    RuntimeServices, Scheduler, SpawnError,
    arch::{
        Machine,
        guards::IrqSaveGuard,
        vm::{Page, PageSize, PageTable, PhysFrame},
    },
    ipc::{
        BroadcastPort, BusPort, Message, P_BIND_RECV, P_LISTEN, P_PUBLISH, P_QUERY_STATE,
        P_RECV_MSG, P_SEND_MSG, P_SUBSCRIBE, P_UNSUBSCRIBE, ProcessInbox, ProcessInboxReceiver,
        ProcessInboxSender, QueuedMessage, ReplyPort, UnicastPort,
    },
    runtime::TimeoutExt,
};

mod control;
mod ipc;
mod loader;
mod stack;
mod tasks;
mod vm;

pub use control::ProcessControl;
pub use loader::BootstrapLoadError;
pub(crate) use stack::{
    UserStackAllocation, allocate_user_stack_for_process, release_user_stack_for_process,
};
pub use vm::ProcessVmError;

/// Stable process identifier allocated by the scheduler runtime.
pub type ProcessId = u64;

fn process_ids() -> &'static IdAllocator<u64> {
    static PROCESS_IDS: Once<IdAllocator<u64>, ScopedGuard<NoOp>> = Once::new();
    PROCESS_IDS.get_or_else(|| IdAllocator::new(1, 1))
}

/// Per-process decision for one synchronous userspace fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessFaultAction {
    Resume,
    Terminate,
}

impl ProcessFaultAction {
    /// Return whether the process chose to resume execution.
    pub fn resumable(self) -> bool {
        matches!(self, Self::Resume)
    }
}

/// Classified userspace page-fault reasons surfaced by the process runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessPageFaultKind {
    /// No logical mapping covers the faulting address.
    NotMapped,
    /// The address lands in one explicit guard page.
    GuardPage,
    /// The address lands in one static layout segment outside managed mappings.
    StaticLayout,
    /// The address lands in one reserved sub-range with no backing object.
    ReservedRange,
    /// The mapping metadata cannot translate the address into a VMO byte range.
    InvalidMappingRange,
    /// The requested access violates mapping or VMO permissions.
    ProtectionDenied,
    /// Backing exists, but one future page-table install path is still needed.
    RetryAfterMap,
    /// No backing exists and the current mapping has no recovery policy.
    UnresolvedMapping,
    /// The fault would need one future private-copy service path.
    PrivateCow,
    /// The fault would need one future pager round-trip.
    PagerBacked,
}

/// Structured page-fault record emitted by [`Process::resolve_user_fault`].
#[derive(Debug, Clone)]
pub struct ProcessPageFault {
    /// Faulting virtual address.
    pub addr: VirtAddr,
    /// Access bits decoded from the architecture page-fault error code.
    pub access: MMUFlags,
    /// Process-visible region classification at the fault address.
    pub region: VmPointerRegion,
    /// High-level fault classification used by logging and policy.
    pub kind: ProcessPageFaultKind,
    /// Mapping metadata when the fault hit one installed VMAR mapping.
    pub mapping: Option<VmarMapping>,
}

/// Result of asking the process runtime how to handle one user page fault.
#[derive(Debug, Clone)]
pub enum ProcessUserFaultResolution {
    /// The current trap can return to user space immediately.
    Resume,
    /// The current trap must block while one userspace pager services the
    /// supplied fault.
    Block(ProcessPageFault),
    /// The current trap must terminate after reporting the supplied fault.
    Terminate(ProcessPageFault),
}

pub const DEFAULT_USER_STACK_PAGES: usize = 16;

/// Errors returned while spawning a kernel-owned task.
#[derive(Debug)]
pub enum SpawnTaskError {
    Object(ObjectError),
    Scheduler(SchedulerError),
}

impl From<ObjectError> for SpawnTaskError {
    fn from(value: ObjectError) -> Self {
        Self::Object(value)
    }
}

impl From<SchedulerError> for SpawnTaskError {
    fn from(value: SchedulerError) -> Self {
        Self::Scheduler(value)
    }
}

#[derive(Debug, Default)]
struct ProcessTasksState {
    tasks: HashMap<TaskId, Weak<Task>>,
    terminating: bool,
    exit_code: Option<usize>,
}

/// Result of removing one task from a process task set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessReapAction {
    Detached,
    ProcessExited { code: usize, cancel: Vec<TaskId> },
}

/// Internal lifecycle state owned by [`Process`].
///
/// This phase is part of the runtime process object itself. The upcoming
/// rewrite will delete the standalone creating-process wrapper and drive
/// lifecycle entirely through this state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessPhase {
    Running,
    Terminating,
    Exited,
}

/// Scheduler-owned live process object published under
/// `/Kernel/Scheduler/Processes/{pid}`.
///
/// `Process` only models runtime state:
/// - handle table and anonymous kernel-owned objects;
/// - the address-space query root and kernel-private VMAR state;
/// - IPC inbox state;
/// - the registered task set and termination state.
///
/// Fixed address-space layout lives in [`VSpace`], while `Process` keeps only
/// the runtime state that belongs to every process regardless of whether it
/// eventually runs userspace code.
pub struct Process {
    pid: ProcessId,
    name: String,
    phase: SpinLock<ProcessPhase, IrqSaveGuard>,
    creating_owner: SpinLock<Option<Handle>, IrqSaveGuard>,
    loaded_image_info: SpinLock<Option<ProcessLoadInfo>, IrqSaveGuard>,
    self_handle: SpinLock<Option<Handle>, IrqSaveGuard>,
    handle_table: SpinRwLock<HashMap<u32, Handle>, IrqSaveGuard>,
    tasks: SpinLock<ProcessTasksState, IrqSaveGuard>,
    exit_event: Event,
    root_vmar_admin: SpinLock<Option<Handle>, IrqSaveGuard>,
    root_vmar: SpinLock<Option<Arc<Vmar>>, IrqSaveGuard>,
    kernel_vmar: SpinLock<Option<Arc<Vmar>>, IrqSaveGuard>,
    vspace: SpinLock<Option<VSpace>, IrqSaveGuard>,
    address_space_root: SpinLock<Option<PhysAddr>, IrqSaveGuard>,
    anon_objects: SpinRwLock<HashMap<usize, Handle>, IrqSaveGuard>,
    /// Stage-1 IPC inbox. Syscall wiring will start consuming it next.
    #[allow(dead_code)]
    inbox: ProcessInbox,
    pending_messages: SpinRwLock<HashMap<u64, VecDeque<Message>>, IrqSaveGuard>,
}

impl Process {
    /// Create a new scheduler-owned process object.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            pid: process_ids().allocate(),
            name: name.into(),
            phase: SpinLock::new(ProcessPhase::Running),
            creating_owner: SpinLock::new(None),
            loaded_image_info: SpinLock::new(None),
            self_handle: SpinLock::new(None),
            handle_table: SpinRwLock::new(HashMap::new()),
            tasks: SpinLock::new(ProcessTasksState::default()),
            exit_event: Event::new(),
            root_vmar_admin: SpinLock::new(None),
            root_vmar: SpinLock::new(None),
            kernel_vmar: SpinLock::new(None),
            vspace: SpinLock::new(None),
            address_space_root: SpinLock::new(None),
            anon_objects: SpinRwLock::new(HashMap::new()),
            inbox: ProcessInbox::new(),
            pending_messages: SpinRwLock::new(HashMap::new()),
        }
    }

    /// Return the stable process identifier.
    pub fn pid(&self) -> ProcessId {
        self.pid
    }

    /// Return the process debug name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Return the current internal lifecycle phase.
    pub fn phase(&self) -> ProcessPhase {
        *self.phase.lock()
    }

    /// Move the process into the running phase after first task start.
    pub fn mark_running(&self) {
        *self.phase.lock() = ProcessPhase::Running;
    }

    /// Move the process into the terminating phase.
    pub fn mark_terminating(&self) {
        *self.phase.lock() = ProcessPhase::Terminating;
    }

    /// Move the process into the exited phase.
    pub fn mark_exited(&self) {
        *self.phase.lock() = ProcessPhase::Exited;
    }

    /// Install bootstrap-only state for one process that has been created but
    /// has not yet started its first userspace task.
    pub fn initialize_creation_state(&self, owner: Handle) {
        *self.creating_owner.lock() = Some(owner);
    }

    /// Cache one userspace image prepared by `ProcessLoad` until the creator
    /// issues `ProcessTaskSpawn`.
    pub fn stage_loaded_image(&self, image: ProcessLoadInfo) {
        *self.loaded_image_info.lock() = Some(image);
    }

    /// Return the most recently staged userspace image metadata.
    pub fn loaded_image(&self) -> Option<ProcessLoadInfo> {
        self.loaded_image_info.lock().as_ref().copied()
    }

    /// Return the process exit code if one process-wide exit has already been
    /// observed.
    pub fn exit_code(&self) -> Option<usize> {
        self.tasks.lock().exit_code
    }

    /// Wait until the process records one terminal exit code.
    pub async fn wait_for_exit(&self, timeout_ns: usize) -> Result<usize, ProcessWaitError> {
        if let Some(code) = self.exit_code() {
            return Ok(code);
        }
        if timeout_ns == 0 {
            return Err(ProcessWaitError::WouldBlock);
        }

        loop {
            let listener = self.exit_event.listen();
            if let Some(code) = self.exit_code() {
                return Ok(code);
            }
            if timeout_ns == usize::MAX {
                listener.await;
            } else if listener
                .timeout(libakarin_core::clock::time::Duration::from_nanos(
                    timeout_ns.min(u64::MAX as usize) as u64,
                ))
                .await
                .is_none()
            {
                return Err(ProcessWaitError::TimedOut);
            }
        }
    }

    /// Consume one writable segment handle from the process-owned creating
    /// state.
    pub fn derive_creating_segment_vmar_handle(
        &self,
        segment: ProcessVmSegment,
    ) -> Result<Handle, ProcessVmarExtractError> {
        if self.creating_owner.lock().is_none() {
            return Err(ProcessVmarExtractError::InvalidState);
        }
        let segment = match segment {
            ProcessVmSegment::UserImage => VmLayoutSegment::UserImage,
            ProcessVmSegment::UserMapped => VmLayoutSegment::UserMapped,
            ProcessVmSegment::UserHeap => VmLayoutSegment::UserHeap,
            ProcessVmSegment::UserStack => VmLayoutSegment::UserStack,
            ProcessVmSegment::UserMmio => VmLayoutSegment::UserMmio,
        };
        self.derive_segment_vmar_handle(segment)
            .map_err(|_| ProcessVmarExtractError::InvalidArgument)
    }

    /// Install one delegated handle into the process while it is still in the
    /// creating state.
    pub fn inherit_handle_from_caller(
        &self,
        caller: &ObjectSyscallContext,
        args: ProcessInheritHandleArgs,
    ) -> Result<u32, ProcessHandleInstallError> {
        if self.creating_owner.lock().is_none() {
            return Err(ProcessHandleInstallError::InvalidState);
        }
        let capability = Capability::from_bits(args.capability_bits)
            .ok_or(ProcessHandleInstallError::InvalidArgument)?;
        let source = caller
            .acquire_handle(args.source_slot)
            .map_err(|_| ProcessHandleInstallError::InvalidArgument)?;
        if !source.capabilities().contains(Capability::SEND) {
            return Err(ProcessHandleInstallError::InvalidArgument);
        }

        let derived = source
            .derive_handle(capability, args.interface_caps)
            .map_err(|_| ProcessHandleInstallError::InvalidArgument)?;
        if args.target_slot == INVALID_HANDLE_SLOT {
            return Ok(self.install_handle_auto(derived));
        }
        if self.handle_table.read().contains_key(&args.target_slot) {
            return Err(ProcessHandleInstallError::InvalidArgument);
        }
        self.install_handle(args.target_slot, derived);
        Ok(args.target_slot)
    }

    /// Start the first userspace task from one image previously staged by
    /// `ProcessLoad`.
    pub fn spawn_initial_task(
        self: &Arc<Self>,
        entry: usize,
        stack_pointer: usize,
        tls_base: usize,
    ) -> Result<(TaskId, Handle), ProcessSpawnError> {
        let owner = self
            .creating_owner
            .lock()
            .take()
            .ok_or(ProcessSpawnError::InvalidState)?;
        let loaded = self.loaded_image().ok_or(ProcessSpawnError::InvalidState)?;
        let entry_point = if entry == 0 { loaded.entry_ip } else { entry };

        self.validate_initial_instruction_pointer(entry_point)?;
        self.validate_initial_stack_pointer(stack_pointer)?;
        self.validate_initial_tls_base(tls_base)?;

        let mut user_ctx = crate::arch::TrapContext::new_user();
        user_ctx.set_instruction_pointer(entry_point);
        user_ctx.set_stack_pointer(stack_pointer);
        user_ctx.set_tls_base(tls_base);

        match Scheduler::spawn_user_task_ref(Scheduler::least_loaded_cpu(), self.clone(), user_ctx)
        {
            Ok((task_id, _task)) => Ok((task_id, owner)),
            Err(error) => {
                *self.creating_owner.lock() = Some(owner);
                match error {
                    SpawnError::Object(_) | SpawnError::Scheduler(SpawnTaskError::Object(_)) => {
                        Err(ProcessSpawnError::InvalidState)
                    }
                    SpawnError::Scheduler(SpawnTaskError::Scheduler(_)) => {
                        Err(ProcessSpawnError::Internal)
                    }
                    SpawnError::OutOfMemory => Err(ProcessSpawnError::OutOfMemory),
                }
            }
        }
    }

    /// Abort one process that never reached first userspace entry.
    pub fn abort_creation(&self) -> Result<(), ObjectError> {
        let owner = self
            .creating_owner
            .lock()
            .take()
            .ok_or(ObjectError::ObjectDestroyed)?;
        let _ = self.loaded_image_info.lock().take();
        self.mark_exited();
        self.exit_event.notify_all();
        drop(owner);
        RuntimeServices::global()
            .namespaces()
            .scheduler_manager()
            .drop_process(self.pid())
    }

    /// Wrap one runtime task in the anonymous owner handle returned from
    /// task-start or task-create syscalls.
    pub fn create_task_control_handle(&self, task: Arc<Task>) -> Result<Handle, ObjectError> {
        self.create_anonymous_admin_handle(
            Payload::new(RuntimeTaskControl::new(task)),
            Capability::ADMIN | Capability::READ,
        )
    }

    /// Bootstrap the process-private VM root objects after the runtime object
    /// has been allocated.
    pub fn bootstrap_vm_objects(&self, root_range: VmRange) -> Result<(), ProcessVmError> {
        self.bootstrap_vm_objects_inner(root_range)
    }

    /// Derive one scheduler-held read handle for this process.
    pub fn task_process_handle(&self) -> Result<Handle, ObjectError> {
        self.self_handle
            .lock()
            .as_ref()
            .ok_or(ObjectError::ObjectNotFound)?
            .acquire_ref()
    }

    /// Bootstrap the process-private VM root objects after the direct process
    /// runtime has been created.
    fn bootstrap_vm_objects_inner(&self, root_range: VmRange) -> Result<(), ProcessVmError> {
        let root_vmar = Arc::new(Vmar::new(root_range));
        let kernel_vmar = Arc::new(Vmar::new(Self::kernel_stack_root_range(self.pid).ok_or(
            ProcessVmError::Underlying(libakarin_syscall::VmError::InvalidRange),
        )?));
        let vmar = Handle::new_anonymous(
            Payload::new(root_vmar.as_ref().clone()),
            Capability::ADMIN | Capability::AGENT,
        );

        let mut page_table = PageTable::empty(RuntimeServices::global().frame_allocator());
        Machine::map_kernel_space(&mut page_table);
        let address_space_root = PageTable::phys_addr(&page_table);

        // VSpace construction is part of the VM subsystem; failures here must
        // remain VM errors instead of being re-encoded as capability/object
        // failures.
        let vspace = VSpace::new(Arc::clone(&root_vmar)).map_err(ProcessVmError::from)?;

        *self.root_vmar_admin.lock() = Some(vmar);
        *self.root_vmar.lock() = Some(root_vmar);
        *self.kernel_vmar.lock() = Some(kernel_vmar);
        *self.vspace.lock() = Some(vspace);
        *self.address_space_root.lock() = Some(address_space_root);
        Ok(())
    }

    /// Return the physical root of the process hardware address space.
    ///
    /// The current scheduler still runs every task under the active kernel CR3.
    /// This root is prepared ahead of the later dispatch rewrite where task
    /// switches will change CR3 before restoring the next task context.
    pub fn address_space_root(&self) -> Result<PhysAddr, ObjectError> {
        self.address_space_root
            .lock()
            .as_ref()
            .copied()
            .ok_or(ObjectError::ObjectNotFound)
    }

    fn root_vmar(&self) -> Result<Arc<Vmar>, ObjectError> {
        self.root_vmar
            .lock()
            .as_ref()
            .cloned()
            .ok_or(ObjectError::ObjectNotFound)
    }

    /// Return one clone of the process logical address space description.
    fn vspace(&self) -> Result<VSpace, ProcessVmError> {
        self.vspace
            .lock()
            .as_ref()
            .cloned()
            .ok_or(ProcessVmError::Underlying(
                libakarin_syscall::VmError::NotMapped,
            ))
    }

    /// Return the fixed VMAR reserved for one userspace segment.
    pub fn segment_vmar(&self, segment: VmLayoutSegment) -> Result<Arc<Vmar>, ProcessVmError> {
        Ok(Arc::clone(self.vspace()?.segment(segment)))
    }

    /// Derive one writable handle to a fixed userspace segment VMAR.
    pub fn derive_segment_vmar_handle(
        &self,
        segment: VmLayoutSegment,
    ) -> Result<Handle, ObjectError> {
        let vmar = match self.segment_vmar(segment) {
            Ok(vmar) => vmar,
            Err(ProcessVmError::Object(error)) => return Err(error),
            Err(ProcessVmError::Underlying(_)) => return Err(ObjectError::InvalidArgument),
        };
        self.create_anonymous_object(
            Payload::new(vmar.as_ref().clone()),
            Capability::READ | Capability::WRITE,
            VMAR_DEFAULT_INTERFACE_CAPS,
        )
    }

    /// Install a handle into the process handle table.
    pub fn install_handle(&self, slot: u32, handle: Handle) -> Option<Handle> {
        self.handle_table.write().insert(slot, handle)
    }

    /// Install a handle into the first free process-local slot.
    pub fn install_handle_auto(&self, handle: Handle) -> u32 {
        let mut table = self.handle_table.write();
        let mut slot = 0u32;
        while table.contains_key(&slot) {
            slot = slot.saturating_add(1);
        }
        let prev = table.insert(slot, handle);
        debug_assert!(
            prev.is_none(),
            "auto-selected handle slot unexpectedly occupied"
        );
        slot
    }

    /// Close one handle table entry.
    pub fn close_handle(&self, slot: u32) -> Result<(), ObjectError> {
        self.handle_table
            .write()
            .remove(&slot)
            .map(|_| ())
            .ok_or(ObjectError::ObjectNotFound)
    }

    /// Clone one existing handle table entry into a fresh local slot.
    pub fn clone_handle(&self, slot: u32) -> Result<u32, ObjectError> {
        let cloned = self
            .handle_table
            .read()
            .get(&slot)
            .ok_or(ObjectError::ObjectNotFound)?
            .try_clone()?;
        Ok(self.install_handle_auto(cloned))
    }

    /// Acquire one temporary in-kernel reference to one handle table entry.
    pub fn acquire_handle(&self, slot: u32) -> Result<Handle, ObjectError> {
        self.handle_table
            .read()
            .get(&slot)
            .ok_or(ObjectError::ObjectNotFound)?
            .acquire_ref()
    }

    /// Remove one handle table entry and return the owned handle.
    pub fn take_handle(&self, slot: u32) -> Result<Handle, ObjectError> {
        self.handle_table
            .write()
            .remove(&slot)
            .ok_or(ObjectError::ObjectNotFound)
    }

    /// Derive one lower-privilege handle into a fresh local slot.
    pub fn derive_handle(
        &self,
        slot: u32,
        capability: Capability,
        interface_caps: u32,
    ) -> Result<u32, ObjectError> {
        let derived = self
            .handle_table
            .read()
            .get(&slot)
            .ok_or(ObjectError::ObjectNotFound)?
            .derive_handle(capability, interface_caps)?;
        Ok(self.install_handle_auto(derived))
    }

    /// Run one closure with a borrowed handle table entry.
    pub fn with_handle<F, R>(&self, slot: u32, f: F) -> Result<R, ObjectError>
    where
        F: FnOnce(&Handle) -> Result<R, ObjectError>,
    {
        let table = self.handle_table.read();
        let handle = table.get(&slot).ok_or(ObjectError::ObjectNotFound)?;
        f(handle)
    }

    /// Run one async closure with a borrowed handle table entry.
    pub async fn with_handle_async<F, R>(&self, slot: u32, f: F) -> Result<R, ObjectError>
    where
        F: AsyncFnOnce(&Handle) -> Result<R, ObjectError>,
    {
        let table = self.handle_table.read();
        let handle = table.get(&slot).ok_or(ObjectError::ObjectNotFound)?;
        f(handle).await
    }

    /// Return the externally visible lifecycle phase of this live process.
    ///
    /// This is the current compatibility bridge to the old process object ABI.
    /// It will disappear once `ProcessControl` exposes the new lifecycle
    /// methods directly.
    pub fn object_phase(&self) -> ProcessObjectPhase {
        match self.phase() {
            ProcessPhase::Running => ProcessObjectPhase::Live,
            ProcessPhase::Terminating | ProcessPhase::Exited => ProcessObjectPhase::Terminating,
        }
    }
}

/// Manager for `/Kernel/Scheduler/Processes`.
///
/// Direct plane:
/// - create and destroy process runtimes
/// - return typed `Arc<Process>` values for scheduler/runtime internals
/// - manage task registration and reaping
///
/// Capability plane:
/// - discover published process objects
/// - derive delegated process handles for user-visible sharing
pub struct ProcessManager {
    processes: Handle,
    process_owners: SpinRwLock<HashMap<ProcessId, Handle>, IrqSaveGuard>,
    process_refs: SpinRwLock<HashMap<ProcessId, Arc<Process>>, IrqSaveGuard>,
}

impl ProcessManager {
    /// Create the process manager from the `/Kernel/Scheduler/Processes`
    /// namespace owner handle.
    pub fn new(processes: Handle) -> Self {
        Self {
            processes,
            process_owners: SpinRwLock::new(HashMap::new()),
            process_refs: SpinRwLock::new(HashMap::new()),
        }
    }

    /// Create and publish a new kernel-only process runtime.
    pub fn create_process(
        &self,
        name: &str,
        root_range: VmRange,
    ) -> Result<Arc<Process>, ProcessVmError> {
        let process = Arc::new(Process::new(name));
        process.bootstrap_vm_objects(root_range)?;
        let owner = self
            .processes
            .write_with(|ns: &dyn WriteOperation| {
                ns.add_child(
                    process.pid().to_string(),
                    Capability::ADMIN | Capability::AGENT,
                    Payload::new(ProcessControl::new_live(process.clone())),
                )
            })
            .map_err(ProcessVmError::Object)??;
        let pid = process.pid();
        let mut self_handle = Handle::new_anonymous(
            Payload::new(ProcessControl::new_live(process.clone())),
            Capability::READ | Capability::WRITE | Capability::EXECUTE,
        );
        self_handle.downgrade(Capability::ADMIN | Capability::AGENT, 0);
        *process.self_handle.lock() = Some(self_handle);
        process.mark_running();
        self.process_owners.write().insert(pid, owner);
        self.process_refs.write().insert(pid, process.clone());
        Ok(process)
    }

    /// Create one process runtime and return its anonymous bootstrap control
    /// handle.
    ///
    /// The returned handle is not published through discovery. The creator
    /// uses it to load the image, delegate handles, derive fixed segment VMAR
    /// handles, and finally start the first task.
    pub fn create_process_bootstrap(
        &self,
        name: &str,
        root_range: VmRange,
    ) -> Result<(Arc<Process>, Handle), ProcessVmError> {
        let process = Arc::new(Process::new(name));
        process.bootstrap_vm_objects(root_range)?;
        let mut self_handle = Handle::new_anonymous(
            Payload::new(ProcessControl::new_live(process.clone())),
            Capability::READ | Capability::WRITE | Capability::EXECUTE,
        );
        self_handle.downgrade(Capability::ADMIN | Capability::AGENT, 0);
        *process.self_handle.lock() = Some(self_handle);

        self.process_refs
            .write()
            .insert(process.pid(), process.clone());

        let supervisor = Handle::new_anonymous(
            Payload::new(ProcessControl::new_supervisor(process.clone())),
            Capability::ADMIN | Capability::AGENT,
        );
        process.initialize_creation_state(supervisor);

        let bootstrap = Handle::new_anonymous(
            Payload::new(ProcessControl::new_creating(process.clone())),
            Capability::READ | Capability::WRITE | Capability::ADMIN,
        );
        Ok((process, bootstrap))
    }

    /// Create one process runtime and return its anonymous bootstrap control
    /// handle.
    pub fn create_process_control(
        &self,
        name: &str,
        root_range: VmRange,
    ) -> Result<Handle, ProcessVmError> {
        self.create_process_bootstrap(name, root_range)
            .map(|(_, handle)| handle)
    }

    /// Look up a published process object through the discoverable namespace
    /// plane.
    pub fn lookup_process_handle(&self, pid: ProcessId) -> Result<Handle, ObjectError> {
        self.processes.locate(&pid.to_string())
    }

    /// Request a delegated non-owner handle to one published process.
    pub fn request_process_handle(&self, pid: ProcessId) -> Result<Handle, ObjectError> {
        self.process_owners
            .read()
            .get(&pid)
            .ok_or(ObjectError::ObjectNotFound)?
            .derive_handle(
                Capability::READ | Capability::WRITE | Capability::EXECUTE,
                u32::MAX,
            )
    }

    /// Destroy one published process runtime and remove its namespace entry.
    pub fn destroy_process(&self, pid: ProcessId) -> Result<(), ObjectError> {
        self.processes
            .write_with(|p| p.remove_child(&pid.to_string(), true))??;
        self.process_owners.write().remove(&pid);
        self.process_refs.write().remove(&pid);
        process_ids().recycle(pid);
        Ok(())
    }

    /// Remove one process runtime from the direct manager tables without going
    /// through the discoverable process namespace.
    pub fn drop_process(&self, pid: ProcessId) -> Result<(), ObjectError> {
        self.process_owners.write().remove(&pid);
        self.process_refs
            .write()
            .remove(&pid)
            .map(|_| {
                process_ids().recycle(pid);
            })
            .ok_or(ObjectError::ObjectNotFound)
    }

    /// Return one strong in-kernel reference to the published process.
    pub fn process_ref(&self, pid: ProcessId) -> Result<Arc<Process>, ObjectError> {
        self.process_refs
            .read()
            .get(&pid)
            .cloned()
            .ok_or(ObjectError::ObjectNotFound)
    }

    /// Reap one task from its owning process.
    ///
    /// Process lifetime is not tied to "last task exited". A process may
    /// temporarily own zero runnable tasks while still keeping address-space
    /// and handle-table state alive for later reuse or explicit teardown.
    pub fn reap_task(
        &self,
        pid: ProcessId,
        exit: TaskExit,
        task_id: TaskId,
    ) -> Result<ProcessReapAction, ObjectError> {
        let process = self.process_ref(pid)?;
        process.reap_task(task_id, exit)
    }

    /// Prepare, admit, register, and finally publish one task shell.
    ///
    /// The ordering here is intentional: the scheduler may learn about an
    /// inactive task shell early so rollback and cancellation can still find
    /// it, but the first runnable publication must not happen until the owning
    /// process has recorded the task id. Otherwise a very short-lived task
    /// could exit and be reaped before process membership exists.
    pub fn spawn_task<F>(
        &self,
        process: &Arc<Process>,
        cpu_id: usize,
        task: Arc<Task>,
        future: F,
    ) -> Result<TaskId, SpawnTaskError>
    where
        F: core::future::Future<Output = TaskExit> + Send + 'static,
    {
        let binding = task.prepare_future_binding(future, move |task_id, target_cpu| {
            let _ = Scheduler::queue_wakeup(target_cpu, task_id);
        });
        let task_id = Scheduler::admit_inactive_task(cpu_id, Arc::clone(&task))?;
        if let Err(err) = process.register_task(&task) {
            let _ = Scheduler::withdraw_inactive_task(cpu_id, task.id());
            return Err(err.into());
        }
        if task.cancel_requested() {
            let _ = process.unregister_task(task.id());
            let _ = Scheduler::withdraw_inactive_task(cpu_id, task.id());
            return Err(ObjectError::ObjectDestroyed.into());
        }

        Scheduler::publish_prepared_task(binding);
        Ok(task_id)
    }
}
