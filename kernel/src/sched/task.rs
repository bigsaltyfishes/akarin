use alloc::{boxed::Box, sync::Arc};
use core::{
    future::Future,
    mem::{offset_of, size_of},
    pin::Pin,
    sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering},
};

use async_task::{Builder, Runnable, Task as AsyncTask};
use intrusive_collections::RBTreeLink;
use libakarin_collections::intrusive::{DoubleLink, ElememtOf, SingleLink};
use libakarin_core::{
    clock::time::{Duration, Instant},
    memory::{VmRange, Vmar, Vmo},
};
use libakarin_machine_core::{
    context::{SimdContextTrait, TrapContextTrait, TrapReason},
    interrupt::InterruptControllerTrait,
    memory::{PhysAddr, VirtAddr},
    sync::{NoOp, RawScopedGuard, ScopedGuard},
};
use libakarin_object::{ControlPlane, ObjectError, SyscallDispatch};
use libakarin_sync::{
    asynchronous::Event,
    collections::IdAllocator,
    spin::{Once, SpinLock},
};

use crate::{
    ProcessId, RuntimeServices,
    arch::{SimdContext, TrapContext, guards::IrqSaveGuard},
    sched::process::{Process, ProcessFaultAction, ProcessUserFaultResolution},
    service::{ServiceCallId, ServiceDelivery, ServiceRole, ServiceWaitKind},
    syscall::{self, Syscall},
};

/// Stable task identifier allocated by the runtime.
pub type TaskId = u64;

fn task_ids() -> &'static IdAllocator<u64> {
    static TASK_IDS: Once<IdAllocator<u64>, ScopedGuard<NoOp>> = Once::new();
    TASK_IDS.get_or_else(|| IdAllocator::new(1, 1))
}

/// Final task result observed by joiners and the scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskExit {
    /// The task future returned normally without an explicit exit syscall.
    Completed,
    /// The current task explicitly exited but did not request process teardown.
    ThreadExited(usize),
    /// The current task requested process-wide exit with the supplied code.
    ProcessExited(usize),
    /// The task was terminated by one external kill request.
    Killed,
    /// The task stopped because it hit one unrecoverable fault.
    Faulted,
    /// The task observed cooperative cancellation before completion.
    Cancelled,
}

/// High-level scheduler-visible state of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskSchedState {
    PreemptReady,
    AsyncReady,
    Blocked,
}

impl TaskSchedState {
    /// Return whether this state contributes runnable weight.
    pub const fn is_ready(self) -> bool {
        matches!(self, Self::PreemptReady | Self::AsyncReady)
    }
}

/// Reason recorded for the next ready transition.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskReadyState {
    Preempt,
    Async,
}

/// Composite task state tracked independently from `Future::Poll`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskState {
    pub sched: TaskSchedState,
    pub terminal: Option<TaskExit>,
}

impl Default for TaskState {
    fn default() -> Self {
        Self {
            sched: TaskSchedState::Blocked,
            terminal: None,
        }
    }
}

/// One trap-boundary result produced after running the current task once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskPollOutcome {
    Ready,
    Blocked,
    Exited(TaskExit),
}

impl TaskPollOutcome {
    /// Return whether the task should stay runnable after this outcome.
    pub const fn is_ready(self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// One explicit kernel boundary transition published by the current task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskBoundaryAction {
    Cancelled,
    Outcome(TaskPollOutcome),
}

/// Kernel stack bookkeeping for a task.
#[derive(Debug, Clone)]
pub struct KernelStack {
    slot: Arc<Vmar>,
    vmo: Arc<Vmo>,
    mapped_range: VmRange,
}

impl KernelStack {
    /// Create one kernel stack resource descriptor.
    pub fn new(slot: Arc<Vmar>, vmo: Arc<Vmo>, mapped_range: VmRange) -> Self {
        Self {
            slot,
            vmo,
            mapped_range,
        }
    }

    /// Return the backing VMO for this stack.
    pub fn vmo(&self) -> &Arc<Vmo> {
        &self.vmo
    }

    /// Return the current kernel stack top.
    pub fn top(&self) -> usize {
        self.mapped_range.end().as_usize()
    }

    /// Return the mapped stack range within the reserved slot.
    pub fn mapped_range(&self) -> VmRange {
        self.mapped_range
    }

    /// Return whether one stack pointer falls inside the mapped stack body.
    pub fn contains_stack_pointer(&self, rsp: usize) -> bool {
        self.mapped_range.contains(VirtAddr::new(rsp))
    }

