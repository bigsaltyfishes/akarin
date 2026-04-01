use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use hashbrown::HashMap;
use intrusive_collections::RBTree;
use libakarin_collections::intrusive::LinkedList;
use libakarin_core::{
    clock::{
        Clock,
        source::ClockSource,
        time::{Duration, Instant},
    },
    memory::{VmLayoutSegment, VmRange},
};
use libakarin_machine_core::{
    context::TrapContextTrait,
    cpu::PerCpuTrait,
    interrupt::{InterruptControllerTrait, IpiReason, IpiTarget},
    memory::VirtAddr,
    sync::{NoOp, ScopedGuard},
};
use libakarin_macros::cpu_local;
use libakarin_object::ObjectError;
use libakarin_sync::spin::{Once, SpinLock};

use super::{
    TaskDeadlineAdapter, TaskEligibleAdapter, TaskRef, TrapBoundaryFinalize, VirtualTime,
    balance::LoadBalancer,
    dispatch::SchedulerDispatch,
    preempt::{
        PreemptGuard, Yield, preempt_count_local, preempt_pending_local, preempt_request_local,
        preempt_take_local,
    },
    process::{Process, ProcessReapAction, SpawnTaskError},
    stack,
    task::{PreparedTaskBinding, Task, TaskBoundaryAction, TaskExit, TaskId, TaskReadyState},
    time::{self, Sleep},
};
use crate::{
    RuntimeServices,
    arch::{PerCpu, TrapContext, guards::IrqSaveGuard},
};

cpu_local! {
    static LOCAL_SCHEDULER: SpinLock<Option<Scheduler>, IrqSaveGuard> = SpinLock::new(None);
}

static KERNEL_PROCESS: Once<Arc<Process>, ScopedGuard<NoOp>> = Once::new();
static SCHEDULER_INSTALL: Once<(), ScopedGuard<NoOp>> = Once::new();
/// Best-effort hint used by balancing code to spread work across CPUs.
pub static SCHEDULER_BALANCE_HINT: AtomicUsize = AtomicUsize::new(0);
static CPU0_TICK_COUNT: AtomicU64 = AtomicU64::new(0);

/// Errors returned by the per-CPU scheduler core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerError {
    /// The requested CPU index is outside the installed per-CPU scheduler set.
    InvalidCpu(usize),
    /// The requested CPU slot exists but its scheduler state is not
    /// initialized.
    UninitializedCpu(usize),
    /// The scheduler picked one task id that no longer owns a runnable shell.
    MissingRunnable(TaskId),
}

/// Per-CPU EEVDF scheduler state.
///
/// One `Scheduler` instance exists for each CPU and lives in CPU-local memory.
/// Cross-CPU operations never reconstruct a second scheduler facade; they lock
/// the destination CPU's `Scheduler` directly through the `cpu_local!` storage.
pub struct Scheduler {
    pub(super) cpu_id: usize,
    pub(super) current: Option<TaskId>,
    pub(super) vtime: VirtualTime,
    pub(super) total_weight: u64,
    pub(super) last_balance: Option<Instant>,
    pub(super) tasks: HashMap<TaskId, TaskRef>,
    pub(super) eligible: RBTree<TaskDeadlineAdapter>,
    pub(super) ineligible: RBTree<TaskEligibleAdapter>,
    pub(super) wake_inbox: LinkedList<Task, libakarin_collections::intrusive::DoubleLink>,
}

