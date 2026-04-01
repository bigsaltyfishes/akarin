//! Scheduler subsystem root.
//!
//! This module ties together the scheduler core, task runtime, process runtime,
//! dispatch boundary handling, and supporting data structures such as sleep
//! timers and load-balancing helpers.

pub mod balance;
pub mod dispatch;
mod eevdf;
pub mod perf;
pub mod preempt;
pub mod process;
/// Per-CPU scheduler core implementation.
pub mod scheduler;
pub mod stack;
pub mod task;
pub mod user;

#[cfg(debug_assertions)]
use alloc::collections::BTreeSet;
use alloc::sync::Arc;

use hashbrown::HashMap;
use intrusive_collections::{KeyAdapter, RBTree, UnsafeRef, intrusive_adapter};
use libakarin_collections::intrusive::LinkedList;
use libakarin_core::clock::time::{Duration, Instant};
use libakarin_machine_core::context::TrapContextTrait;
pub use preempt::{PreemptGuard, Yield};
pub use process::ProcessId;
pub use scheduler::{Scheduler, SchedulerError, SpawnError};
pub use task::{TaskExit, TaskId, TaskPollOutcome};
pub use time::{Sleep, Timer};

use self::{
    eevdf::{EevdfParams, TaskVirtualDeadline, TaskVirtualEligible, VirtualTime},
    perf::SchedulerCpuDebugSnapshot,
};
pub mod time;

use self::task::{
    Task, TaskBoundaryAction, TaskReadyState, TaskSchedMeta, TaskSchedState, TaskState,
};
use crate::arch::TrapContext;

/// Shared task reference used by the scheduler core.
pub type TaskRef = Arc<Task>;

/// Result returned after one trap-boundary task finalization.
pub struct TrapBoundaryFinalize {
    pub reaped: Option<TaskRef>,
    pub migration_ready: Option<TaskId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationPolicy {
    Steal,
    BalanceRequest,
}

intrusive_adapter!(
    TaskDeadlineAdapter = UnsafeRef<Task>: Task { run_deadline_link => intrusive_collections::RBTreeLink }
);

impl<'a> KeyAdapter<'a> for TaskDeadlineAdapter {
    type Key = (TaskVirtualDeadline, TaskId);

    fn get_key(&self, task: &'a Task) -> Self::Key {
        let meta = task.sched_meta();
        (meta.vdeadline, task.id())
    }
}

intrusive_adapter!(
    TaskEligibleAdapter = UnsafeRef<Task>: Task { eligible_time_link => intrusive_collections::RBTreeLink }
);

impl<'a> KeyAdapter<'a> for TaskEligibleAdapter {
    type Key = (TaskVirtualEligible, TaskId);

    fn get_key(&self, task: &'a Task) -> Self::Key {
        let meta = task.sched_meta();
        (meta.veligible, task.id())
    }
}

impl Scheduler {
    fn saturating_virtual_delta(lhs: VirtualTime, rhs: VirtualTime) -> i128 {
        if lhs >= rhs {
            lhs.saturating_sub(rhs).min(i128::MAX as u128) as i128
        } else {
            -((rhs.saturating_sub(lhs)).min(i128::MAX as u128) as i128)
        }
    }

    fn counts_as_runnable(state: TaskSchedState) -> bool {
        state.is_ready()
    }

    fn scheduler_state_for_ready(ready: TaskReadyState) -> TaskSchedState {
        match ready {
            TaskReadyState::Preempt => TaskSchedState::PreemptReady,
            TaskReadyState::Async => TaskSchedState::AsyncReady,
        }
    }

    fn add_runnable_weight(&mut self, task: &Task) {
        self.total_weight = self
            .total_weight
            .saturating_add(task.sched_meta().weight.max(1));
    }

    fn adjust_vtime_for_departure(&mut self, task: &Task) {
        let meta = task.sched_meta();
        let weight = meta.weight.max(1);
        let remaining_weight = self.total_weight.saturating_sub(weight);
        if remaining_weight == 0 {
            return;
        }

        let correction = self
            .vtime
            .abs_diff(meta.veligible)
            .saturating_mul(weight as u128)
            / remaining_weight as u128;
        if self.vtime >= meta.veligible {
            self.vtime = self.vtime.saturating_add(correction);
        } else {
            self.vtime = self.vtime.saturating_sub(correction);
        }
    }