    /// Return the VMAR slot reserved for this kernel stack.
    pub fn slot(&self) -> &Arc<Vmar> {
        &self.slot
    }
}

/// EEVDF metadata owned by a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskSchedMeta {
    pub weight: u64,
    pub vruntime: u128,
    pub veligible: u128,
    pub vdeadline: u128,
    pub slice: Duration,
    pub exec_start: Option<Instant>,
    pub exec_runtime: Duration,
    pub cpu_id: usize,
}

impl Default for TaskSchedMeta {
    fn default() -> Self {
        Self {
            weight: 1024,
            vruntime: 0,
            veligible: 0,
            vdeadline: 0,
            slice: Duration::from_millis(4),
            exec_start: None,
            exec_runtime: Duration::from_secs(0),
            cpu_id: 0,
        }
    }
}

/// Persistent trap frame owned by one task.
///
/// The long-term trap-driven scheduler design uses this as the single
/// continuation source for both first entry and later preemption restores.
struct TaskFrame {
    state: SpinLock<TaskFrameState, IrqSaveGuard>,
}

#[derive(Clone)]
struct TaskFrameState {
    trap: TrapContext,
    initialized: bool,
    resume_from_trap: bool,
}

impl TaskFrame {
    fn new() -> Self {
        Self {
            state: SpinLock::new(TaskFrameState {
                trap: TrapContext::new_kernel(),
                initialized: false,
                resume_from_trap: false,
            }),
        }
    }

    fn save_from_trap(&self, trap: &TrapContext) {
        let mut state = self.state.lock();
        debug_assert!(
            !state.resume_from_trap,
            "task frame saved a second trap continuation before consuming the previous one",
        );
        state.trap = trap.clone();
        state.initialized = true;
        state.resume_from_trap = true;
    }

    fn dispatch_snapshot(&self, entry: usize, stack_pointer: usize) -> TrapContext {
        let mut state = self.state.lock();
        if state.resume_from_trap {
            state.resume_from_trap = false;
            return state.trap.clone();
        }
        if !state.initialized {
            state.trap = TrapContext::new_kernel();
            state.initialized = true;
        }
        state.trap.set_instruction_pointer(entry);
        state.trap.set_stack_pointer(stack_pointer);
        state.trap.set_interrupt_en(false);
        state.trap.clone()
    }

    fn bind_simd_context(&self, ctx: Pin<&mut SimdContext>) {
        let mut state = self.state.lock();
        if !state.initialized {
            state.trap = TrapContext::new_kernel();
            state.initialized = true;
        }
        state.trap.bind_simd_context(ctx);
    }

    fn save_simd_context(&self, simd_ctx: Pin<Box<SimdContext>>) -> Pin<Box<SimdContext>> {
        let mut state = self.state.lock();
        if !state.initialized {
            state.trap = TrapContext::new_kernel();
            state.initialized = true;
        }
        unsafe { state.trap.save_simd(simd_ctx) }
    }
}

/// Execution-side task state used by the dispatcher and async wakeups.
struct TaskExecution {
    runnable_slot: SpinLock<Option<Runnable<TaskId>>, IrqSaveGuard>,
    join: SpinLock<Option<AsyncTask<TaskExit, TaskId>>, IrqSaveGuard>,
    recorded_exit: SpinLock<Option<TaskExit>, IrqSaveGuard>,
    boundary_action: SpinLock<Option<TaskBoundaryAction>, IrqSaveGuard>,
    ready_state: AtomicU8,
    cancel_requested: AtomicBool,
}

impl TaskExecution {
    fn new() -> Self {
        Self {
            runnable_slot: SpinLock::new(None),
            join: SpinLock::new(None),
            recorded_exit: SpinLock::new(None),
            boundary_action: SpinLock::new(None),
            ready_state: AtomicU8::new(TaskReadyState::Async as u8),
            cancel_requested: AtomicBool::new(false),
        }
    }

    fn install_runnable(&self, runnable: Runnable<TaskId>) {
        let mut slot = self.runnable_slot.lock();
        assert!(slot.is_none(), "task runnable slot unexpectedly occupied");
        *slot = Some(runnable);
    }

    fn install_join_handle(&self, join: AsyncTask<TaskExit, TaskId>) {
        let mut slot = self.join.lock();
        assert!(slot.is_none(), "task join handle already installed");
        *slot = Some(join);
    }

    fn record_exit(&self, exit: TaskExit) {
        *self.recorded_exit.lock() = Some(exit);
    }

    fn request_cancel(&self) {
        self.cancel_requested.store(true, Ordering::Release);
    }