impl Scheduler {
    /// Install one scheduler instance for each CPU and wire the timer hook.
    pub fn install(cpu_count: usize) {
        let installed_cpu_count = cpu_count.max(1);
        SCHEDULER_INSTALL.get_or_else(|| {
            time::init_timer();
            for cpu_id in 0..installed_cpu_count {
                let slot = unsafe { LOCAL_SCHEDULER.remote_ref_raw(cpu_id) }
                    .expect("scheduler cpu-local slot must exist");
                let mut slot = slot.lock();
                *slot = Some(Self::new(cpu_id));
            }

            let root_range = VmRange::new(
                VirtAddr::new(VmLayoutSegment::UserImage.start()),
                VirtAddr::new(
                    VmLayoutSegment::UserStack
                        .end_exclusive()
                        .expect("user stack segment is bounded"),
                ),
            )
            .expect("kernel scheduler process root range must be valid");
            let kernel_process = KERNEL_PROCESS.get_or_else(|| {
                RuntimeServices::global()
                    .namespaces()
                    .scheduler_manager()
                    .create_process("kernel", root_range)
                    .expect("kernel scheduler process creation must succeed")
            });

            RuntimeServices::global()
                .namespaces()
                .clock_source_manager()
                .register_tick_hook(Self::handle_tick);
            let _ = RuntimeServices::global()
                .namespaces()
                .clock_source_manager()
                .setup_timer();
            RuntimeServices::global().install_reschedule_ipi_hook(preempt_request_local);

            log::info!(
                "[kernel/scheduler] installed {} per-cpu scheduler(s), kernel process {}",
                installed_cpu_count,
                kernel_process.pid()
            );
        });
    }

