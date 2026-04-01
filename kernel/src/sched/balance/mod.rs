use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use libakarin_core::clock::time::{Duration, Instant};
use libakarin_machine_core::cpu::PerCpuTrait;

use super::{MigrationPolicy, Scheduler, SchedulerError, TaskId, TaskRef};
use crate::{RuntimeServices, arch::PerCpu};

const LOAD_BALANCE_WEIGHT_THRESHOLD: u64 = 1024;
const LOAD_BALANCE_INTERVAL: Duration = Duration::from_millis(1);
const CACHE_HOT_THRESHOLD: Duration = Duration::from_micros(250);

/// Per-CPU load balancer for one scheduling decision point.
pub struct LoadBalancer {
    cpu_id: usize,
    now: Instant,
}

impl LoadBalancer {
    /// Create one load-balancer view for the supplied CPU and timestamp.
    pub fn new(cpu_id: usize, now: Instant) -> Self {
        Self { cpu_id, now }
    }

    fn pick_source(&self) -> Option<(usize, u64)> {
        let mut candidates = Vec::new();
        let mut best_weight = 0u64;

        for cpu_id in 0..Scheduler::cpu_count() {
            if cpu_id == self.cpu_id {
                continue;
            }

            let Some((has_work, weight)) = Scheduler::with_cpu(cpu_id, |scheduler| {
                (scheduler.has_runnable_work(), scheduler.total_weight())
            })
            .ok() else {
                continue;
            };
            if !has_work || weight == 0 {
                continue;
            }

            if weight > best_weight {
                candidates.clear();
                candidates.push(cpu_id);
                best_weight = weight;
            } else if weight == best_weight {
                candidates.push(cpu_id);
            }
        }

        if candidates.is_empty() {
            return None;
        }

        let index = super::scheduler::SCHEDULER_BALANCE_HINT.fetch_add(1, Ordering::AcqRel);
        Some((candidates[index % candidates.len()], best_weight))
    }

    fn migrate(&self, cpu_id: usize, task: TaskRef) -> Result<TaskId, SchedulerError> {
        task.set_migration_ready(false);
        let task_id = Scheduler::admit_task_on_cpu(cpu_id, task)?;
        Scheduler::notify_cpu(cpu_id);
        Ok(task_id)
    }

    fn request_remote(&self, source_cpu: usize) {
        let Ok(intercpu) = RuntimeServices::global().intercpu() else {
            return;
        };
        let requester_cpu = self.cpu_id;
        let _ = intercpu.notify_blocking(source_cpu, move || {
            let source_cpu = PerCpu::id();
            let Some(now) = Scheduler::current_time() else {
                return;
            };
            let _ = LoadBalancer::new(source_cpu, now).handle_remote_request(requester_cpu);
        });
    }

    fn handle_remote_request(&self, requester_cpu: usize) -> Result<bool, SchedulerError> {
        if self.cpu_id == requester_cpu {
            return Ok(false);
        }

        let source_weight = Scheduler::with_cpu(self.cpu_id, |scheduler| scheduler.total_weight())?;
        let target_weight =
            Scheduler::with_cpu(requester_cpu, |scheduler| scheduler.total_weight())?;
        if source_weight <= target_weight.saturating_add(LOAD_BALANCE_WEIGHT_THRESHOLD) {
            return Ok(false);
        }

        let task = Scheduler::with_cpu_mut(self.cpu_id, |scheduler| {
            let task = scheduler.take_migratable(
                self.now,
                CACHE_HOT_THRESHOLD,
                MigrationPolicy::BalanceRequest,
            );
            if task.is_some() {
                scheduler.mark_rebalanced(self.now);
            }
            task
        })?;
        let Some(task) = task else {
            return Ok(false);
        };

        let task_id = self.migrate(requester_cpu, task)?;
        Scheduler::with_cpu_mut(requester_cpu, |scheduler| {
            scheduler.mark_rebalanced(self.now)
        })?;
        log::info!(
            "[sched cpu={} -> cpu={}] migrated task {} on remote balance request",
            self.cpu_id,
            requester_cpu,
            task_id
        );
        Ok(true)
    }

    /// Try to steal or request work for the local CPU.
    pub fn pull(&self) -> Result<bool, SchedulerError> {
        let local_weight = Scheduler::with_cpu(self.cpu_id, |scheduler| {
            if !scheduler.should_rebalance(self.now, LOAD_BALANCE_INTERVAL) {
                return None;
            }
            Some(scheduler.total_weight())
        })?;
        let Some(local_weight) = local_weight else {
            return Ok(false);
        };

        let Some((source_cpu, source_weight)) = self.pick_source() else {
            return Ok(false);
        };
        if source_weight <= local_weight.saturating_add(LOAD_BALANCE_WEIGHT_THRESHOLD) {
            return Ok(false);
        }

        let task = Scheduler::with_cpu_mut(source_cpu, |scheduler| {
            let task =
                scheduler.take_migratable(self.now, CACHE_HOT_THRESHOLD, MigrationPolicy::Steal);
            if task.is_some() {
                scheduler.mark_rebalanced(self.now);
            }
            task
        })?;

        let Some(task) = task else {
            Scheduler::with_cpu_mut(self.cpu_id, |scheduler| scheduler.mark_rebalanced(self.now))?;
            self.request_remote(source_cpu);
            return Ok(false);
        };

        let task_id = self.migrate(self.cpu_id, task)?;
        Scheduler::with_cpu_mut(self.cpu_id, |scheduler| scheduler.mark_rebalanced(self.now))?;
        log::info!(
            "[sched cpu={} <- cpu={}] stole task {}",
            self.cpu_id,
            source_cpu,
            task_id
        );
        Ok(true)
    }
}