    fn cancel_requested(&self) -> bool {
        self.cancel_requested.load(Ordering::Acquire)
    }

    fn publish_boundary(&self, action: TaskBoundaryAction) {
        let mut slot = self.boundary_action.lock();
        assert!(slot.is_none(), "task boundary action unexpectedly occupied");
        *slot = Some(action);
    }

    fn take_boundary_action(&self) -> Option<TaskBoundaryAction> {
        self.boundary_action.lock().take()
    }

    fn poll_once(&self) -> TaskPollOutcome {
        let Some(runnable) = self.runnable_slot.lock().take() else {
            return TaskPollOutcome::Blocked;
        };

        let yielded = runnable.run();
        if let Some(exit) = self.recorded_exit.lock().take() {
            return TaskPollOutcome::Exited(exit);
        }
        if yielded {
            return TaskPollOutcome::Ready;
        }
        if self.has_runnable() {
            return TaskPollOutcome::Ready;
        }
        TaskPollOutcome::Blocked
    }

    fn set_ready(&self, ready: TaskReadyState) {
        self.ready_state.store(ready as u8, Ordering::Release);
    }

    fn take_ready(&self) -> TaskReadyState {
        match self
            .ready_state
            .swap(TaskReadyState::Async as u8, Ordering::AcqRel)
        {
            value if value == TaskReadyState::Preempt as u8 => TaskReadyState::Preempt,
            _ => TaskReadyState::Async,
        }
    }

    fn has_runnable(&self) -> bool {
        self.runnable_slot.lock().is_some()
    }
}

/// Placement-side task state used by migration and cache locality policy.
struct TaskPlacement {
    started: AtomicBool,
    migration_ready: AtomicBool,
    migration_allowed: AtomicBool,
    last_cpu: AtomicUsize,
    last_stop_nanos: AtomicU64,
}

/// Service-dispatch state owned by one task.
struct TaskServiceState {
    role: Option<ServiceRole>,
    waiting: bool,
    wait_kind: Option<ServiceWaitKind>,
    active_call: Option<ServiceCallId>,
    pending_delivery: Option<ServiceDelivery>,
}

impl TaskServiceState {
    fn new() -> Self {
        Self {
            role: None,
            waiting: false,
            wait_kind: None,
            active_call: None,
            pending_delivery: None,
        }
    }
}

impl TaskPlacement {
    fn new() -> Self {
        Self {
            started: AtomicBool::new(false),
            migration_ready: AtomicBool::new(false),
            migration_allowed: AtomicBool::new(true),
            last_cpu: AtomicUsize::new(usize::MAX),
            last_stop_nanos: AtomicU64::new(0),
        }
    }

    fn mark_started(&self) {
        self.started.store(true, Ordering::Release);
    }

    fn started(&self) -> bool {
        self.started.load(Ordering::Acquire)
    }

    fn set_migration_ready(&self, ready: bool) {
        self.migration_ready.store(ready, Ordering::Release);
    }

    fn migration_ready(&self) -> bool {
        self.migration_ready.load(Ordering::Acquire)
    }

    fn allow_migration(&self, allowed: bool) {
        self.migration_allowed.store(allowed, Ordering::Release);
    }

    fn migration_allowed(&self) -> bool {
        self.migration_allowed.load(Ordering::Acquire)
    }

    fn instant_nanos(now: Instant) -> u64 {
        now.as_duration().as_nanos().min(u64::MAX as u128) as u64
    }

    fn record_stop(&self, cpu_id: usize, now: Instant) {
        self.last_cpu.store(cpu_id, Ordering::Release);
        self.last_stop_nanos
            .store(Self::instant_nanos(now), Ordering::Release);
    }

    fn is_cache_hot_on(&self, cpu_id: usize, now: Instant, threshold: Duration) -> bool {
        if self.last_cpu.load(Ordering::Acquire) != cpu_id {
            return false;
        }

        let stop_nanos = self.last_stop_nanos.load(Ordering::Acquire);
        let now_nanos = Self::instant_nanos(now);
        let threshold_nanos = threshold.as_nanos().min(u64::MAX as u128) as u64;
        now_nanos.saturating_sub(stop_nanos) < threshold_nanos
    }
}