    /// Return the kernel-owned process used for internal kernel tasks.
    pub fn kernel_process() -> &'static Arc<Process> {
        KERNEL_PROCESS.get()
    }

    /// Return the number of installed CPU schedulers.
    pub fn cpu_count() -> usize {
        PerCpu::count().max(1)
    }

    /// Return one tick counter used by scheduler self-tests and diagnostics.
    pub fn cpu0_test_ticks() -> u64 {
        CPU0_TICK_COUNT.load(Ordering::Acquire)
    }

    /// Return one snapshot for each installed CPU scheduler.
    pub fn debug_snapshots() -> Vec<super::perf::SchedulerCpuDebugSnapshot> {
        let mut snapshots = Vec::with_capacity(Self::cpu_count());
        for cpu_id in 0..Self::cpu_count() {
            let Some(snapshot) =
                Self::with_cpu(cpu_id, |scheduler| scheduler.debug_snapshot()).ok()
            else {
                continue;
            };
            snapshots.push(snapshot);
        }
        snapshots
    }

    /// Return the least-loaded CPU for one new runnable task.
    pub fn least_loaded_cpu() -> usize {
        let mut candidates = Vec::new();
        let mut best_weight = u64::MAX;
        let mut best_runnable = usize::MAX;
        let mut best_has_work = true;
        for cpu_id in 0..Self::cpu_count() {
            let Some((has_work, weight, runnable)) = Self::with_cpu(cpu_id, |scheduler| {
                (
                    scheduler.has_runnable_work(),
                    scheduler.total_weight(),
                    scheduler.runnable_task_count(),
                )
            })
            .ok() else {
                continue;
            };
            if weight < best_weight
                || (weight == best_weight && runnable < best_runnable)
                || (weight == best_weight
                    && runnable == best_runnable
                    && best_has_work
                    && !has_work)
            {
                candidates.clear();
                candidates.push(cpu_id);
                best_weight = weight;
                best_runnable = runnable;
                best_has_work = has_work;
            } else if weight == best_weight
                && runnable == best_runnable
                && has_work == best_has_work
            {
                candidates.push(cpu_id);
            }
        }

        let hint = SCHEDULER_BALANCE_HINT.fetch_add(1, Ordering::AcqRel);
        candidates[hint % candidates.len()]
    }

    /// Return one task reference on `cpu_id` if it is currently tracked there.
    pub fn task_on_cpu(cpu_id: usize, task_id: TaskId) -> Option<TaskRef> {
        Self::with_cpu(cpu_id, |scheduler| scheduler.task(task_id))
            .ok()
            .flatten()
    }

    /// Return the currently running task on the supplied CPU.
    pub fn current_task_on_cpu(cpu_id: usize) -> Option<TaskRef> {
        Self::with_cpu(cpu_id, |scheduler| scheduler.current_task())
            .ok()
            .flatten()
    }

    /// Return the task currently published as running on this CPU.
    pub fn current_task_ref() -> Option<TaskRef> {
        Self::current_task_on_cpu(PerCpu::id())
    }

    /// Return the process currently executing on this CPU.
    pub fn current_process() -> Result<Arc<Process>, ObjectError> {
        Self::current_task_ref()
            .map(|task| Arc::clone(task.process()))
            .ok_or(ObjectError::ObjectNotFound)
    }

    /// Return one process handle for the process currently executing on this
    /// CPU.
    pub fn current_process_handle() -> Result<libakarin_object::Handle, ObjectError> {
        Self::current_process()?.task_process_handle()
    }

    /// Request one local reschedule on the current CPU.
    pub fn request_resched() {
        preempt_request_local();
    }

    /// Mark the current task runnable again and request one local reschedule.
    pub fn yield_current() {
        if let Some(task) = Self::current_task_ref() {
            task.mark_ready(TaskReadyState::Async);
        }
        Self::request_resched();
    }

    /// Toggle whether the current task may migrate to another CPU.
    pub fn allow_current_migration(allowed: bool) {
        if let Some(task) = Self::current_task_ref() {
            task.set_migration_allowed(allowed);
        }
    }

    /// Spawn one kernel task on the least-loaded CPU.
    pub fn spawn<F>(process: &Arc<Process>, future: F) -> Result<TaskId, SpawnError>
    where
        F: core::future::Future<Output = TaskExit> + Send + 'static,
    {
        Self::spawn_on(Self::least_loaded_cpu(), process, future)
    }

    /// Spawn one kernel task on the supplied CPU.
    pub fn spawn_on<F>(
        cpu_id: usize,
        process: &Arc<Process>,
        future: F,
    ) -> Result<TaskId, SpawnError>
    where
        F: core::future::Future<Output = TaskExit> + Send + 'static,
    {
        let _preempt = PreemptGuard::enter_deferred();
        let pid = process.pid();
        let kernel_stack = process.allocate_kernel_stack(stack::TASK_KERNEL_STACK_PAGES)?;
        let task = Task::new(pid, Arc::clone(process), kernel_stack);
        let task_id = RuntimeServices::global()
            .namespaces()
            .scheduler_manager()
            .spawn_task(process, cpu_id, Arc::clone(&task), future)?;
        Ok(task_id)
    }

    /// Spawn one user task and return both the task id and runtime task object.
    pub fn spawn_user_task_ref(
        cpu_id: usize,
        process: Arc<Process>,
        user_ctx: TrapContext,
    ) -> Result<(TaskId, Arc<Task>), SpawnError> {
        let _preempt = PreemptGuard::enter_deferred();
        let pid = process.pid();
        let kernel_stack = process.allocate_kernel_stack(stack::TASK_KERNEL_STACK_PAGES)?;
        let task = Task::new(pid, Arc::clone(&process), kernel_stack);
        let future = {
            let task_ref = Arc::clone(&task);
            task_ref.install_user_context(user_ctx);
            task_ref.run_user(Arc::clone(&process))
        };
        let task_id = RuntimeServices::global()
            .namespaces()
            .scheduler_manager()
            .spawn_task(&process, cpu_id, Arc::clone(&task), future)?;
        Ok((task_id, task))
    }

    /// Spawn one user task on the supplied CPU.
    pub fn spawn_user_on(
        cpu_id: usize,
        process: &Arc<Process>,
        user_ctx: TrapContext,
    ) -> Result<TaskId, SpawnError> {
        Self::spawn_user_task_ref(cpu_id, Arc::clone(process), user_ctx).map(|(task_id, _)| task_id)
    }

    /// Enter the current CPU's long-running scheduler dispatch loop.
    pub fn enter_current_cpu() -> ! {
        SchedulerDispatch::current().enter()
    }

    /// Force an immediate local reschedule through the machine trap entry.
    pub fn invoke_local_reschedule() -> ! {
        SchedulerDispatch::current().reschedule_now()
    }

    /// Sleep for one relative duration through the scheduler timer runtime.
    pub fn sleep(duration: Duration) -> Sleep {
        time::timer().sleep(duration)
    }

    /// Sleep until one absolute deadline through the scheduler timer runtime.
    pub fn sleep_until(deadline: Instant) -> Sleep {
        time::timer().sleep_until(deadline)
    }

    /// Yield once on the current CPU.
    pub fn yield_now() -> Yield {
        Yield::once()
    }

    /// Read the current time from the default scheduler clock source.
    pub fn current_time() -> Option<Instant> {
        RuntimeServices::global()
            .namespaces()
            .clock_source_manager()
            .with_default_clock(|clock: &Clock| Ok(clock.now()))
            .ok()
    }

    /// Admit one task shell on the supplied CPU before it becomes runnable.
    pub fn admit_inactive_task(cpu_id: usize, task: Arc<Task>) -> Result<TaskId, SchedulerError> {
        Self::with_cpu_mut(cpu_id, |scheduler| scheduler.admit_task(task))
    }

    /// Admit one task onto the supplied CPU and wake that CPU if needed.
    pub fn admit_task_on_cpu(cpu_id: usize, task: Arc<Task>) -> Result<TaskId, SchedulerError> {
        Self::with_cpu_mut(cpu_id, |scheduler| scheduler.admit_task(task))
    }

    /// Withdraw one admitted-but-never-runnable task shell from a CPU.
    pub fn withdraw_inactive_task(cpu_id: usize, task_id: TaskId) -> Result<bool, SchedulerError> {
        Self::with_cpu_mut(cpu_id, |scheduler| scheduler.remove_task(task_id).is_some())
    }

    /// Publish one prepared task binding after process registration completed.
    pub fn publish_prepared_task(binding: PreparedTaskBinding) {
        binding.activate();
    }

    /// Enqueue one wakeup on the target CPU and send an IPI if needed.
    pub fn queue_wakeup(cpu_id: usize, task_id: TaskId) -> Result<(), SchedulerError> {
        Self::with_cpu_mut(cpu_id, |scheduler| scheduler.enqueue_wakeup(task_id))?;
        if PerCpu::id() == cpu_id {
            preempt_request_local();
        } else {
            Self::notify_cpu(cpu_id);
        }
        Ok(())
    }

    /// Cancel one task by id if it is still present on any CPU.
    pub fn cancel_task(task_id: TaskId) -> Result<bool, SchedulerError> {
        for cpu_id in 0..Self::cpu_count() {
            let task = Self::task_on_cpu(cpu_id, task_id);
            let Some(task) = task else {
                continue;
            };
            task.request_cancel();
            Self::queue_wakeup(task.sched_meta().cpu_id, task_id)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Finalize one completed task and propagate process-wide cancellation.
    pub fn reap_finished_task(task: &Task) {
        let exit = task.state().terminal.unwrap_or(TaskExit::Completed);
        match RuntimeServices::global()
            .namespaces()
            .scheduler_manager()
            .reap_task(task.process_id(), exit, task.id())
        {
            Ok(ProcessReapAction::Detached) => {}
            Ok(ProcessReapAction::ProcessExited { code, cancel }) => {
                log::info!(
                    "[kernel/scheduler] task {} exited process {} with code {} ({} sibling \
                     task(s) to cancel)",
                    task.id(),
                    task.process_id(),
                    code,
                    cancel.len()
                );
                for sibling in cancel {
                    if let Err(err) = Self::cancel_task(sibling) {
                        log::warn!(
                            "[kernel/scheduler] failed to cancel task {} during process {} \
                             termination: {:?}",
                            sibling,
                            task.process_id(),
                            err
                        );
                    }
                }
            }
            Err(err) => {
                log::warn!(
                    "[kernel/scheduler] failed to reap task {} from process {}: {:?}",
                    task.id(),
                    task.process_id(),
                    err
                );
            }
        }
    }

    /// Run one scheduler tick on the supplied CPU.
    pub fn on_tick_for_cpu(cpu_id: usize, now: Instant) -> Result<(), SchedulerError> {
        if cpu_id == 0 {
            CPU0_TICK_COUNT.fetch_add(1, Ordering::AcqRel);
        }
        if let Some(timer) = time::try_timer() {
            let _ = timer.tick(now);
        }
        let should_reschedule = Self::with_cpu_mut(cpu_id, |scheduler| scheduler.on_tick(now))?;
        let pulled = LoadBalancer::new(cpu_id, now).pull()?;
        if should_reschedule || pulled {
            preempt_request_local();
        }
        Ok(())
    }

    /// Select the next task to run on one CPU, performing wakeup drain first.
    pub fn take_next_task_on_cpu(
        cpu_id: usize,
        now: Instant,
    ) -> Result<Option<TaskRef>, SchedulerError> {
        Self::with_cpu_mut(cpu_id, |scheduler| scheduler.drain_wakeups())?;
        let next = Self::with_cpu_mut(cpu_id, |scheduler| scheduler.take_next(now))?;

        let task = match next {
            Some(task) => task,
            None if LoadBalancer::new(cpu_id, now).pull()? => {
                Self::with_cpu_mut(cpu_id, |scheduler| scheduler.drain_wakeups())?;
                let Some(task) = Self::with_cpu_mut(cpu_id, |scheduler| scheduler.take_next(now))?
                else {
                    return Ok(None);
                };
                task
            }
            None => return Ok(None),
        };

        Ok(Some(task))
    }

    /// Resolve one trap boundary on the current CPU and return the next frame.
    pub fn schedule_current_cpu(
        ctx: &mut TrapContext,
    ) -> Result<Option<TrapContext>, SchedulerError> {
        if !ctx.is_kernel_mode() {
            return Ok(None);
        }

        let cpu_id = PerCpu::id();
        let _ = Self::with_cpu(cpu_id, |_| ())?;
        let dispatch = SchedulerDispatch::current();
        if dispatch.trap_on_fallback_stack(ctx) {
            if Self::current_task_on_cpu(cpu_id).is_some() {
                log::warn!(
                    "[kernel/scheduler cpu={}] scheduler shell trap observed with published \
                     current task",
                    cpu_id
                );
            }

            if preempt_take_local() || Self::cpu_has_work(cpu_id) {
                dispatch.flush_deferred_migration_ready();
                return Ok(Some(dispatch.select_next_or_idle_frame()));
            }

            return Ok(None);
        }

        if let Some(boundary) =
            Self::current_task_on_cpu(cpu_id).and_then(|task| task.take_boundary_action())
        {
            let Some(finalized) = Self::finalize_current_boundary_on_cpu(
                cpu_id,
                ctx,
                boundary,
                Self::current_time(),
            )?
            else {
                return Ok(None);
            };
            if let Some(task_id) = finalized.migration_ready {
                dispatch.defer_migration_ready(task_id);
            }
            if let Some(task) = finalized.reaped {
                Self::reap_finished_task(&task);
            }

            let _ = preempt_take_local();
            dispatch.flush_deferred_migration_ready();
            return Ok(Some(dispatch.select_next_or_idle_frame()));
        }

        if preempt_count_local() > 0 || !preempt_pending_local() {
            return Ok(None);
        }

        let Some(task_id) = Self::save_preempted_current_on_cpu(cpu_id, ctx, Self::current_time())?
        else {
            return Ok(None);
        };

        let _ = preempt_take_local();
        dispatch.defer_migration_ready(task_id);
        dispatch.flush_deferred_migration_ready();
        Ok(Some(dispatch.select_next_or_idle_frame()))
    }

    /// Finalize one task boundary on `cpu_id` using the supplied trap frame.
    pub fn finalize_current_boundary_on_cpu(
        cpu_id: usize,
        ctx: &TrapContext,
        state: TaskBoundaryAction,
        now: Option<Instant>,
    ) -> Result<Option<TrapBoundaryFinalize>, SchedulerError> {
        Self::with_cpu_mut(cpu_id, |scheduler| {
            scheduler.finalize_current_boundary(ctx, state, now)
        })
    }

    /// Save the currently running task on `cpu_id` as a preempted runnable
    /// task.
    pub fn save_preempted_current_on_cpu(
        cpu_id: usize,
        ctx: &mut TrapContext,
        now: Option<Instant>,
    ) -> Result<Option<TaskId>, SchedulerError> {
        Self::with_cpu_mut(cpu_id, |scheduler| {
            scheduler.save_preempted_current(ctx, now)
        })
    }

    /// Return whether `cpu_id` currently has runnable or pending wakeup work.
    pub fn cpu_has_work(cpu_id: usize) -> bool {
        Self::with_cpu(cpu_id, |scheduler| scheduler.has_runnable_work()).unwrap_or(false)
    }

    /// Run one shared borrow closure against the scheduler instance on
    /// `cpu_id`.
    pub fn with_cpu<R>(
        cpu_id: usize,
        f: impl FnOnce(&Scheduler) -> R,
    ) -> Result<R, SchedulerError> {
        let slot = unsafe { LOCAL_SCHEDULER.remote_ref_raw(cpu_id) }
            .ok_or(SchedulerError::InvalidCpu(cpu_id))?;
        let slot = slot.lock();
        let scheduler = slot
            .as_ref()
            .ok_or(SchedulerError::UninitializedCpu(cpu_id))?;
        Ok(f(scheduler))
    }

    /// Run one mutable borrow closure against the scheduler instance on
    /// `cpu_id`.
    pub fn with_cpu_mut<R>(
        cpu_id: usize,
        f: impl FnOnce(&mut Scheduler) -> R,
    ) -> Result<R, SchedulerError> {
        let slot = unsafe { LOCAL_SCHEDULER.remote_ref_raw(cpu_id) }
            .ok_or(SchedulerError::InvalidCpu(cpu_id))?;
        let mut slot = slot.lock();
        let scheduler = slot
            .as_mut()
            .ok_or(SchedulerError::UninitializedCpu(cpu_id))?;
        Ok(f(scheduler))
    }

    /// Send one reschedule IPI to `cpu_id` when it is remote.
    pub fn notify_cpu(cpu_id: usize) {
        let current_cpu = PerCpu::id();
        if cpu_id == current_cpu {
            return;
        }

        let _ = RuntimeServices::global()
            .interrupt_controller()
            .send_ipi(IpiReason::Reschedule, IpiTarget::Specific(cpu_id));
    }

    fn handle_tick(cpu_id: usize, now: Instant) {
        let _ = Self::on_tick_for_cpu(cpu_id, now);
    }
}

/// Errors returned while creating one task.
#[derive(Debug)]
#[allow(dead_code)]
pub enum SpawnError {
    /// One object-system failure happened while preparing task resources.
    Object(libakarin_object::ObjectError),
    /// One scheduler/process integration failure happened while publishing the
    /// task.
    Scheduler(SpawnTaskError),
    /// Task creation failed because one backing allocation was exhausted.
    OutOfMemory,
}

impl From<libakarin_object::ObjectError> for SpawnError {
    fn from(value: libakarin_object::ObjectError) -> Self {
        Self::Object(value)
    }
}

impl From<SpawnTaskError> for SpawnError {
    fn from(value: SpawnTaskError) -> Self {
        Self::Scheduler(value)
    }
}