    fn sub_runnable_weight(&mut self, task: &Task) {
        self.total_weight = self
            .total_weight
            .saturating_sub(task.sched_meta().weight.max(1));
    }

    #[cfg(debug_assertions)]
    fn debug_verify(&self, phase: &'static str) {
        let mut eligible_ids = BTreeSet::new();
        let mut ineligible_ids = BTreeSet::new();

        let mut eligible_cursor = self.eligible.front();
        while let Some(task) = eligible_cursor.get() {
            let task_id = task.id();
            let inserted = eligible_ids.insert(task_id);
            debug_assert!(
                inserted,
                "[kernel/scheduler cpu={}] {} duplicated eligible task {}",
                self.cpu_id, phase, task_id
            );
            let state = task.state();
            let meta = task.sched_meta();
            debug_assert!(
                state.sched.is_ready(),
                "[kernel/scheduler cpu={}] {} non-runnable task {} remained in eligible
            tree",
                self.cpu_id,
                phase,
                task_id
            );
            debug_assert!(
                Some(task_id) != self.current,
                "[kernel/scheduler cpu={}] {} current task {} still present in eligible
            tree",
                self.cpu_id,
                phase,
                task_id
            );
            debug_assert!(
                meta.veligible <= self.vtime,
                "[kernel/scheduler cpu={}] {} eligible task {} has veligible={} beyond
            vtime={}",
                self.cpu_id,
                phase,
                task_id,
                meta.veligible,
                self.vtime
            );
            debug_assert!(
                meta.veligible <= meta.vdeadline,
                "[kernel/scheduler cpu={}] {} eligible task {} has inverted window
            {}..{}",
                self.cpu_id,
                phase,
                task_id,
                meta.veligible,
                meta.vdeadline
            );
            eligible_cursor.move_next();
        }

        let mut ineligible_cursor = self.ineligible.front();
        while let Some(task) = ineligible_cursor.get() {
            let task_id = task.id();
            let inserted = ineligible_ids.insert(task_id);
            debug_assert!(
                inserted,
                "[kernel/scheduler cpu={}] {} duplicated ineligible task {}",
                self.cpu_id, phase, task_id
            );
            debug_assert!(
                !eligible_ids.contains(&task_id),
                "[kernel/scheduler cpu={}] {} task {} present in both trees",
                self.cpu_id,
                phase,
                task_id
            );
            let state = task.state();
            let meta = task.sched_meta();
            debug_assert!(
                state.sched.is_ready(),
                "[kernel/scheduler cpu={}] {} non-runnable task {} remained in ineligible tree",
                self.cpu_id,
                phase,
                task_id
            );
            debug_assert!(
                Some(task_id) != self.current,
                "[kernel/scheduler cpu={}] {} current task {} still present in ineligible tree",
                self.cpu_id,
                phase,
                task_id
            );
            debug_assert!(
                meta.veligible <= meta.vdeadline,
                "[kernel/scheduler cpu={}] {} ineligible task {} has inverted window {}..{}",
                self.cpu_id,
                phase,
                task_id,
                meta.veligible,
                meta.vdeadline
            );
            ineligible_cursor.move_next();
        }

        let mut runnable_weight = 0u64;
        for (&task_id, task) in &self.tasks {
            let state = task.state();
            let meta = task.sched_meta();
            let in_eligible = eligible_ids.contains(&task_id);
            let in_ineligible = ineligible_ids.contains(&task_id);

            debug_assert!(
                meta.cpu_id == self.cpu_id,
                "[kernel/scheduler cpu={}] {} task {} metadata cpu={} mismatched owner cpu={}",
                self.cpu_id,
                phase,
                task_id,
                meta.cpu_id,
                self.cpu_id
            );
            debug_assert!(
                meta.veligible <= meta.vdeadline,
                "[kernel/scheduler cpu={}] {} task {} has inverted window {}..{}",
                self.cpu_id,
                phase,
                task_id,
                meta.veligible,
                meta.vdeadline
            );

            if state.sched.is_ready() {
                runnable_weight = runnable_weight.saturating_add(meta.weight.max(1));
                if Some(task_id) == self.current {
                    debug_assert!(
                        !in_eligible && !in_ineligible,
                        "[kernel/scheduler cpu={}] {} current task {} leaked into runqueue",
                        self.cpu_id,
                        phase,
                        task_id
                    );
                    debug_assert!(
                        meta.exec_start.is_some(),
                        "[kernel/scheduler cpu={}] {} current task {} missing exec_start",
                        self.cpu_id,
                        phase,
                        task_id
                    );
                    debug_assert!(
                        state.terminal.is_none(),
                        "[kernel/scheduler cpu={}] {} current task {} unexpectedly terminal",
                        self.cpu_id,
                        phase,
                        task_id
                    );
                } else {
                    debug_assert!(
                        in_eligible ^ in_ineligible,
                        "[kernel/scheduler cpu={}] {} ready task {} missing or duplicated in \
                         runqueue",
                        self.cpu_id,
                        phase,
                        task_id
                    );
                    debug_assert!(
                        meta.exec_start.is_none(),
                        "[kernel/scheduler cpu={}] {} queued task {} still carries exec_start",
                        self.cpu_id,
                        phase,
                        task_id
                    );
                }
            } else {
                debug_assert!(
                    !in_eligible && !in_ineligible,
                    "[kernel/scheduler cpu={}] {} blocked task {} remained in runqueue",
                    self.cpu_id,
                    phase,
                    task_id
                );
                debug_assert!(
                    meta.exec_start.is_none(),
                    "[kernel/scheduler cpu={}] {} blocked task {} still carries exec_start",
                    self.cpu_id,
                    phase,
                    task_id
                );
            }
        }

        debug_assert!(
            runnable_weight == self.total_weight,
            "[kernel/scheduler cpu={}] {} runnable weight mismatch computed={} tracked={}",
            self.cpu_id,
            phase,
            runnable_weight,
            self.total_weight
        );
    }