/// Task runtime state shared between the scheduler and async wakeups.
pub struct Task {
    id: TaskId,
    process_id: ProcessId,
    process: Arc<Process>,
    state: SpinLock<TaskState, IrqSaveGuard>,
    sched: SpinLock<TaskSchedMeta, IrqSaveGuard>,
    kernel_stack: KernelStack,
    kernel_frame: TaskFrame,
    user_ctx: SpinLock<Option<Pin<Box<TrapContext>>>, IrqSaveGuard>,
    simd_ctx: SpinLock<Option<Pin<Box<SimdContext>>>, IrqSaveGuard>,
    execution: TaskExecution,
    placement: TaskPlacement,
    service: SpinLock<TaskServiceState, IrqSaveGuard>,
    service_event: Event,
    wake_queued: AtomicBool,
    /// Intrusive wake-queue link owned by the scheduler.
    pub wake_link: DoubleLink,
    /// Intrusive wait-queue link owned by userspace service objects.
    pub service_wait_link: SingleLink,
    /// Intrusive eligible-tree link owned by the scheduler run queue.
    pub run_deadline_link: RBTreeLink,
    /// Intrusive ineligible-tree link owned by the scheduler run queue.
    pub eligible_time_link: RBTreeLink,
}

/// Anonymous lifecycle-control object returned to task creators.
pub struct TaskControl {
    /// Keeping the owning `Arc<Task>` inside the capability object is the
    /// current lifecycle model, even before task-control methods are added.
    #[allow(dead_code)]
    task: Arc<Task>,
}

impl TaskControl {
    pub fn new(task: Arc<Task>) -> Self {
        Self { task }
    }
}

impl ControlPlane for TaskControl {
    type ReadGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type WriteGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type AgentGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type AdminGuard<'a>
        = &'a Self
    where
        Self: 'a;

    fn read(&self, _interface_caps: u32) -> Self::ReadGuard<'_> {
        self
    }

    fn write(&self, _interface_caps: u32) -> Self::WriteGuard<'_> {
        self
    }

    fn execute(&self, _interface_caps: u32) -> Self::ExecuteGuard<'_> {
        self
    }

    fn agent(&self, _interface_caps: u32) -> Self::AgentGuard<'_> {
        self
    }

    fn admin(&self, _interface_caps: u32) -> Self::AdminGuard<'_> {
        self
    }
}

impl SyscallDispatch<libakarin_object::ObjectSyscallContext> for TaskControl {}

impl ElememtOf<Task, DoubleLink> for Task {
    fn link(node: &Task) -> &DoubleLink {
        &node.wake_link
    }

    fn link_mut(node: &mut Task) -> &mut DoubleLink {
        &mut node.wake_link
    }

    fn element(link: &DoubleLink) -> &Task {
        let offset = offset_of!(Task, wake_link);
        unsafe { &*((link as *const DoubleLink).byte_sub(offset) as *const Task) }
    }

    fn element_mut(link: &mut DoubleLink) -> &mut Task {
        let offset = offset_of!(Task, wake_link);
        unsafe { &mut *((link as *mut DoubleLink).byte_sub(offset) as *mut Task) }
    }
}

impl ElememtOf<Task, SingleLink> for Task {
    fn link(node: &Task) -> &SingleLink {
        &node.service_wait_link
    }

    fn link_mut(node: &mut Task) -> &mut SingleLink {
        &mut node.service_wait_link
    }

    fn element(link: &SingleLink) -> &Task {
        let offset = offset_of!(Task, service_wait_link);
        unsafe { &*((link as *const SingleLink).byte_sub(offset) as *const Task) }
    }

    fn element_mut(link: &mut SingleLink) -> &mut Task {
        let offset = offset_of!(Task, service_wait_link);
        unsafe { &mut *((link as *mut SingleLink).byte_sub(offset) as *mut Task) }
    }
}

/// One prepared task binding that is not runnable until the scheduler
/// publishes it onto a concrete CPU.
pub struct PreparedTaskBinding {
    initial_runnable: Option<Runnable<TaskId>>,
}

impl PreparedTaskBinding {
    /// Publish the initial runnable after the owning process has registered
    /// the task and the scheduler has admitted it onto a target CPU.
    pub fn activate(mut self) {
        let runnable = self
            .initial_runnable
            .take()
            .expect("prepared task binding activated more than once");
        runnable.schedule();
    }
}

// SAFETY: All intrusive tree links are owned by the task object, but they are
// only inserted, removed, or inspected through the scheduler's outer lock.
// The remaining mutable fields are protected by `SpinLock`, so sharing or
// moving a `Task` across CPUs is sound as long as the contained trap context
// type itself is `Send`.
unsafe impl Send for Task {}
unsafe impl Sync for Task {}

