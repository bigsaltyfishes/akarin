use super::{
    TaskId,
    eevdf::{TaskVirtualDeadline, VirtualTime},
};

/// Debug snapshot of one per-CPU EEVDF scheduler state.
#[derive(Debug, Clone, Copy)]
pub struct SchedulerCpuDebugSnapshot {
    pub cpu_id: usize,
    pub current: Option<TaskId>,
    pub vtime: VirtualTime,
    pub total_weight: u64,
    pub task_count: usize,
    pub runnable_count: usize,
    pub blocked_count: usize,
    pub terminal_count: usize,
    pub eligible_count: usize,
    pub ineligible_count: usize,
    pub promotable_ineligible_count: usize,
    pub wake_inbox_len: usize,
    pub min_vruntime: Option<VirtualTime>,
    pub max_vruntime: Option<VirtualTime>,
    pub min_vdeadline: Option<TaskVirtualDeadline>,
    pub max_vdeadline: Option<TaskVirtualDeadline>,
    pub min_vruntime_delta: i128,
    pub max_vruntime_delta: i128,
}