    /// Build one diagnostic snapshot of the current CPU scheduler state.
    pub fn debug_snapshot(&self) -> SchedulerCpuDebugSnapshot {
        let mut runnable_count = 0usize;
        let mut blocked_count = 0usize;
        let mut terminal_count = 0usize;
        let mut min_vruntime: Option<VirtualTime> = None;
        let mut max_vruntime: Option<VirtualTime> = None;
        let mut min_vdeadline: Option<TaskVirtualDeadline> = None;
        let mut max_vdeadline: Option<TaskVirtualDeadline> = None;
        let mut min_vruntime_delta = i128::MAX;
        let mut max_vruntime_delta = i128::MIN;

        for task in self.tasks.values() {
            let state = task.state();
            let meta = task.sched_meta();
            if state.sched.is_ready() {
                runnable_count += 1;
            } else {
                blocked_count += 1;
            }
            if state.terminal.is_some() {
                terminal_count += 1;
            }

            min_vruntime = Some(match min_vruntime {
                Some(current) => current.min(meta.vruntime),
                None => meta.vruntime,
            });
            max_vruntime = Some(match max_vruntime {
                Some(current) => current.max(meta.vruntime),
                None => meta.vruntime,
            });
            min_vdeadline = Some(match min_vdeadline {
                Some(current) => current.min(meta.vdeadline),
                None => meta.vdeadline,
            });
            max_vdeadline = Some(match max_vdeadline {
                Some(current) => current.max(meta.vdeadline),
                None => meta.vdeadline,
            });

            let delta = Self::saturating_virtual_delta(self.vtime, meta.vruntime);
            min_vruntime_delta = min_vruntime_delta.min(delta);
            max_vruntime_delta = max_vruntime_delta.max(delta);
        }

        if self.tasks.is_empty() {
            min_vruntime_delta = 0;
            max_vruntime_delta = 0;
        }

        let mut eligible_count = 0usize;
        let mut eligible_cursor = self.eligible.front();
        while eligible_cursor.get().is_some() {
            eligible_count += 1;
            eligible_cursor.move_next();
        }

        let mut ineligible_count = 0usize;
        let mut promotable_ineligible_count = 0usize;
        let mut ineligible_cursor = self.ineligible.front();
        while let Some(task) = ineligible_cursor.get() {
            ineligible_count += 1;
            if task.sched_meta().veligible <= self.vtime {
                promotable_ineligible_count += 1;
            }
            ineligible_cursor.move_next();
        }

        SchedulerCpuDebugSnapshot {
            cpu_id: self.cpu_id,
            current: self.current,
            vtime: self.vtime,
            total_weight: self.total_weight,
            task_count: self.tasks.len(),
            runnable_count,
            blocked_count,
            terminal_count,
            eligible_count,
            ineligible_count,
            promotable_ineligible_count,
            wake_inbox_len: self.wake_inbox.len(),
            min_vruntime,
            max_vruntime,
            min_vdeadline,
            max_vdeadline,
            min_vruntime_delta,
            max_vruntime_delta,
        }
    }