impl Task {
    fn prepare_kernel_entry_stack(&self) -> usize {
        let top = self.kernel_stack.top();
        let rsp = top
            .checked_sub(size_of::<usize>())
            .expect("kernel stack top must accommodate a synthetic return address");
        unsafe {
            (rsp as *mut usize).write(0);
        }
        rsp
    }

    /// Create a new task shell. Future binding and scheduling are performed
    /// separately by the task runtime.
    pub fn new(
        process_id: ProcessId,
        process: Arc<Process>,
        kernel_stack: KernelStack,
    ) -> Arc<Self> {
        Arc::new(Self {
            id: task_ids().allocate(),
            process_id,
            process,
            state: SpinLock::new(TaskState::default()),
            sched: SpinLock::new(TaskSchedMeta::default()),
            kernel_stack,
            kernel_frame: TaskFrame::new(),
            user_ctx: SpinLock::new(None),
            simd_ctx: SpinLock::new(None),
            execution: TaskExecution::new(),
            placement: TaskPlacement::new(),
            service: SpinLock::new(TaskServiceState::new()),
            service_event: Event::new(),
            wake_queued: AtomicBool::new(false),
            wake_link: DoubleLink::new(),
            service_wait_link: SingleLink::new(),
            run_deadline_link: RBTreeLink::new(),
            eligible_time_link: RBTreeLink::new(),
        })
    }

    /// Return the stable task id.
    pub fn id(&self) -> TaskId {
        self.id
    }

    /// Return the owning process id.
    pub fn process_id(&self) -> ProcessId {
        self.process_id
    }

    /// Return the owning process runtime object.
    pub fn process(&self) -> &Arc<Process> {
        &self.process
    }

    /// Build the userspace execution future for this task.
    ///
    /// This is the long-term replacement for the old standalone `UserTask`
    /// wrapper. The task owns the userspace trap context, while the process
    /// owns userspace fault policy and stack teardown.
    pub fn run_user(
        self: &Arc<Self>,
        process: Arc<Process>,
    ) -> Pin<Box<dyn Future<Output = TaskExit> + Send>> {
        let task = Arc::clone(self);
        Box::pin(async move {
            // Userspace tasks always start with one pinned SIMD save area so
            // syscall and trap boundaries preserve SSE/AVX state by default.
            task.setup_simd();
            loop {
                if task.cancel_requested() {
                    task.drop_user_context();
                    return TaskExit::Cancelled;
                }

                let Some(mut user_ctx) = task.take_user_context() else {
                    task.drop_user_context();
                    return TaskExit::Killed;
                };

                unsafe {
                    user_ctx.as_mut().get_mut().run(false);
                }
                task.save_trap_simd(user_ctx.as_mut().get_mut());

                let reason = user_ctx.as_ref().get_ref().reason();
                match reason {
                    TrapReason::Syscall => {
                        let request = user_ctx.as_ref().get_ref().syscall_args();
                        let exits_process = matches!(
                            Syscall::try_from(request.method_id()),
                            Ok(Syscall::ProcessExit)
                        );
                        let exits_task = matches!(
                            Syscall::try_from(request.method_id()),
                            Ok(Syscall::TaskExit)
                        );
                        let ret = syscall::dispatch(request).await;

                        if exits_process && ret.is_ok() {
                            task.drop_user_context();
                            return TaskExit::ProcessExited(ret.values[0]);
                        }
                        if exits_task && ret.is_ok() {
                            task.drop_user_context();
                            return TaskExit::ThreadExited(ret.values[0]);
                        }

                        if let Some(delivery) = task.take_service_delivery() {
                            let trap = user_ctx.as_mut().get_mut();
                            trap.set_instruction_pointer(delivery.usr_ip);
                            trap.set_frame_words(delivery.frame.words());
                            task.put_user_context(user_ctx);
                            continue;
                        }

                        user_ctx.as_mut().get_mut().set_syscall_ret(ret);
                        task.put_user_context(user_ctx);
                    }
                    TrapReason::Interrupt(vector) => {
                        let _ = RuntimeServices::global()
                            .interrupt_controller()
                            .handle_irq(vector);
                        task.put_user_context(user_ctx);
                    }
                    TrapReason::PageFault(_, _)
                    | TrapReason::UndefinedInstruction
                    | TrapReason::UnalignedAccess
                    | TrapReason::GeneralFault(_) => {
                        if let TrapReason::PageFault(addr, flags) = reason {
                            match process.resolve_user_fault(addr, flags) {
                                Ok(ProcessUserFaultResolution::Resume) => {
                                    task.put_user_context(user_ctx);
                                    continue;
                                }
                                Ok(ProcessUserFaultResolution::Block(detail)) => {
                                    if process.service_pager_fault(&detail).await {
                                        task.put_user_context(user_ctx);
                                        continue;
                                    }
                                }
                                Ok(ProcessUserFaultResolution::Terminate(_)) => {}
                                Err(_) => {}
                            }
                        }

                        let action = process.handle_fault(&reason);
                        if matches!(action, ProcessFaultAction::Resume) {
                            task.put_user_context(user_ctx);
                            continue;
                        }

                        task.drop_user_context();
                        return TaskExit::Faulted;
                    }
                    _ => unreachable!(),
                }
            }
        })
    }

