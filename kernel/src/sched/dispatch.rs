use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use libakarin_boot_proto::memory::ArenaKind;
use libakarin_core::clock::time::{Duration, Instant};
use libakarin_machine_core::{
    context::TrapContextTrait,
    cpu::PerCpuTrait,
    memory::{AddressSpaceTrait, PhysAddr},
    scheduler::SchedulerHelper,
};
use libakarin_macros::cpu_local;
use libakarin_object::ObjectError;

use super::{
    preempt::{PreemptGuard, preempt_pending_local},
    scheduler::Scheduler,
    stack::CpuFallbackStack,
    task::{Task, TaskBoundaryAction},
};
use crate::{
    RuntimeServices,
    arch::{Machine, PerCpu, TrapContext},
};

static BSP_BOOTSTRAP_STACK_RECLAIMED: AtomicBool = AtomicBool::new(false);

#[inline(never)]
pub extern "C" fn scheduler_task_runner_inner() -> ! {
    SchedulerDispatch::current().task_runner_loop()
}

cpu_local! {
    static DEFERRED_MIGRATION_READY_TASK: AtomicU64 = AtomicU64::new(0);
}

/// Internal execution surface for scheduler dispatch on the current CPU.
pub struct SchedulerDispatch {
    cpu_id: usize,
}

impl SchedulerDispatch {
    /// Create a dispatch view for the current global scheduler.
    pub fn current() -> Self {
        Self {
            cpu_id: PerCpu::id(),
        }
    }

    pub fn trap_on_fallback_stack(&self, ctx: &TrapContext) -> bool {
        CpuFallbackStack::contains(self.cpu_id, ctx.stack_pointer())
    }

    fn reclaim_bsp_bootstrap_stack(&self) {
        if self.cpu_id != 0 {
            return;
        }
        if BSP_BOOTSTRAP_STACK_RECLAIMED.swap(true, Ordering::AcqRel) {
            return;
        }

        let Some(arena) = RuntimeServices::boot_info()
            .memory_map
            .iter()
            .find(|arena| arena.kind == ArenaKind::KernelStack)
        else {
            return;
        };

        crate::memory::MEMORY_SUBSYSTEM
            .reclaim_reserved_range(PhysAddr::new(arena.start)..PhysAddr::new(arena.end));
        info!(
            "[kernel/scheduler] reclaimed BSP bootstrap stack: phys=[{:#x}..{:#x})",
            arena.start, arena.end
        );
    }

    pub fn defer_migration_ready(&self, task_id: u64) {
        DEFERRED_MIGRATION_READY_TASK
            .with_current(|slot: &mut AtomicU64| slot.store(task_id, Ordering::Release));
    }

    pub fn flush_deferred_migration_ready(&self) {
        let task_id = DEFERRED_MIGRATION_READY_TASK
            .with_current(|slot: &mut AtomicU64| slot.swap(0, Ordering::AcqRel));
        if task_id == 0 {
            return;
        }

        let cpu_id = PerCpu::id();
        let task = Scheduler::task_on_cpu(cpu_id, task_id);
        if let Some(task) = task {
            task.set_migration_ready(true);
        }
    }

    fn seed_idle_context(&self, ctx: &mut TrapContext) {
        *ctx = TrapContext::new_kernel();
        ctx.set_instruction_pointer(Self::idle_entry as *const () as usize);
        ctx.set_stack_pointer(CpuFallbackStack::boot_entry_stack_pointer(self.cpu_id));
        ctx.set_interrupt_en(true);
    }

    fn idle_resume_frame(&self) -> TrapContext {
        let mut ctx = TrapContext::new_kernel();
        self.seed_idle_context(&mut ctx);
        ctx
    }

    fn enter_idle_shell(&self) -> ! {
        unsafe {
            Machine::enter_idle_shell(CpuFallbackStack::top(self.cpu_id), Self::idle_entry);
        }
    }

    fn dispatch_current_task(&self, entry_guard: Option<PreemptGuard>) -> ! {
        let task = self.current_task();
        task.mark_started();
        task.set_migration_ready(false);

        if task.cancel_requested() {
            task.publish_boundary(TaskBoundaryAction::Cancelled);
            self.invoke_local_reschedule();
        }

        drop(entry_guard);

        let outcome = task.poll_boundary();
        task.save_kernel_simd_state();
        task.publish_boundary(TaskBoundaryAction::Outcome(outcome));
        self.invoke_local_reschedule();
    }

    fn current_task(&self) -> Arc<Task> {
        Scheduler::current_task_on_cpu(self.cpu_id)
            .expect("missing current task at kernel execution boundary")
    }

    pub fn reschedule_now(self) -> ! {
        self.invoke_local_reschedule();
    }

    fn task_resume_frame(&self, task: &Task) -> Result<TrapContext, ObjectError> {
        let next_root = task.address_space_root()?;
        if Machine::current_base() != next_root {
            unsafe {
                Machine::switch_base(next_root);
            }
        }

        Ok(task.kernel_resume_frame(Machine::task_runner_trampoline as *const () as usize))
    }

    pub fn select_next_or_idle_frame(&self) -> TrapContext {
        self.pick_next_resume_frame()
            .ok()
            .flatten()
            .unwrap_or_else(|| self.idle_resume_frame())
    }

    fn pick_next_resume_frame(&self) -> Result<Option<TrapContext>, ObjectError> {
        let now =
            Scheduler::current_time().unwrap_or_else(|| Instant::new(Duration::from_nanos(0)));
        let Some(task) = Scheduler::take_next_task_on_cpu(self.cpu_id, now)
            .map_err(|_| ObjectError::InvalidArgument)?
        else {
            return Ok(None);
        };

        self.task_resume_frame(&task).map(Some)
    }

    /// Enter the current CPU's long-running scheduler loop.
    pub fn enter(self) -> ! {
        self.enter_idle_shell()
    }

    fn invoke_local_reschedule(&self) -> ! {
        unsafe {
            Machine::invoke_local_reschedule();
        }
        panic!("local reschedule trap unexpectedly returned to kernel task dispatcher");
    }

    fn task_runner_loop(&self) -> ! {
        loop {
            let guard = PreemptGuard::enter_deferred();
            PerCpu::enable_interrupt();
            self.dispatch_current_task(Some(guard));
        }
    }

    extern "C" fn idle_entry() -> ! {
        Self::current().reclaim_bsp_bootstrap_stack();
        Self::idle_loop()
    }

    fn idle_loop() -> ! {
        loop {
            let dispatch = Self::current();
            dispatch.flush_deferred_migration_ready();
            PerCpu::disable_interrupt();
            let should_sleep =
                !Scheduler::cpu_has_work(dispatch.cpu_id) && !preempt_pending_local();
            if should_sleep {
                PerCpu::idle_until_interrupt();
            } else {
                PerCpu::enable_interrupt();
                unsafe {
                    Machine::invoke_local_reschedule();
                }
            }
        }
    }
}