    fn transition_task_state(&mut self, task: &Task, new_state: TaskState) {
        let old_state = task.state();
        let was_runnable = Self::counts_as_runnable(old_state.sched);
        let now_runnable = Self::counts_as_runnable(new_state.sched);
        match (was_runnable, now_runnable) {
            (false, true) => self.add_runnable_weight(task),
            (true, false) => {
                self.adjust_vtime_for_departure(task);
                self.sub_runnable_weight(task);
            }
            _ => {}
        }
        task.set_state(new_state);
    }

    /// Create an empty scheduler for one CPU.
    pub fn new(cpu_id: usize) -> Self {
        Self {
            cpu_id,
            current: None,
            vtime: VirtualTime::default(),
            total_weight: 0,
            last_balance: None,
            tasks: HashMap::new(),
            eligible: RBTree::new(TaskDeadlineAdapter::new()),
            ineligible: RBTree::new(TaskEligibleAdapter::new()),
            wake_inbox: LinkedList::new(),
        }
    }

    /// Return the currently running task id, if any.
    pub fn current(&self) -> Option<TaskId> {
        self.current
    }

    /// Return the total runnable weight tracked by this scheduler.
    pub fn total_weight(&self) -> u64 {
        self.total_weight
    }

    /// Return whether this CPU is eligible for another load-balance attempt.
    pub fn should_rebalance(&self, now: Instant, interval: Duration) -> bool {
        self.last_balance
            .map(|last| now < last || now.duration_since(last) >= interval)
            .unwrap_or(true)
    }

    /// Record one load-balance attempt for this CPU.
    pub fn mark_rebalanced(&mut self, now: Instant) {
        self.last_balance = Some(now);
    }

    /// Return the owned task reference for `task_id`.
    pub fn task(&self, task_id: TaskId) -> Option<TaskRef> {
        self.tasks.get(&task_id).cloned()
    }

    /// Return the task currently published as running on this CPU.
    pub fn current_task(&self) -> Option<TaskRef> {
        self.current.and_then(|task_id| self.task(task_id))
    }

    /// Remove one task from the scheduler task table.
    ///
    /// Normal exit and spawn rollback both reuse this path. Rollback only
    /// removes admitted-but-never-runnable shells, so it does not violate the
    /// usual "remove after exit" lifecycle.
    pub fn remove_task(&mut self, task_id: TaskId) -> Option<TaskRef> {
        let task = self.tasks.remove(&task_id)?;
        if task.clear_wake_queued() {
            let task_ptr = Self::wake_task_ptr(&task);
            unsafe {
                (*task_ptr).wake_link.detach(&mut self.wake_inbox);
            }
        }
        if Self::counts_as_runnable(task.state().sched) {
            self.sub_runnable_weight(&task);
        }
        Some(task)
    }

    /// Insert a task into the scheduler and enqueue it as runnable.
    pub fn admit_task(&mut self, task: TaskRef) -> TaskId {
        let task_id = task.id();
        if self.tasks.contains_key(&task_id) {
            return task_id;
        }

        let mut meta = task.sched_meta();
        meta.cpu_id = self.cpu_id;
        self.refresh_deadlines(&task, meta);
        self.tasks.insert(task_id, task.clone());
        let state = task.state();
        if task.has_runnable() || state.sched.is_ready() {
            let ready_state = match state.sched {
                TaskSchedState::PreemptReady => TaskReadyState::Preempt,
                _ => TaskReadyState::Async,
            };
            // Cross-CPU admission transfers one already-runnable task into a new
            // scheduler instance. Its task-local state remains ready, so the
            // normal state transition below will not add weight on our behalf.
            if state.sched.is_ready() {
                self.add_runnable_weight(&task);
            }
            self.insert_runnable_pointer(Self::task_ptr(&task), ready_state);
        }
        #[cfg(debug_assertions)]
        self.debug_verify("admit_task");
        task_id
    }