    /// Return the owning process hardware address-space root.
    pub fn address_space_root(&self) -> Result<PhysAddr, ObjectError> {
        self.process.address_space_root()
    }

    /// Snapshot the task state.
    pub fn state(&self) -> TaskState {
        *self.state.lock()
    }

    /// Update the task state.
    pub fn set_state(&self, state: TaskState) {
        *self.state.lock() = state;
    }

    /// Snapshot scheduler metadata.
    pub fn sched_meta(&self) -> TaskSchedMeta {
        *self.sched.lock()
    }

    /// Replace scheduler metadata.
    pub fn set_sched_meta(&self, meta: TaskSchedMeta) {
        *self.sched.lock() = meta;
    }

    /// Install one pinned userspace trap context owned by this task.
    pub fn install_user_context(&self, user_ctx: TrapContext) {
        let mut slot = self.user_ctx.lock();
        assert!(slot.is_none(), "task user context already installed");
        *slot = Some(Box::pin(user_ctx));
        if let Some(user_ctx) = slot.as_mut() {
            self.bind_simd_to_trap(user_ctx.as_mut().get_mut());
        }
    }

    /// Install one already-pinned userspace trap context back into this task.
    pub fn put_user_context(&self, user_ctx: Pin<Box<TrapContext>>) {
        let mut slot = self.user_ctx.lock();
        assert!(
            slot.is_none(),
            "task user context slot unexpectedly occupied"
        );
        *slot = Some(user_ctx);
        if let Some(user_ctx) = slot.as_mut() {
            self.bind_simd_to_trap(user_ctx.as_mut().get_mut());
        }
    }

    /// Take the pinned userspace trap context owned by this task.
    pub fn take_user_context(&self) -> Option<Pin<Box<TrapContext>>> {
        self.user_ctx.lock().take()
    }

    /// Drop the userspace trap context owned by this task.
    pub fn drop_user_context(&self) {
        let _ = self.user_ctx.lock().take();
    }

    /// Allocate one pinned SIMD save area for this task and bind it to all
    /// task-owned trap contexts.
    pub fn setup_simd(&self) {
        let mut simd_slot = self.simd_ctx.lock();
        if simd_slot.is_some() {
            return;
        }

        let mut simd_ctx = Box::pin(SimdContext::new());
        {
            let _irq_guard = IrqSaveGuard::enter();
            unsafe {
                simd_ctx.as_mut().get_unchecked_mut().save();
            }
        }
        self.bind_simd_to_task_contexts(simd_ctx.as_mut());
        *simd_slot = Some(simd_ctx);
    }

    fn bind_simd_to_task_contexts(&self, mut simd_ctx: Pin<&mut SimdContext>) {
        self.kernel_frame.bind_simd_context(simd_ctx.as_mut());
        let mut user_ctx = self.user_ctx.lock();
        if let Some(user_ctx) = user_ctx.as_mut() {
            user_ctx
                .as_mut()
                .get_mut()
                .bind_simd_context(simd_ctx.as_mut());
        }
    }

    fn bind_simd_to_trap(&self, trap: &mut TrapContext) {
        let mut simd_ctx = self.simd_ctx.lock();
        let Some(simd_ctx) = simd_ctx.as_mut() else {
            return;
        };
        trap.bind_simd_context(simd_ctx.as_mut());
    }

    /// Save the live SIMD register file into this task's pinned save area and
    /// bind that area back to the supplied trap frame.
    pub fn save_trap_simd(&self, trap: &mut TrapContext) {
        let mut simd_ctx = self.simd_ctx.lock();
        let Some(saved_ctx) = simd_ctx.take() else {
            return;
        };
        let saved_ctx = unsafe { trap.save_simd(saved_ctx) };
        *simd_ctx = Some(saved_ctx);
    }

    /// Save one kernel task's live SIMD state into its persistent resume frame.
    pub fn save_kernel_simd_state(&self) {
        let mut simd_ctx = self.simd_ctx.lock();
        let Some(saved_ctx) = simd_ctx.take() else {
            return;
        };
        let saved_ctx = self.kernel_frame.save_simd_context(saved_ctx);
        *simd_ctx = Some(saved_ctx);
    }

    /// Save the current trap boundary into this task's persistent kernel
    /// runner frame.
    pub fn save_kernel_frame(&self, trap: &TrapContext) {
        debug_assert!(
            self.kernel_stack
                .contains_stack_pointer(trap.stack_pointer()),
            "task {} saved kernel rsp {:#x} outside kernel stack {:#x?}",
            self.id,
            trap.stack_pointer(),
            self.kernel_stack.mapped_range(),
        );
        self.kernel_frame.save_from_trap(trap);
    }

    /// Snapshot the next kernel continuation restored for this task.
    pub fn kernel_resume_frame(&self, entry: usize) -> TrapContext {
        let mut frame = self
            .kernel_frame
            .dispatch_snapshot(entry, self.prepare_kernel_entry_stack());
        self.bind_simd_to_trap(&mut frame);
        debug_assert!(
            self.kernel_stack
                .contains_stack_pointer(frame.stack_pointer()),
            "task {} resume rsp {:#x} fell outside kernel stack {:#x?}",
            self.id,
            frame.stack_pointer(),
            self.kernel_stack.mapped_range(),
        );
        frame
    }

    /// Prepare one future binding for this task without making it runnable.
    ///
    /// This split keeps the spawn protocol explicit: the task future may be
    /// constructed early, but the first runnable publication must not happen
    /// until the owning process has recorded task membership.
    pub fn prepare_future_binding<F, S>(
        self: &Arc<Self>,
        future: F,
        on_schedule: S,
    ) -> PreparedTaskBinding
    where
        F: Future<Output = TaskExit> + Send + 'static,
        S: Fn(TaskId, usize) + Send + Sync + 'static,
    {
        let weak = Arc::downgrade(self);
        let weak_exit = Arc::downgrade(self);
        let task_id = self.id;
        let schedule = move |runnable: Runnable<TaskId>| {
            let Some(task) = weak.upgrade() else {
                return;
            };
            task.execution.install_runnable(runnable);
            on_schedule(task_id, task.sched_meta().cpu_id);
        };
        let future = async move {
            let exit = future.await;
            if let Some(task) = weak_exit.upgrade() {
                task.execution.record_exit(exit);
            }
            exit
        };

        let (runnable, join) = Builder::new()
            .metadata(task_id)
            .spawn(move |_| future, schedule);
        self.execution.install_join_handle(join);
        PreparedTaskBinding {
            initial_runnable: Some(runnable),
        }
    }

    /// Poll the task exactly once and classify the scheduling outcome.
    pub fn poll_boundary(&self) -> TaskPollOutcome {
        self.execution.poll_once()
    }

    pub fn has_runnable(&self) -> bool {
        self.execution.has_runnable()
    }

    pub fn request_cancel(&self) {
        self.execution.request_cancel();
    }

    pub fn cancel_requested(&self) -> bool {
        self.execution.cancel_requested()
    }

    pub fn publish_boundary(&self, action: TaskBoundaryAction) {
        self.execution.publish_boundary(action);
    }

    pub fn take_boundary_action(&self) -> Option<TaskBoundaryAction> {
        self.execution.take_boundary_action()
    }

    pub fn mark_ready(&self, ready: TaskReadyState) {
        self.execution.set_ready(ready);
    }

    /// Try to mark this task as queued for one scheduler wakeup pass.
    pub fn try_mark_wake_queued(&self) -> bool {
        let already_queued = self.wake_queued.swap(true, Ordering::AcqRel);
        if already_queued {
            return false;
        }
        true
    }

    /// Clear the wake-queued marker after the task leaves the wake list.
    pub fn clear_wake_queued(&self) -> bool {
        let was_queued = self.wake_queued.swap(false, Ordering::AcqRel);
        if !was_queued {
            return false;
        }
        true
    }

    pub fn take_ready_reason(&self) -> TaskReadyState {
        self.execution.take_ready()
    }

    /// Enter one service wait state for the supplied wait kind.
    pub fn begin_service_wait(
        &self,
        wait_kind: ServiceWaitKind,
    ) -> Result<(), libakarin_syscall::ServiceError> {
        let mut state = self.service.lock();
        if state.waiting || state.active_call.is_some() || state.pending_delivery.is_some() {
            return Err(libakarin_syscall::ServiceError::InvalidState);
        }

        state.role = Some(wait_kind.role());
        state.waiting = true;
        state.wait_kind = Some(wait_kind);
        Ok(())
    }