    /// Queue a wakeup request for later local processing.
    pub fn enqueue_wakeup(&mut self, task_id: TaskId) {
        let Some(task) = self.tasks.get(&task_id).cloned() else {
            return;
        };
        if !task.try_mark_wake_queued() {
            return;
        }
        unsafe {
            self.wake_inbox.push_back(Self::wake_task_ptr(&task));
        }
    }

    /// Return whether this CPU currently has any runnable work.
    pub fn has_runnable_work(&self) -> bool {
        self.current.is_some()
            || self.eligible.front().get().is_some()
            || self.ineligible.front().get().is_some()
            || !self.wake_inbox.is_empty()
    }

    /// Return the number of runnable tasks currently tracked on this CPU.
    pub fn runnable_task_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|task| task.state().sched.is_ready())
            .count()
    }

    /// Remove one runnable task from this CPU so it can be migrated elsewhere.
    /// Remove one runnable task so it can be migrated to another CPU.
    ///
    /// `MigrationPolicy::BalanceRequest` first prefers a cold migratable task
    /// and only relaxes the cache-hot rule if no such task is available.
    pub fn take_migratable(
        &mut self,
        now: Instant,
        cache_hot_threshold: Duration,
        policy: MigrationPolicy,
    ) -> Option<TaskRef> {
        let task = self.extract_migratable_task(
            now,
            cache_hot_threshold,
            matches!(policy, MigrationPolicy::BalanceRequest),
        )?;
        let task_id = task.id();
        let task = self.tasks.remove(&task_id)?;
        self.adjust_vtime_for_departure(&task);
        self.sub_runnable_weight(&task);

        let mut meta = task.sched_meta();
        meta.exec_start = None;
        meta.exec_runtime = Duration::from_secs(0);
        task.set_sched_meta(meta);

        #[cfg(debug_assertions)]
        self.debug_verify("take_migratable");
        Some(task)
    }

    fn extract_migratable_task(
        &mut self,
        now: Instant,
        cache_hot_threshold: Duration,
        relax_cache_hot: bool,
    ) -> Option<TaskRef> {
        self.remove_first_migratable_from_tree(true, now, cache_hot_threshold, false)
            .or_else(|| {
                self.remove_first_migratable_from_tree(false, now, cache_hot_threshold, false)
            })
            .or_else(|| {
                if relax_cache_hot {
                    self.remove_first_migratable_from_tree(true, now, cache_hot_threshold, true)
                } else {
                    None
                }
            })
            .or_else(|| {
                if relax_cache_hot {
                    self.remove_first_migratable_from_tree(false, now, cache_hot_threshold, true)
                } else {
                    None
                }
            })
    }

    /// Drain queued wakeups into the runnable trees.
    pub fn drain_wakeups(&mut self) -> usize {
        let mut drained = 0;
        while let Some(task_ptr) = unsafe { self.wake_inbox.pop_front() } {
            let task = unsafe { &*task_ptr };
            let task_id = task.id();
            let _ = task.clear_wake_queued();
            if self.activate_wakeup_task(task_id) {
                drained += 1;
            }
        }
        #[cfg(debug_assertions)]
        self.debug_verify("drain_wakeups");
        drained
    }

    /// Apply one queued wakeup directly to the runnable trees.
    ///
    /// Both the normal inbox drain and the bounded-queue overflow fallback use
    /// the same transition so ready-state accounting stays identical.
    fn activate_wakeup_task(&mut self, task_id: TaskId) -> bool {
        let Some(task) = self.tasks.get(&task_id).cloned() else {
            return false;
        };

        let state = task.state();
        if state.sched.is_ready() {
            return false;
        }

        let ready_state = task.take_ready_reason();
        self.transition_task_state(
            &task,
            TaskState {
                sched: Self::scheduler_state_for_ready(ready_state),
                ..state
            },
        );
        if task.started() {
            task.set_migration_ready(true);
        }
        self.insert_runnable_pointer(Self::task_ptr(&task), ready_state);
        true
    }

    /// Select the next eligible task by EEVDF virtual deadline.
    fn pick_next(&mut self) -> Option<TaskRef> {
        self.promote_eligible();

        // EEVDF may temporarily place every runnable task into the ineligible
        // tree after runtime accounting. If the CPU has no current task in
        // that state, idling here would deadlock progress: virtual time would
        // stop advancing and the earliest runnable task would never become
        // eligible. When that happens, jump virtual time forward to the first
        // pending eligibility boundary and promote again.
        if self.eligible.front().get().is_none() {
            if let Some(next_eligible) = self
                .ineligible
                .front()
                .get()
                .map(|task| task.sched_meta().veligible)
            {
                if next_eligible > self.vtime {
                    self.vtime = next_eligible;
                }
                self.promote_eligible();
            }
        }

        let pointer = self.eligible.front_mut().remove()?;
        let task_id = pointer.id();
        let task = self.tasks.get(&task_id).cloned()?;
        self.current = Some(task_id);
        Some(task)
    }

    /// Select the next eligible task and stamp its execution start time.
    pub fn take_next(&mut self, now: Instant) -> Option<TaskRef> {
        let task = self.pick_next()?;
        let mut meta = task.sched_meta();
        meta.exec_start = Some(now);
        meta.exec_runtime = Duration::from_secs(0);
        task.set_sched_meta(meta);
        #[cfg(debug_assertions)]
        self.debug_verify("take_next");
        Some(task)
    }

    /// Requeue the previously running task with one explicit ready reason.
    fn requeue_current_with(&mut self, ready: TaskReadyState) -> Option<TaskId> {
        let task_id = self.current.take()?;
        let task = self.tasks.get(&task_id)?.clone();
        let state = task.state();
        if state.terminal.is_some() {
            return Some(task_id);
        }
        let mut meta = task.sched_meta();
        meta.exec_start = None;
        meta.exec_runtime = Duration::from_secs(0);
        task.set_sched_meta(meta);
        self.transition_task_state(
            &task,
            TaskState {
                sched: Self::scheduler_state_for_ready(ready),
                ..state
            },
        );
        self.insert_runnable_pointer(Self::task_ptr(&task), ready);
        Some(task_id)
    }

    /// Block the current task until one asynchronous wakeup makes it runnable
    /// again.
    fn block_current(&mut self) -> Option<TaskId> {
        let task_id = self.current.take()?;
        let task = self.tasks.get(&task_id)?.clone();
        let state = task.state();
        let mut meta = task.sched_meta();
        meta.exec_start = None;
        meta.exec_runtime = Duration::from_secs(0);
        task.set_sched_meta(meta);
        self.transition_task_state(
            &task,
            TaskState {
                sched: TaskSchedState::Blocked,
                ..state
            },
        );
        Some(task_id)
    }

    /// Finalize the current task after one explicit poll outcome.
    fn finalize_current_poll(&mut self, outcome: TaskPollOutcome) -> Option<TaskId> {
        match outcome {
            TaskPollOutcome::Ready => {
                self.requeue_current_with(self.current_task()?.take_ready_reason())
            }
            TaskPollOutcome::Blocked => self.block_current(),
            TaskPollOutcome::Exited(exit) => self.exit_current_with(exit),
        }
    }

    /// Mark the current task as exited with one explicit terminal reason.
    fn exit_current_with(&mut self, exit: TaskExit) -> Option<TaskId> {
        let task_id = self.current.take()?;
        let task = self.tasks.get(&task_id)?.clone();
        let mut meta = task.sched_meta();
        meta.exec_start = None;
        meta.exec_runtime = Duration::from_secs(0);
        task.set_sched_meta(meta);
        self.transition_task_state(
            &task,
            TaskState {
                sched: TaskSchedState::Blocked,
                terminal: Some(exit),
                ..task.state()
            },
        );
        Some(task_id)
    }

    /// Account execution time to the current task and advance scheduler vtime.
    fn account_current_runtime(
        &mut self,
        runtime: Duration,
        now: Instant,
    ) -> Option<TaskSchedMeta> {
        let task_id = self.current?;
        let task = self.tasks.get(&task_id)?.clone();
        let mut meta = task.sched_meta();
        let params = EevdfParams {
            weight: meta.weight.max(1),
            slice: meta.slice,
        };
        let task_delta = params.runtime_to_virtual(runtime);
        let sched_delta =
            EevdfParams::scheduler_runtime_to_virtual(runtime, self.total_weight.max(1));
        meta.vruntime = meta.vruntime.saturating_add(task_delta);
        meta.exec_runtime = meta.exec_runtime.saturating_add(runtime);
        meta.exec_start = Some(now);
        self.vtime = self.vtime.saturating_add(sched_delta);
        self.refresh_deadlines(&task, meta);
        Some(task.sched_meta())
    }

    /// Return whether the current task should yield to another eligible task.
    fn should_preempt_current(&mut self) -> bool {
        let Some(task_id) = self.current else {
            return false;
        };
        let Some(current) = self.tasks.get(&task_id).cloned() else {
            return false;
        };

        self.promote_eligible();
        let Some(contender) = self.eligible.front().get() else {
            return false;
        };

        let current_meta = current.sched_meta();
        let contender_meta = contender.sched_meta();
        contender_meta.vdeadline < current_meta.vdeadline
            || (current_meta.exec_runtime >= current_meta.slice && contender.id() != task_id)
    }

    fn current_task_from_trap(
        &self,
        ctx: &TrapContext,
        phase: &'static str,
        warn_on_missing_current: bool,
    ) -> Option<(TaskId, TaskRef)> {
        let Some(task_id) = self.current else {
            if warn_on_missing_current {
                log::warn!(
                    "[kernel/scheduler cpu={}] {} missing current task",
                    self.cpu_id,
                    phase
                );
            }
            return None;
        };
        let Some(task) = self.task(task_id) else {
            log::warn!(
                "[kernel/scheduler cpu={}] {} missing task {}",
                self.cpu_id,
                phase,
                task_id
            );
            return None;
        };
        let saved_rsp = ctx.stack_pointer();
        if !task.kernel_stack_contains(saved_rsp) {
            log::warn!(
                "[kernel/scheduler cpu={}] {} stack mismatch task={} rsp={:#x} stack={:#x?}",
                self.cpu_id,
                phase,
                task_id,
                saved_rsp,
                task.kernel_stack_range(),
            );
            return None;
        }

        Some((task_id, task))
    }

    fn account_trap_runtime(&mut self, task: &Task, now: Option<Instant>) {
        let Some(now) = now else {
            return;
        };
        let Some(exec_start) = task.sched_meta().exec_start else {
            return;
        };
        if now >= exec_start {
            let runtime = now.duration_since(exec_start);
            if runtime.as_nanos() > 0 {
                let _ = self.account_current_runtime(runtime, now);
            }
        }
    }

    /// Finalize one explicit task-owned boundary action observed at a trap
    /// boundary.
    pub fn finalize_current_boundary(
        &mut self,
        ctx: &TrapContext,
        action: TaskBoundaryAction,
        now: Option<Instant>,
    ) -> Option<TrapBoundaryFinalize> {
        let (_, task) = self.current_task_from_trap(ctx, "boundary trap", true)?;
        self.account_trap_runtime(&task, now);
        if let Some(now) = now {
            task.record_stop(self.cpu_id, now);
        }

        let mut migration_ready = None;
        let reaped = match action {
            TaskBoundaryAction::Cancelled => {
                self.exit_current_with(TaskExit::Cancelled);
                self.remove_task(task.id())
            }
            TaskBoundaryAction::Outcome(outcome) => match outcome {
                TaskPollOutcome::Exited(exit) => {
                    self.exit_current_with(exit);
                    self.remove_task(task.id())
                }
                TaskPollOutcome::Ready | TaskPollOutcome::Blocked => {
                    let result = self.finalize_current_poll(outcome);
                    if result.is_some() && outcome.is_ready() {
                        migration_ready = Some(task.id());
                    }
                    None
                }
            },
        };

        let finalized = TrapBoundaryFinalize {
            reaped,
            migration_ready,
        };
        #[cfg(debug_assertions)]
        self.debug_verify("finalize_current_boundary");
        Some(finalized)
    }

    /// Save the currently running task as preempted from one trap boundary and
    /// requeue it with a preempt-ready reason.
    pub fn save_preempted_current(
        &mut self,
        ctx: &mut TrapContext,
        now: Option<Instant>,
    ) -> Option<TaskId> {
        let (task_id, task) = self.current_task_from_trap(ctx, "preempt trap", false)?;
        self.account_trap_runtime(&task, now);

        task.save_trap_simd(ctx);
        task.save_kernel_frame(ctx);
        if let Some(now) = now {
            task.record_stop(self.cpu_id, now);
        }
        task.mark_ready(TaskReadyState::Preempt);
        let saved = matches!(
            self.requeue_current_with(TaskReadyState::Preempt),
            Some(requeued) if requeued == task_id
        )
        .then_some(task_id);
        #[cfg(debug_assertions)]
        self.debug_verify("save_preempted_current");
        saved
    }

    /// Account tick runtime and return whether the current CPU should preempt.
    pub fn on_tick(&mut self, now: Instant) -> bool {
        let drained = self.drain_wakeups();

        let mut accounted = false;
        if let Some(task_id) = self.current() {
            if let Some(task) = self.task(task_id) {
                if let Some(exec_start) = task.sched_meta().exec_start {
                    if now >= exec_start {
                        let runtime = now.duration_since(exec_start);
                        if runtime.as_nanos() > 0 {
                            let _ = self.account_current_runtime(runtime, now);
                            accounted = true;
                        }
                    }
                }
            }
        }

        let should_preempt = self.current().is_none() && drained > 0
            || (accounted || drained > 0) && self.should_preempt_current();
        #[cfg(debug_assertions)]
        self.debug_verify("on_tick");
        should_preempt
    }

    fn task_ptr(task: &TaskRef) -> UnsafeRef<Task> {
        unsafe { UnsafeRef::from_raw(Arc::as_ptr(task)) }
    }

    fn wake_task_ptr(task: &TaskRef) -> *mut Task {
        Arc::as_ptr(task) as *mut Task
    }

    fn refresh_deadlines(&self, task: &Task, mut meta: TaskSchedMeta) {
        let params = EevdfParams {
            weight: meta.weight.max(1),
            slice: meta.slice,
        };
        meta.veligible = params.eligible(self.vtime, meta.vruntime);
        meta.vdeadline = params.deadline_from_eligible(meta.veligible);
        task.set_sched_meta(meta);
    }

    fn insert_runnable_pointer(&mut self, pointer: UnsafeRef<Task>, ready: TaskReadyState) {
        let task_id = pointer.id();
        let Some(task) = self.tasks.get(&task_id).cloned() else {
            return;
        };

        let mut meta = task.sched_meta();
        meta.cpu_id = self.cpu_id;
        self.refresh_deadlines(&task, meta);
        let state = task.state();
        self.transition_task_state(
            &task,
            TaskState {
                sched: Self::scheduler_state_for_ready(ready),
                ..state
            },
        );
        task.mark_ready(TaskReadyState::Async);

        let ready_meta = task.sched_meta();
        if ready_meta.veligible <= self.vtime {
            self.eligible.insert(pointer);
        } else {
            self.ineligible.insert(pointer);
        }
    }

    fn remove_first_migratable_from_tree(
        &mut self,
        eligible: bool,
        now: Instant,
        cache_hot_threshold: Duration,
        ignore_cache_hot: bool,
    ) -> Option<TaskRef> {
        let task = if eligible {
            let mut cursor = self.eligible.front();
            let mut selected = None;
            while let Some(task) = cursor.clone_pointer() {
                if task.migration_ready()
                    && task.migration_allowed()
                    && (ignore_cache_hot
                        || !task.is_cache_hot_on(self.cpu_id, now, cache_hot_threshold))
                {
                    selected = Some(task);
                    break;
                }
                cursor.move_next();
            }
            let task = selected?;
            unsafe {
                self.eligible
                    .cursor_mut_from_ptr(intrusive_collections::UnsafeRef::into_raw(task.clone()))
                    .remove();
            }
            task
        } else {
            let mut cursor = self.ineligible.front();
            let mut selected = None;
            while let Some(task) = cursor.clone_pointer() {
                if task.migration_ready()
                    && task.migration_allowed()
                    && (ignore_cache_hot
                        || !task.is_cache_hot_on(self.cpu_id, now, cache_hot_threshold))
                {
                    selected = Some(task);
                    break;
                }
                cursor.move_next();
            }
            let task = selected?;
            unsafe {
                self.ineligible
                    .cursor_mut_from_ptr(intrusive_collections::UnsafeRef::into_raw(task.clone()))
                    .remove();
            }
            task
        };

        self.tasks.get(&task.id()).cloned()
    }

    fn promote_eligible(&mut self) {
        loop {
            let Some(veligible) = self
                .ineligible
                .front()
                .get()
                .map(|task| task.sched_meta().veligible)
            else {
                break;
            };
            if veligible > self.vtime {
                break;
            }

            let Some(pointer) = self.ineligible.front_mut().remove() else {
                break;
            };
            self.eligible.insert(pointer);
        }
    }
}