    /// Wait until one pending service delivery has been armed for this task.
    pub async fn wait_for_service_delivery(
        &self,
        wait_kind: ServiceWaitKind,
    ) -> Result<(), libakarin_syscall::ServiceError> {
        loop {
            let listener = {
                let state = self.service.lock();
                if state.pending_delivery.is_some() {
                    return Ok(());
                }
                if state.wait_kind != Some(wait_kind) || !state.waiting {
                    return Err(libakarin_syscall::ServiceError::InvalidState);
                }
                self.service_event.listen()
            };
            {
                let state = self.service.lock();
                if state.pending_delivery.is_some() {
                    return Ok(());
                }
                if state.wait_kind != Some(wait_kind) || !state.waiting {
                    return Err(libakarin_syscall::ServiceError::InvalidState);
                }
            }
            listener.await;
        }
    }

    /// Abort the current service wait without arming a delivery.
    pub fn abort_service_wait(&self, wait_kind: ServiceWaitKind) {
        let mut state = self.service.lock();
        if state.wait_kind != Some(wait_kind) {
            return;
        }
        state.role = None;
        state.waiting = false;
        state.wait_kind = None;
        state.pending_delivery = None;
    }

    /// Arm one pending service delivery and mark this task as actively
    /// servicing `call_id`.
    pub fn activate_service_call(
        &self,
        call_id: ServiceCallId,
        delivery: ServiceDelivery,
    ) -> Result<(), libakarin_syscall::ServiceError> {
        let mut state = self.service.lock();
        if !state.waiting || state.wait_kind.is_none() {
            return Err(libakarin_syscall::ServiceError::InvalidState);
        }

        state.waiting = false;
        state.active_call = Some(call_id);
        state.pending_delivery = Some(delivery);
        state.wait_kind = None;
        self.service_event.notify_all();
        Ok(())
    }

    /// Return and clear one pending service delivery, if one exists.
    pub fn take_service_delivery(&self) -> Option<ServiceDelivery> {
        self.service.lock().pending_delivery.take()
    }

    /// Return and clear the currently active service call identifier.
    pub fn take_active_service_call(
        &self,
    ) -> Result<ServiceCallId, libakarin_syscall::ServiceError> {
        let mut state = self.service.lock();
        let Some(call_id) = state.active_call.take() else {
            return Err(libakarin_syscall::ServiceError::InvalidState);
        };
        state.role = None;
        Ok(call_id)
    }

    /// Roll back one freshly armed service activation before the task has
    /// resumed user or kernel service code.
    pub fn cancel_active_service_call(&self, call_id: ServiceCallId) {
        let mut state = self.service.lock();
        if state.active_call != Some(call_id) {
            return;
        }
        state.active_call = None;
        state.pending_delivery = None;
        state.role = None;
        state.wait_kind = None;
        state.waiting = false;
    }

    pub fn mark_started(&self) {
        self.placement.mark_started();
    }

    pub fn started(&self) -> bool {
        self.placement.started()
    }

    pub fn set_migration_ready(&self, ready: bool) {
        self.placement.set_migration_ready(ready);
    }

    pub fn migration_ready(&self) -> bool {
        self.placement.migration_ready()
    }

    pub fn set_migration_allowed(&self, allowed: bool) {
        self.placement.allow_migration(allowed);
    }

    pub fn migration_allowed(&self) -> bool {
        self.placement.migration_allowed()
    }

    pub fn record_stop(&self, cpu_id: usize, now: Instant) {
        self.placement.record_stop(cpu_id, now);
    }

    pub fn is_cache_hot_on(&self, cpu_id: usize, now: Instant, threshold: Duration) -> bool {
        self.placement.is_cache_hot_on(cpu_id, now, threshold)
    }

    pub fn kernel_stack_contains(&self, rsp: usize) -> bool {
        self.kernel_stack.contains_stack_pointer(rsp)
    }

    pub fn kernel_stack_range(&self) -> VmRange {
        self.kernel_stack.mapped_range()
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        if let Some(call_id) = self.service.lock().active_call.take() {
            let _ = crate::service::dispatcher()
                .cancel_call(call_id, libakarin_syscall::ServiceError::ServiceFaulted);
        }
        task_ids().recycle(self.id);
        if let Err(err) = self.process.release_kernel_stack(self.kernel_stack.clone()) {
            warn!(
                "[kernel/scheduler] failed to release kernel stack for task {} process {}: {:?}",
                self.id, self.process_id, err
            );
        }
    }
}
