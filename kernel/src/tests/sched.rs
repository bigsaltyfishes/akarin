use alloc::{format, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use libakarin_core::{
    clock::time::Duration,
    memory::{
        PAGE_SIZE, RegionPurpose, VMO_DEFAULT_INTERFACE_CAPS, VmFlags, VmLayoutSegment, VmRange,
        Vmo, VmoPagePurpose,
    },
};
use libakarin_machine_core::{
    context::TrapContextTrait,
    cpu::{PERCPU_TEMPLATE_END, PERCPU_TEMPLATE_START, PerCpuTrait},
    memory::{VirtAddr, paging::MMUFlags},
};
use libakarin_object::{Capability, CpAccessMode, ObjectError, Payload};
use libakarin_syscall::{
    FutexError, SYSCALL_STATUS_OK, Syscall, SyscallArgs, VmError, VmoOpRangeOperation,
};

use crate::{
    RuntimeServices, TaskExit,
    arch::{self, Machine, PerCpu},
    sched::{
        Scheduler,
        process::{
            Process, ProcessUserFaultResolution, ProcessVmError, allocate_user_stack_for_process,
            release_user_stack_for_process,
        },
    },
    service::{PagerObject, ServiceFrame, SyscallHandlerObject, UserObject, dispatcher},
    syscall,
};

const KERNEL_PROBE_ROUNDS: usize = 4;
const SMP_BALANCE_PROBE_TASKS_PER_CPU: usize = 2;
const FAIRNESS_TASKS_PER_CPU: usize = 2;
const FAIRNESS_MIXED_TASKS_PER_CPU: usize = 2;
const TIMER_PROBE_TIMEOUT_TICKS: u64 = 16;
const FAIRNESS_WINDOW_TICKS: u64 = 64;
const FAIRNESS_COMPLETION_TIMEOUT_TICKS: u64 = 64;
const FAIRNESS_WARN_SPREAD_PERMILLE: u64 = 2000;
const TEARDOWN_STRESS_ROUNDS: usize = 4;
const TEARDOWN_SIBLING_TASKS: usize = 3;
const TEARDOWN_TIMEOUT_TICKS: u64 = 64;
const TEARDOWN_USER_STACK_PAGES: usize = 4;
const SPAWN_RACE_STRESS_ROUNDS: usize = 64;
const SPAWN_RACE_TIMEOUT_TICKS: u64 = 64;
const FUTEX_WAIT_WAKE_TIMEOUT_TICKS: u64 = 64;
const FUTEX_TIMEOUT_PROBE_TIMEOUT_TICKS: u64 = 128;
const FUTEX_TIMEOUT_PROBE_WAIT: Duration = Duration::from_millis(32);

static SCHED_CPU_LOCAL_PROBE_MASK: AtomicUsize = AtomicUsize::new(0);
static SCHED_CPU_LOCAL_DONE_MASK: AtomicUsize = AtomicUsize::new(0);
static SCHED_MIGRATION_PROBE_MASK: AtomicUsize = AtomicUsize::new(0);
static FAIRNESS_WINDOW_NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);
static FAIRNESS_WINDOW_ACTIVE_EPOCH: AtomicU64 = AtomicU64::new(0);
static FAIRNESS_WINDOW_DEADLINE_TICKS: AtomicU64 = AtomicU64::new(0);

/// Scheduler-focused runtime self-tests.
pub struct SchedulerSelfTests;

#[derive(Debug)]
enum FutexProbeSetupError {
    Object(ObjectError),
    Vm(VmError),
}

fn service_probe_caller_done(state: usize) -> bool {
    if state == 1 {
        return true;
    }
    if state == 2 {
        return true;
    }
    state == 3
}

fn pager_probe_caller_done(state: usize) -> bool {
    if state == 0x42 {
        return true;
    }
    if state == 0x43 {
        return true;
    }
    state == 0x44
}

fn syscall_handler_probe_caller_done(state: usize) -> bool {
    if state == 0x52 {
        return true;
    }
    if state == 0x53 {
        return true;
    }
    state == 0x54
}

#[derive(Default)]
struct FairnessProbe {
    progress: AtomicU64,
    migrations: AtomicU64,
    cpu_mask: AtomicUsize,
    last_cpu: AtomicUsize,
}

impl FairnessProbe {
    const UNSET_CPU: usize = usize::MAX;

    fn new() -> Self {
        Self {
            progress: AtomicU64::new(0),
            migrations: AtomicU64::new(0),
            cpu_mask: AtomicUsize::new(0),
            last_cpu: AtomicUsize::new(Self::UNSET_CPU),
        }
    }

    fn record_progress(&self, cpu_id: usize) {
        self.record_progress_batch(cpu_id, 1);
    }

    fn record_progress_batch(&self, cpu_id: usize, count: u64) {
        self.progress.fetch_add(count, Ordering::Relaxed);
        if cpu_id < usize::BITS as usize {
            self.cpu_mask.fetch_or(1usize << cpu_id, Ordering::Relaxed);
        }
        let previous = self.last_cpu.swap(cpu_id, Ordering::AcqRel);
        if previous != Self::UNSET_CPU && previous != cpu_id {
            self.migrations.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn progress(&self) -> u64 {
        self.progress.load(Ordering::Acquire)
    }

    fn migrations(&self) -> u64 {
        self.migrations.load(Ordering::Acquire)
    }

    fn cpu_mask(&self) -> usize {
        self.cpu_mask.load(Ordering::Acquire)
    }

    fn last_cpu(&self) -> Option<usize> {
        match self.last_cpu.load(Ordering::Acquire) {
            Self::UNSET_CPU => None,
            cpu_id => Some(cpu_id),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct FairnessSummary {
    task_count: usize,
    min_progress: u64,
    max_progress: u64,
    mean_progress: u64,
    spread_permille: u64,
    max_deviation_permille: u64,
    total_migrations: u64,
    observed_cpu_mask: usize,
}

impl FairnessSummary {
    fn gather(probes: &[Arc<FairnessProbe>]) -> Self {
        let mut min_progress = u64::MAX;
        let mut max_progress = 0u64;
        let mut total_progress = 0u128;
        let mut total_migrations = 0u64;
        let mut observed_cpu_mask = 0usize;

        for probe in probes {
            let progress = probe.progress();
            min_progress = min_progress.min(progress);
            max_progress = max_progress.max(progress);
            total_progress = total_progress.saturating_add(progress as u128);
            total_migrations = total_migrations.saturating_add(probe.migrations());
            observed_cpu_mask |= probe.cpu_mask();
        }

        if probes.is_empty() {
            min_progress = 0;
        }

        let task_count = probes.len();
        let mean_progress = if task_count == 0 {
            0
        } else {
            (total_progress / task_count as u128) as u64
        };
        let spread_permille = if min_progress == 0 {
            u64::MAX
        } else {
            max_progress.saturating_mul(1000) / min_progress
        };
        let mut max_deviation = 0u64;
        for probe in probes {
            let progress = probe.progress();
            let deviation = progress.abs_diff(mean_progress);
            max_deviation = max_deviation.max(deviation);
        }
        let max_deviation_permille = if mean_progress == 0 {
            u64::MAX
        } else {
            max_deviation.saturating_mul(1000) / mean_progress
        };

        Self {
            task_count,
            min_progress,
            max_progress,
            mean_progress,
            spread_permille,
            max_deviation_permille,
            total_migrations,
            observed_cpu_mask,
        }
    }
}

impl SchedulerSelfTests {
    const CPU_BOUND_BATCH: u64 = 256;
    const TEARDOWN_CPU: usize = 0;

    pub async fn run() {
        info!("[kernel/tests] scheduler self-tests: basic probes start");
        Self::run_basic_probes().await;
        info!("[kernel/tests] scheduler self-tests: basic probes done");
        info!("[kernel/tests] scheduler self-tests: timer probe start");
        Self::run_timer_probe().await;
        info!("[kernel/tests] scheduler self-tests: timer probe done");
        info!("[kernel/tests] scheduler self-tests: futex probes start");
        Self::run_futex_probes().await;
        info!("[kernel/tests] scheduler self-tests: futex probes done");
        info!("[kernel/tests] scheduler self-tests: service object probes start");
        Self::run_service_object_probes().await;
        info!("[kernel/tests] scheduler self-tests: service object probes done");
        info!("[kernel/tests] scheduler self-tests: pager object probes start");
        Self::run_pager_object_probes().await;
        info!("[kernel/tests] scheduler self-tests: pager object probes done");
        info!("[kernel/tests] scheduler self-tests: pager fault probes start");
        Self::run_pager_fault_probes().await;
        info!("[kernel/tests] scheduler self-tests: pager fault probes done");
        info!("[kernel/tests] scheduler self-tests: syscall handler probes start");
        Self::run_syscall_handler_probes().await;
        info!("[kernel/tests] scheduler self-tests: syscall handler probes done");
        info!("[kernel/tests] scheduler self-tests: fairness probes start");
        Self::run_fairness_probes().await;
        info!("[kernel/tests] scheduler self-tests: fairness probes done");
        info!("[kernel/tests] scheduler self-tests: teardown probes start");
        Self::run_teardown_probes().await;
        info!("[kernel/tests] scheduler self-tests: teardown probes done");
        info!("[kernel/tests] scheduler self-tests: spawn race probes start");
        Self::run_spawn_race_probes().await;
        info!("[kernel/tests] scheduler self-tests: spawn race probes done");
    }

    async fn run_basic_probes() {
        let pcpu_start = unsafe { &PERCPU_TEMPLATE_START as *const u8 as usize };
        let pcpu_end = unsafe { &PERCPU_TEMPLATE_END as *const u8 as usize };
        let cpu_count = PerCpu::count();
        log::info!(
            "Per-CPU template located at: {:#x} - {:#x}",
            pcpu_start,
            pcpu_end
        );

        if cpu_count > 1 {
            Self::run_smp_cpu_local_probe(cpu_count).await;
            Self::run_smp_load_balance_probe(cpu_count).await;
            Self::run_intercpu_probe().await;
        } else {
            Self::run_single_cpu_probe().await;
        }
    }

    async fn run_single_cpu_probe() {
        match Scheduler::spawn(Scheduler::kernel_process(), async {
            for round in 1..=KERNEL_PROBE_ROUNDS {
                log::info!("[kernel/scheduler cpu=0] probe task A round {}", round);
                Scheduler::yield_now().await;
            }
            log::info!("[kernel/scheduler cpu=0] probe task A completed");
            TaskExit::Completed
        }) {
            Ok(task_id) => info!("[kernel/scheduler] probe task A spawned as {}", task_id),
            Err(err) => warn!("[kernel/scheduler] probe task A spawn failed: {:?}", err),
        }

        match Scheduler::spawn(Scheduler::kernel_process(), async {
            for round in 1..=KERNEL_PROBE_ROUNDS {
                log::info!("[kernel/scheduler cpu=0] probe task B round {}", round);
                Scheduler::yield_now().await;
            }
            log::info!("[kernel/scheduler cpu=0] probe task B completed");
            TaskExit::Completed
        }) {
            Ok(task_id) => info!("[kernel/scheduler] probe task B spawned as {}", task_id),
            Err(err) => warn!("[kernel/scheduler] probe task B spawn failed: {:?}", err),
        }

        for round in 1..=12 {
            log::info!(
                "[kernel/scheduler] kernel scheduler self-test round {} yielding",
                round
            );
            Scheduler::yield_now().await;
        }
    }

    async fn run_smp_cpu_local_probe(cpu_count: usize) {
        SCHED_CPU_LOCAL_PROBE_MASK.store(0, Ordering::Release);
        SCHED_CPU_LOCAL_DONE_MASK.store(0, Ordering::Release);
        Scheduler::allow_current_migration(false);
        let expected_mask = Self::expected_cpu_mask(cpu_count);

        for cpu_id in 0..cpu_count {
            match Scheduler::spawn_on(cpu_id, Scheduler::kernel_process(), async move {
                Scheduler::allow_current_migration(false);
                let observed = PerCpu::id();
                Self::mark_cpu_observed(&SCHED_CPU_LOCAL_PROBE_MASK, observed);
                log::info!(
                    "[kernel/scheduler cpu={}] cpu-local probe target={}",
                    observed,
                    cpu_id
                );
                for _ in 0..KERNEL_PROBE_ROUNDS {
                    Scheduler::yield_now().await;
                }
                Self::mark_cpu_observed(&SCHED_CPU_LOCAL_DONE_MASK, observed);
                TaskExit::Completed
            }) {
                Ok(task_id) => info!(
                    "[kernel/scheduler] cpu-local probe for cpu {} spawned as {}",
                    cpu_id, task_id
                ),
                Err(err) => warn!(
                    "[kernel/scheduler] cpu-local probe for cpu {} failed: {:?}",
                    cpu_id, err
                ),
            }
        }

        for _ in 0..256 {
            if SCHED_CPU_LOCAL_PROBE_MASK.load(Ordering::Acquire) == expected_mask {
                break;
            }
            Self::progress_system(cpu_count).await;
        }

        info!(
            "[kernel/scheduler] smp cpu-local probe observed={:#x} expected={:#x}",
            SCHED_CPU_LOCAL_PROBE_MASK.load(Ordering::Acquire),
            expected_mask
        );

        for _ in 0..256 {
            if SCHED_CPU_LOCAL_DONE_MASK.load(Ordering::Acquire) == expected_mask {
                break;
            }
            Self::progress_system(cpu_count).await;
        }

        for _ in 0..256 {
            let mut ap_idle = true;
            for cpu_id in 1..cpu_count {
                if Scheduler::cpu_has_work(cpu_id) {
                    ap_idle = false;
                    break;
                }
            }
            if ap_idle {
                break;
            }
            Self::progress_system(cpu_count).await;
        }
    }

    async fn run_smp_load_balance_probe(cpu_count: usize) {
        SCHED_MIGRATION_PROBE_MASK.store(0, Ordering::Release);
        let done = Arc::new(AtomicUsize::new(0));
        let task_count = cpu_count * SMP_BALANCE_PROBE_TASKS_PER_CPU;
        let balance_epoch = Self::arm_fairness_window(FAIRNESS_WINDOW_TICKS);

        for task_index in 0..task_count {
            let done = Arc::clone(&done);
            match Scheduler::spawn_on(0, Scheduler::kernel_process(), async move {
                let mut last_cpu = usize::MAX;
                while !Self::fairness_window_closed(balance_epoch) {
                    let current = PerCpu::id();
                    if current != last_cpu {
                        if current != 0 {
                            Self::mark_cpu_observed(&SCHED_MIGRATION_PROBE_MASK, current);
                        }
                        log::info!(
                            "[kernel/scheduler cpu={}] balance probe task={} migrated",
                            current,
                            task_index
                        );
                        last_cpu = current;
                    }
                    for _ in 0..Self::CPU_BOUND_BATCH {
                        core::hint::spin_loop();
                    }
                }
                done.fetch_add(1, Ordering::AcqRel);
                TaskExit::Completed
            }) {
                Ok(task_id) => info!(
                    "[kernel/scheduler] balance probe task {} spawned as {}",
                    task_index, task_id
                ),
                Err(err) => warn!(
                    "[kernel/scheduler] balance probe task {} failed: {:?}",
                    task_index, err
                ),
            }
        }

        while !Self::fairness_window_closed(balance_epoch) {
            Self::progress_system(cpu_count).await;
        }

        let deadline = Self::cpu0_test_ticks().saturating_add(FAIRNESS_COMPLETION_TIMEOUT_TICKS);
        while Self::cpu0_test_ticks() < deadline {
            if done.load(Ordering::Acquire) == task_count {
                break;
            }
            Self::progress_system(cpu_count).await;
        }
        if done.load(Ordering::Acquire) != task_count {
            warn!(
                "[kernel/tests] smp load-balance completion lagged: done={} tasks={}",
                done.load(Ordering::Acquire),
                task_count
            );
        }

        info!(
            "[kernel/scheduler] smp load-balance probe observed={:#x}",
            SCHED_MIGRATION_PROBE_MASK.load(Ordering::Acquire)
        );
        Scheduler::allow_current_migration(true);
    }

    async fn run_intercpu_probe() {
        info!("[kernel/tests] intercpu probe start");
        let done = Arc::new(AtomicUsize::new(0));
        let remote_cpu = Arc::new(AtomicUsize::new(usize::MAX));

        match RuntimeServices::global().intercpu() {
            Ok(runtime) => {
                let done_task = Arc::clone(&done);
                let remote_cpu_task = Arc::clone(&remote_cpu);
                match Scheduler::spawn(Scheduler::kernel_process(), async move {
                    let local_cpu = PerCpu::id();
                    info!("[kernel/tests] intercpu helper start cpu={}", local_cpu);
                    match runtime.call(1, || PerCpu::id()).await {
                        Ok(cpu_id) => {
                            info!(
                                "[kernel/tests] intercpu helper reply cpu={} local_cpu={}",
                                cpu_id, local_cpu
                            );
                            remote_cpu_task.store(cpu_id, Ordering::Release);
                            done_task.store(1, Ordering::Release);
                        }
                        Err(err) => {
                            warn!(
                                "[kernel/intercpu] mailbox self-test failed on cpu {}: {:?}",
                                local_cpu, err
                            );
                            done_task.store(2, Ordering::Release);
                        }
                    }
                    TaskExit::Completed
                }) {
                    Ok(task_id) => info!(
                        "[kernel/tests] intercpu probe helper spawned as {}",
                        task_id
                    ),
                    Err(err) => {
                        warn!(
                            "[kernel/tests] intercpu probe helper spawn failed: {:?}",
                            err
                        );
                        return;
                    }
                }

                let deadline = Self::cpu0_test_ticks().saturating_add(TIMER_PROBE_TIMEOUT_TICKS);
                while Self::cpu0_test_ticks() < deadline {
                    match done.load(Ordering::Acquire) {
                        1 => {
                            log::info!(
                                "[kernel/intercpu] mailbox self-test reached cpu {} (runtime \
                                 cpus={})",
                                remote_cpu.load(Ordering::Acquire),
                                runtime.cpu_count()
                            );
                            info!("[kernel/tests] intercpu probe done");
                            return;
                        }
                        2 => {
                            info!("[kernel/tests] intercpu probe done (failed)");
                            return;
                        }
                        _ => Scheduler::yield_now().await,
                    }
                }

                warn!(
                    "[kernel/tests] intercpu probe timed out after {} cpu0 ticks",
                    TIMER_PROBE_TIMEOUT_TICKS
                );
            }
            Err(err) => warn!(
                "[kernel/intercpu] mailbox runtime unavailable during self-test: {:?}",
                err
            ),
        }
    }

    async fn run_timer_probe() {
        info!("[kernel/tests] timer probe: spawn begin");
        let resumed = Arc::new(AtomicBool::new(false));
        let resumed_task = Arc::clone(&resumed);
        match Scheduler::spawn(Scheduler::kernel_process(), async move {
            log::info!("[kernel/scheduler] timer self-test armed");
            Scheduler::sleep(Duration::from_millis(1)).await;
            resumed_task.store(true, Ordering::Release);
            TaskExit::Completed
        }) {
            Ok(task_id) => info!("[kernel/scheduler] timer self-test spawned as {}", task_id),
            Err(err) => warn!("[kernel/scheduler] timer self-test spawn failed: {:?}", err),
        }
        info!("[kernel/tests] timer probe: spawn end");

        let deadline = Self::cpu0_test_ticks().saturating_add(TIMER_PROBE_TIMEOUT_TICKS);
        while Self::cpu0_test_ticks() < deadline {
            if resumed.load(Ordering::Acquire) {
                return;
            }
            Scheduler::yield_now().await;
        }

        if !resumed.load(Ordering::Acquire) {
            warn!(
                "[kernel/tests] timer self-test did not complete within {} cpu0 ticks",
                TIMER_PROBE_TIMEOUT_TICKS
            );
        }
    }

    async fn run_fairness_probes() {
        let cpu_count = PerCpu::count();
        Self::settle_system(cpu_count).await;
        Self::run_equal_cpu_bound_fairness(cpu_count).await;
        Self::settle_system(cpu_count).await;
        Self::run_mixed_sleep_fairness(cpu_count).await;
    }

    async fn run_futex_probes() {
        let cpu_count = PerCpu::count();
        Self::settle_system(cpu_count).await;
        Self::run_futex_wait_wake_probe(cpu_count).await;
        Self::settle_system(cpu_count).await;
        Self::run_futex_timeout_probe(cpu_count).await;
        Self::settle_system(cpu_count).await;
    }

    async fn run_service_object_probes() {
        const METHOD_ID: usize = 0x1234;
        const SERVICE_IP: usize = 0x0040_1000;
        const SERVER_STARTED: usize = 0x20;
        const SERVER_WAITING: usize = 0x21;
        const SERVER_WAIT_ERR: usize = 0x22;
        const SERVER_NO_DELIVERY: usize = 0x23;
        const SERVER_BAD_DELIVERY: usize = 0x24;
        const SERVER_REPLY_ERR: usize = 0x25;
        const SERVER_DONE_STATE: usize = 0x26;
        const CALLER_STARTED: usize = 0x10;
        const CALLER_INVOKING: usize = 0x11;
        const CALLER_OK: usize = 1;
        const CALLER_BAD_REPLY: usize = 2;
        const CALLER_INVOKE_ERROR: usize = 3;

        let process = Scheduler::kernel_process();
        let object = UserObject::new(Arc::clone(&process), SERVICE_IP);
        let expected_reply = libakarin_syscall::SyscallResult::new(
            SYSCALL_STATUS_OK,
            [
                0xCA11_0001,
                0xCA11_0002,
                0xCA11_0003,
                0xCA11_0004,
                0xCA11_0005,
            ],
        );
        let server_ready = Arc::new(AtomicBool::new(false));
        let server_done = Arc::new(AtomicBool::new(false));
        let server_state = Arc::new(AtomicUsize::new(0));
        let caller_state = Arc::new(AtomicUsize::new(0));
        let server_ready_flag = Arc::clone(&server_ready);
        let server_done_flag = Arc::clone(&server_done);
        let server_state_flag = Arc::clone(&server_state);
        let caller_state_flag = Arc::clone(&caller_state);
        let server_object = object.clone();
        let server_cpu = Self::TEARDOWN_CPU.min(PerCpu::count().saturating_sub(1));
        if let Err(err) = Scheduler::spawn_on(server_cpu, &process, async move {
            let Some(task) = Scheduler::current_task_ref() else {
                return TaskExit::ThreadExited(0x6100);
            };
            server_state_flag.store(SERVER_STARTED, Ordering::Release);
            server_ready_flag.store(true, Ordering::Release);
            server_state_flag.store(SERVER_WAITING, Ordering::Release);
            if server_object.wait(Arc::clone(&task)).await.is_err() {
                server_state_flag.store(SERVER_WAIT_ERR, Ordering::Release);
                return TaskExit::ThreadExited(0x6101);
            }
            let Some(delivery) = task.take_service_delivery() else {
                server_state_flag.store(SERVER_NO_DELIVERY, Ordering::Release);
                return TaskExit::ThreadExited(0x6102);
            };
            let expected_request = ServiceFrame::object_request(
                CpAccessMode::Execute,
                u32::MAX,
                METHOD_ID,
                0x11,
                0x22,
            );
            if delivery.usr_ip != SERVICE_IP || delivery.frame != expected_request {
                server_state_flag.store(SERVER_BAD_DELIVERY, Ordering::Release);
                return TaskExit::ThreadExited(0x6103);
            }
            if dispatcher()
                .complete_current_call(&task, expected_reply)
                .is_err()
            {
                server_state_flag.store(SERVER_REPLY_ERR, Ordering::Release);
                return TaskExit::ThreadExited(0x6104);
            }
            server_state_flag.store(SERVER_DONE_STATE, Ordering::Release);
            server_done_flag.store(true, Ordering::Release);
            TaskExit::Completed
        }) {
            warn!(
                "[kernel/tests/service] failed to spawn service probe server: {:?}",
                err
            );
            return;
        }

        let cpu_count = PerCpu::count();
        let ready_deadline = Self::cpu0_test_ticks().saturating_add(FUTEX_WAIT_WAKE_TIMEOUT_TICKS);
        while !server_ready.load(Ordering::Acquire) && Self::cpu0_test_ticks() < ready_deadline {
            Self::progress_system(cpu_count).await;
        }
        if !server_ready.load(Ordering::Acquire) {
            warn!("[kernel/tests/service] service probe server did not reach wait state");
            return;
        }

        let caller_object = object.clone();
        let caller_task_id = match Scheduler::spawn_on(server_cpu, &process, async move {
            caller_state_flag.store(CALLER_STARTED, Ordering::Release);
            caller_state_flag.store(CALLER_INVOKING, Ordering::Release);
            match caller_object
                .invoke(CpAccessMode::Execute, u32::MAX, METHOD_ID, 0x11, 0x22)
                .await
            {
                Ok(reply) if reply == expected_reply => {
                    caller_state_flag.store(CALLER_OK, Ordering::Release);
                    TaskExit::Completed
                }
                Ok(reply) => {
                    warn!(
                        "[kernel/tests/service] unexpected service-object reply: {:?}",
                        reply
                    );
                    caller_state_flag.store(CALLER_BAD_REPLY, Ordering::Release);
                    TaskExit::ThreadExited(0x6105)
                }
                Err(err) => {
                    warn!(
                        "[kernel/tests/service] service-object invoke failed: {:?}",
                        err
                    );
                    caller_state_flag.store(CALLER_INVOKE_ERROR, Ordering::Release);
                    TaskExit::ThreadExited(0x6106)
                }
            }
        }) {
            Ok(task_id) => task_id,
            Err(err) => {
                warn!(
                    "[kernel/tests/service] failed to spawn service probe caller: {:?}",
                    err
                );
                return;
            }
        };

        let done_deadline = Self::cpu0_test_ticks().saturating_add(FUTEX_WAIT_WAKE_TIMEOUT_TICKS);
        while Self::cpu0_test_ticks() < done_deadline {
            let caller_done = service_probe_caller_done(caller_state.load(Ordering::Acquire));
            if caller_done && server_done.load(Ordering::Acquire) {
                break;
            }
            Self::progress_system(cpu_count).await;
        }
        if caller_state.load(Ordering::Acquire) != CALLER_OK {
            let mut caller_snapshot = None;
            for cpu_id in 0..PerCpu::count() {
                let Some(task) = Scheduler::task_on_cpu(cpu_id, caller_task_id) else {
                    continue;
                };
                caller_snapshot =
                    Some((cpu_id, task.state(), task.sched_meta(), task.has_runnable()));
                break;
            }
            warn!(
                "[kernel/tests/service] service probe caller did not complete successfully: \
                 state={:#x} server_state={:#x} server_done={} caller_task={:?}",
                caller_state.load(Ordering::Acquire),
                server_state.load(Ordering::Acquire),
                server_done.load(Ordering::Acquire),
                caller_snapshot
            );
            return;
        }
        if !server_done.load(Ordering::Acquire) {
            warn!("[kernel/tests/service] service probe server did not finish reply path");
        }
    }

    async fn run_pager_object_probes() {
        const SERVICE_IP: usize = 0x0040_2000;
        const SERVER_STARTED: usize = 0x30;
        const SERVER_WAITING: usize = 0x31;
        const SERVER_WAIT_ERR: usize = 0x32;
        const SERVER_NO_DELIVERY: usize = 0x33;
        const SERVER_BAD_DELIVERY: usize = 0x34;
        const SERVER_REPLY_ERR: usize = 0x35;
        const SERVER_DONE_STATE: usize = 0x36;
        const CALLER_STARTED: usize = 0x40;
        const CALLER_INVOKING: usize = 0x41;
        const CALLER_OK: usize = 0x42;
        const CALLER_BAD_REPLY: usize = 0x43;
        const CALLER_INVOKE_ERROR: usize = 0x44;

        let process = Scheduler::kernel_process();
        let pager = PagerObject::new(Arc::clone(&process), SERVICE_IP);
        let expected_reply = libakarin_syscall::SyscallResult::new(
            SYSCALL_STATUS_OK,
            [
                0xCA22_0001,
                0xCA22_0002,
                0xCA22_0003,
                0xCA22_0004,
                0xCA22_0005,
            ],
        );
        let server_ready = Arc::new(AtomicBool::new(false));
        let server_done = Arc::new(AtomicBool::new(false));
        let server_state = Arc::new(AtomicUsize::new(0));
        let caller_state = Arc::new(AtomicUsize::new(0));
        let server_ready_flag = Arc::clone(&server_ready);
        let server_done_flag = Arc::clone(&server_done);
        let server_state_flag = Arc::clone(&server_state);
        let caller_state_flag = Arc::clone(&caller_state);
        let server_pager = pager.clone();
        let server_cpu = Self::TEARDOWN_CPU.min(PerCpu::count().saturating_sub(1));
        if let Err(err) = Scheduler::spawn_on(server_cpu, &process, async move {
            let Some(task) = Scheduler::current_task_ref() else {
                return TaskExit::ThreadExited(0x6200);
            };
            server_state_flag.store(SERVER_STARTED, Ordering::Release);
            server_ready_flag.store(true, Ordering::Release);
            server_state_flag.store(SERVER_WAITING, Ordering::Release);
            if server_pager.wait(Arc::clone(&task)).await.is_err() {
                server_state_flag.store(SERVER_WAIT_ERR, Ordering::Release);
                return TaskExit::ThreadExited(0x6201);
            }
            let Some(delivery) = task.take_service_delivery() else {
                server_state_flag.store(SERVER_NO_DELIVERY, Ordering::Release);
                return TaskExit::ThreadExited(0x6202);
            };
            let expected_request =
                ServiceFrame::pager_fault_request(0x4010_0000, 0x7, 0x55aa, 8, 1);
            if delivery.usr_ip != SERVICE_IP || delivery.frame != expected_request {
                server_state_flag.store(SERVER_BAD_DELIVERY, Ordering::Release);
                return TaskExit::ThreadExited(0x6203);
            }
            if dispatcher()
                .complete_current_call(&task, expected_reply)
                .is_err()
            {
                server_state_flag.store(SERVER_REPLY_ERR, Ordering::Release);
                return TaskExit::ThreadExited(0x6204);
            }
            server_state_flag.store(SERVER_DONE_STATE, Ordering::Release);
            server_done_flag.store(true, Ordering::Release);
            TaskExit::Completed
        }) {
            warn!(
                "[kernel/tests/pager] failed to spawn pager probe server: {:?}",
                err
            );
            return;
        }

        let cpu_count = PerCpu::count();
        let ready_deadline = Self::cpu0_test_ticks().saturating_add(FUTEX_WAIT_WAKE_TIMEOUT_TICKS);
        while !server_ready.load(Ordering::Acquire) && Self::cpu0_test_ticks() < ready_deadline {
            Self::progress_system(cpu_count).await;
        }
        if !server_ready.load(Ordering::Acquire) {
            warn!("[kernel/tests/pager] pager probe server did not reach wait state");
            return;
        }

        let caller_pager = pager.clone();
        let caller_task_id = match Scheduler::spawn_on(server_cpu, &process, async move {
            caller_state_flag.store(CALLER_STARTED, Ordering::Release);
            caller_state_flag.store(CALLER_INVOKING, Ordering::Release);
            match caller_pager
                .submit_fault(0x4010_0000, 0x7, 0x55aa, 8, 1)
                .await
            {
                Ok(reply) if reply == expected_reply => {
                    caller_state_flag.store(CALLER_OK, Ordering::Release);
                    TaskExit::Completed
                }
                Ok(reply) => {
                    warn!("[kernel/tests/pager] unexpected pager reply: {:?}", reply);
                    caller_state_flag.store(CALLER_BAD_REPLY, Ordering::Release);
                    TaskExit::ThreadExited(0x6205)
                }
                Err(err) => {
                    warn!("[kernel/tests/pager] pager submit failed: {:?}", err);
                    caller_state_flag.store(CALLER_INVOKE_ERROR, Ordering::Release);
                    TaskExit::ThreadExited(0x6206)
                }
            }
        }) {
            Ok(task_id) => task_id,
            Err(err) => {
                warn!(
                    "[kernel/tests/pager] failed to spawn pager probe caller: {:?}",
                    err
                );
                return;
            }
        };

        let completion_deadline =
            Self::cpu0_test_ticks().saturating_add(FUTEX_WAIT_WAKE_TIMEOUT_TICKS);
        while (!server_done.load(Ordering::Acquire)
            || !pager_probe_caller_done(caller_state.load(Ordering::Acquire)))
            && Self::cpu0_test_ticks() < completion_deadline
        {
            Self::progress_system(cpu_count).await;
        }
        if !server_done.load(Ordering::Acquire) {
            warn!(
                "[kernel/tests/pager] pager probe server did not complete: state=0x{:x}",
                server_state.load(Ordering::Acquire)
            );
        }
        let caller_final = caller_state.load(Ordering::Acquire);
        if !pager_probe_caller_done(caller_final) {
            warn!(
                "[kernel/tests/pager] pager probe caller did not complete successfully: \
                 state=0x{:x}",
                caller_final
            );
        }
        let _ = caller_task_id;
    }

    async fn run_pager_fault_probes() {
        const SERVICE_IP: usize = 0x0040_3000;
        const PAGER_COOKIE: usize = 0x7711;

        let process = match RuntimeServices::global()
            .namespaces()
            .scheduler_manager()
            .create_process("pager-fault-probe", Self::test_process_root_range())
        {
            Ok(process) => process,
            Err(err) => {
                warn!(
                    "[kernel/tests/pager] failed to create pager-fault process: {:?}",
                    err
                );
                return;
            }
        };
        if let Err(err) = Self::preflight_process_resources(&process) {
            warn!(
                "[kernel/tests/pager] failed to preflight pager-fault process resources: {:?}",
                err
            );
            return;
        }

        let pager_process = Scheduler::kernel_process();
        let source_vmo = Vmo::new(
            "pager-source",
            PAGE_SIZE,
            PAGE_SIZE,
            VmFlags::READ | VmFlags::WRITE | VmFlags::MAP,
        );
        let allocator = RuntimeServices::global().frame_allocator();
        if source_vmo
            .commit_range::<Machine>(0, PAGE_SIZE, VmoPagePurpose::Anonymous, allocator)
            .is_err()
        {
            warn!("[kernel/tests/pager] failed to commit pager source vmo");
            return;
        }
        let source_bytes = [0x5Au8; 64];
        if !source_vmo.write(0, &source_bytes) {
            warn!("[kernel/tests/pager] failed to seed pager source vmo");
            return;
        }
        let source_handle = match pager_process.create_anonymous_object(
            Payload::new(source_vmo.clone()),
            Capability::READ | Capability::WRITE,
            VMO_DEFAULT_INTERFACE_CAPS,
        ) {
            Ok(handle) => handle,
            Err(err) => {
                warn!(
                    "[kernel/tests/pager] failed to publish pager source handle: {:?}",
                    err
                );
                return;
            }
        };
        let source_slot = pager_process.install_handle_auto(source_handle);

        let pager = PagerObject::new(Arc::clone(&pager_process), SERVICE_IP);
        let target_vmo = Vmo::new(
            "pager-target",
            PAGE_SIZE,
            PAGE_SIZE,
            VmFlags::READ | VmFlags::WRITE | VmFlags::USER | VmFlags::MAP,
        );
        if let Err(err) = pager.bind_vmo(&target_vmo, PAGER_COOKIE) {
            warn!(
                "[kernel/tests/pager] failed to bind pager-backed vmo: {:?}",
                err
            );
            return;
        }

        let mapped_slot = match process.derive_segment_vmar_handle(VmLayoutSegment::UserMapped) {
            Ok(handle) => process.install_handle_auto(handle),
            Err(err) => {
                warn!(
                    "[kernel/tests/pager] failed to derive UserMapped handle for pager-fault \
                     probe: {:?}",
                    err
                );
                return;
            }
        };
        let target_handle = match process.create_anonymous_object(
            Payload::new(target_vmo.clone()),
            Capability::READ | Capability::WRITE,
            VMO_DEFAULT_INTERFACE_CAPS,
        ) {
            Ok(handle) => handle,
            Err(err) => {
                warn!(
                    "[kernel/tests/pager] failed to create target vmo handle: {:?}",
                    err
                );
                return;
            }
        };
        let target_slot = process.install_handle_auto(target_handle);
        let (child_vmar, range) = match process.allocate_child_vmar_any(mapped_slot, PAGE_SIZE) {
            Ok(state) => state,
            Err(err) => {
                warn!(
                    "[kernel/tests/pager] failed to allocate child VMAR for pager-fault probe: \
                     {:?}",
                    err
                );
                return;
            }
        };
        let child_slot = process.install_handle_auto(child_vmar);
        if let Err(err) = process.vmar_map(
            child_slot,
            target_slot,
            range,
            0,
            VmFlags::READ | VmFlags::WRITE | VmFlags::USER,
            RegionPurpose::User,
        ) {
            warn!(
                "[kernel/tests/pager] failed to map pager-backed target vmo into probe process: \
                 {:?}",
                err
            );
            return;
        }

        let server_ready = Arc::new(AtomicBool::new(false));
        let server_done = Arc::new(AtomicBool::new(false));
        let server_ready_flag = Arc::clone(&server_ready);
        let server_done_flag = Arc::clone(&server_done);
        let server_pager = pager.clone();
        let server_cpu = Self::TEARDOWN_CPU.min(PerCpu::count().saturating_sub(1));
        if let Err(err) = Scheduler::spawn_on(server_cpu, &pager_process, async move {
            let Some(task) = Scheduler::current_task_ref() else {
                return TaskExit::ThreadExited(0x6210);
            };
            server_ready_flag.store(true, Ordering::Release);
            if server_pager.wait(Arc::clone(&task)).await.is_err() {
                return TaskExit::ThreadExited(0x6211);
            }
            let Some(delivery) = task.take_service_delivery() else {
                return TaskExit::ThreadExited(0x6212);
            };
            let expected_request = ServiceFrame::pager_fault_request(
                range.start().as_usize(),
                (MMUFlags::READ | MMUFlags::USER).bits() as usize,
                PAGER_COOKIE,
                0,
                0,
            );
            if delivery.usr_ip != SERVICE_IP || delivery.frame != expected_request {
                return TaskExit::ThreadExited(0x6213);
            }
            let reply = libakarin_syscall::SyscallResult::new(
                SYSCALL_STATUS_OK,
                [source_slot as usize, 0, 0, 0, 0],
            );
            if dispatcher().complete_current_call(&task, reply).is_err() {
                return TaskExit::ThreadExited(0x6214);
            }
            server_done_flag.store(true, Ordering::Release);
            TaskExit::Completed
        }) {
            warn!(
                "[kernel/tests/pager] failed to spawn pager-fault probe server: {:?}",
                err
            );
            return;
        }

        let cpu_count = PerCpu::count();
        let ready_deadline = Self::cpu0_test_ticks().saturating_add(FUTEX_WAIT_WAKE_TIMEOUT_TICKS);
        while !server_ready.load(Ordering::Acquire) && Self::cpu0_test_ticks() < ready_deadline {
            Self::progress_system(cpu_count).await;
        }
        if !server_ready.load(Ordering::Acquire) {
            warn!("[kernel/tests/pager] pager-fault probe server did not reach wait state");
            return;
        }

        let fault_addr = range.start();
        let resolution = match process
            .resolve_user_fault(fault_addr, MMUFlags::READ | MMUFlags::USER)
        {
            Ok(resolution) => resolution,
            Err(err) => {
                warn!(
                    "[kernel/tests/pager] resolve_user_fault failed for pager-fault probe: {:?}",
                    err
                );
                return;
            }
        };
        let ProcessUserFaultResolution::Block(detail) = resolution else {
            warn!(
                "[kernel/tests/pager] expected pager-backed fault to block, got {:?}",
                resolution
            );
            return;
        };
        if !process.service_pager_fault(&detail).await {
            warn!("[kernel/tests/pager] pager-fault probe failed to service blocked fault");
            return;
        }
        if !server_done.load(Ordering::Acquire) {
            warn!("[kernel/tests/pager] pager-fault probe server did not finish reply");
            return;
        }
        let mut copied = [0u8; 64];
        if !target_vmo.read(0, &mut copied) || copied != source_bytes {
            warn!("[kernel/tests/pager] pager-fault probe copied unexpected data");
            return;
        }
        match process.resolve_user_fault(fault_addr, MMUFlags::READ | MMUFlags::USER) {
            Ok(ProcessUserFaultResolution::Resume) => {}
            Ok(other) => warn!(
                "[kernel/tests/pager] pager-fault probe did not resume after fill: {:?}",
                other
            ),
            Err(err) => warn!(
                "[kernel/tests/pager] pager-fault probe post-fill resolve failed: {:?}",
                err
            ),
        }
    }

    async fn run_syscall_handler_probes() {
        const SERVICE_IP: usize = 0x0040_3000;
        const SERVER_STARTED: usize = 0x50;
        const SERVER_WAITING: usize = 0x51;
        const SERVER_WAIT_ERR: usize = 0x52;
        const SERVER_NO_DELIVERY: usize = 0x53;
        const SERVER_BAD_DELIVERY: usize = 0x54;
        const SERVER_REPLY_ERR: usize = 0x55;
        const SERVER_DONE_STATE: usize = 0x56;
        const CALLER_STARTED: usize = 0x50;
        const CALLER_INVOKING: usize = 0x51;
        const CALLER_OK: usize = 0x52;
        const CALLER_BAD_REPLY: usize = 0x53;
        const CALLER_INVOKE_ERROR: usize = 0x54;

        let process = Scheduler::kernel_process();
        let handler =
            SyscallHandlerObject::new(Arc::clone(&process), SERVICE_IP, Syscall::HandleUpgrade);
        if let Err(err) = crate::service::syscall_handler_runtime().bind_handler(handler.clone()) {
            warn!(
                "[kernel/tests/syscall-handler] failed to register userspace syscall handler: {:?}",
                err
            );
            return;
        }

        let expected_request =
            SyscallArgs::from_syscall(Syscall::HandleUpgrade, [0x51, 0x52, 0x53, 0x54, 0x55]);
        let expected_reply = libakarin_syscall::SyscallResult::new(
            SYSCALL_STATUS_OK,
            [
                0xDA11_0001,
                0xDA11_0002,
                0xDA11_0003,
                0xDA11_0004,
                0xDA11_0005,
            ],
        );
        let server_ready = Arc::new(AtomicBool::new(false));
        let server_done = Arc::new(AtomicBool::new(false));
        let server_state = Arc::new(AtomicUsize::new(0));
        let caller_state = Arc::new(AtomicUsize::new(0));
        let server_ready_flag = Arc::clone(&server_ready);
        let server_done_flag = Arc::clone(&server_done);
        let server_state_flag = Arc::clone(&server_state);
        let caller_state_flag = Arc::clone(&caller_state);
        let server_handler = handler.clone();
        let server_cpu = Self::TEARDOWN_CPU.min(PerCpu::count().saturating_sub(1));
        if let Err(err) = Scheduler::spawn_on(server_cpu, &process, async move {
            let Some(task) = Scheduler::current_task_ref() else {
                return TaskExit::ThreadExited(0x6300);
            };
            server_state_flag.store(SERVER_STARTED, Ordering::Release);
            server_ready_flag.store(true, Ordering::Release);
            server_state_flag.store(SERVER_WAITING, Ordering::Release);
            if server_handler.wait(Arc::clone(&task)).await.is_err() {
                server_state_flag.store(SERVER_WAIT_ERR, Ordering::Release);
                return TaskExit::ThreadExited(0x6301);
            }
            let Some(delivery) = task.take_service_delivery() else {
                server_state_flag.store(SERVER_NO_DELIVERY, Ordering::Release);
                return TaskExit::ThreadExited(0x6302);
            };
            let expected_frame =
                ServiceFrame::syscall_request(expected_request.method_id, *expected_request.args());
            if delivery.usr_ip != SERVICE_IP || delivery.frame != expected_frame {
                server_state_flag.store(SERVER_BAD_DELIVERY, Ordering::Release);
                return TaskExit::ThreadExited(0x6303);
            }
            if dispatcher()
                .complete_current_call(&task, expected_reply)
                .is_err()
            {
                server_state_flag.store(SERVER_REPLY_ERR, Ordering::Release);
                return TaskExit::ThreadExited(0x6304);
            }
            server_state_flag.store(SERVER_DONE_STATE, Ordering::Release);
            server_done_flag.store(true, Ordering::Release);
            TaskExit::Completed
        }) {
            warn!(
                "[kernel/tests/syscall-handler] failed to spawn server task: {:?}",
                err
            );
            return;
        }

        let cpu_count = PerCpu::count();
        let ready_deadline = Self::cpu0_test_ticks().saturating_add(FUTEX_WAIT_WAKE_TIMEOUT_TICKS);
        while !server_ready.load(Ordering::Acquire) && Self::cpu0_test_ticks() < ready_deadline {
            Self::progress_system(cpu_count).await;
        }
        if !server_ready.load(Ordering::Acquire) {
            warn!("[kernel/tests/syscall-handler] server did not reach wait state");
            return;
        }

        let caller_task_id = match Scheduler::spawn_on(server_cpu, &process, async move {
            caller_state_flag.store(CALLER_STARTED, Ordering::Release);
            caller_state_flag.store(CALLER_INVOKING, Ordering::Release);
            let reply = syscall::dispatch(expected_request).await;
            if reply == expected_reply {
                caller_state_flag.store(CALLER_OK, Ordering::Release);
                return TaskExit::Completed;
            }
            if reply.is_ok() {
                caller_state_flag.store(CALLER_BAD_REPLY, Ordering::Release);
                return TaskExit::ThreadExited(0x6305);
            }
            caller_state_flag.store(CALLER_INVOKE_ERROR, Ordering::Release);
            TaskExit::ThreadExited(0x6306)
        }) {
            Ok(task_id) => task_id,
            Err(err) => {
                warn!(
                    "[kernel/tests/syscall-handler] failed to spawn caller task: {:?}",
                    err
                );
                return;
            }
        };

        let done_deadline = Self::cpu0_test_ticks().saturating_add(FUTEX_WAIT_WAKE_TIMEOUT_TICKS);
        while Self::cpu0_test_ticks() < done_deadline {
            let caller_done =
                syscall_handler_probe_caller_done(caller_state.load(Ordering::Acquire));
            if caller_done && server_done.load(Ordering::Acquire) {
                break;
            }
            Self::progress_system(cpu_count).await;
        }
        if caller_state.load(Ordering::Acquire) != CALLER_OK {
            let mut caller_snapshot = None;
            for cpu_id in 0..PerCpu::count() {
                let Some(task) = Scheduler::task_on_cpu(cpu_id, caller_task_id) else {
                    continue;
                };
                caller_snapshot =
                    Some((cpu_id, task.state(), task.sched_meta(), task.has_runnable()));
                break;
            }
            warn!(
                "[kernel/tests/syscall-handler] caller did not complete successfully: state={:#x} \
                 server_state={:#x} server_done={} caller_task={:?}",
                caller_state.load(Ordering::Acquire),
                server_state.load(Ordering::Acquire),
                server_done.load(Ordering::Acquire),
                caller_snapshot
            );
            return;
        }
        if !server_done.load(Ordering::Acquire) {
            warn!("[kernel/tests/syscall-handler] server did not finish reply path");
        }
    }

    async fn run_futex_wait_wake_probe(cpu_count: usize) {
        let (process, futex_addr) = match Self::create_futex_probe_process("futex-wait-wake", 1) {
            Ok(state) => state,
            Err(err) => {
                warn!("[kernel/tests/futex] wait-wake setup failed: {:?}", err);
                return;
            }
        };
        let pid = process.pid();

        let waiter_state = Arc::new(AtomicUsize::new(0));
        let waker_state = Arc::new(AtomicUsize::new(0));

        let waiter_probe = Arc::clone(&waiter_state);
        let waiter_task = Scheduler::spawn_on(Self::TEARDOWN_CPU, &process, async move {
            Scheduler::allow_current_migration(false);
            let process = match Scheduler::current_process() {
                Ok(process) => process,
                Err(err) => {
                    waiter_probe.store(0x1000 + err.abi_code(), Ordering::Release);
                    return TaskExit::ThreadExited(0x3000);
                }
            };
            waiter_probe.store(1, Ordering::Release);
            match RuntimeServices::global()
                .futex()
                .wait(&process, futex_addr, 1, usize::MAX)
                .await
            {
                Ok(()) => waiter_probe.store(2, Ordering::Release),
                Err(err) => waiter_probe.store(0x1100 + err as usize, Ordering::Release),
            }
            TaskExit::ThreadExited(0x3001)
        });
        if let Err(err) = waiter_task {
            warn!(
                "[kernel/tests/futex] wait-wake process {} failed to spawn waiter: {:?}",
                pid, err
            );
            return;
        }

        let waker_probe = Arc::clone(&waker_state);
        let waiter_probe = Arc::clone(&waiter_state);
        let waker_task = Scheduler::spawn_on(Self::TEARDOWN_CPU, &process, async move {
            Scheduler::allow_current_migration(false);
            let process = match Scheduler::current_process() {
                Ok(process) => process,
                Err(err) => {
                    waker_probe.store(0x1200 + err.abi_code(), Ordering::Release);
                    return TaskExit::ThreadExited(0x3002);
                }
            };

            for attempt in 0..256usize {
                if waiter_probe.load(Ordering::Acquire) != 1 {
                    Scheduler::yield_now().await;
                    continue;
                }
                match RuntimeServices::global()
                    .futex()
                    .wake(&process, futex_addr, 1)
                {
                    Ok(1) => {
                        waker_probe.store(1, Ordering::Release);
                        return TaskExit::ThreadExited(0x3003);
                    }
                    Ok(0) => Scheduler::yield_now().await,
                    Ok(other) => {
                        waker_probe.store(0x1300 + other, Ordering::Release);
                        return TaskExit::ThreadExited(0x3004);
                    }
                    Err(err) => {
                        waker_probe.store(
                            0x1400 + err as usize + attempt.saturating_mul(0x10),
                            Ordering::Release,
                        );
                        return TaskExit::ThreadExited(0x3005);
                    }
                }
            }

            waker_probe.store(0x1500, Ordering::Release);
            TaskExit::ThreadExited(0x3006)
        });
        if let Err(err) = waker_task {
            warn!(
                "[kernel/tests/futex] wait-wake process {} failed to spawn waker: {:?}",
                pid, err
            );
            return;
        }

        let deadline = Self::cpu0_test_ticks().saturating_add(FUTEX_WAIT_WAKE_TIMEOUT_TICKS);
        while Self::cpu0_test_ticks() < deadline {
            if waiter_state.load(Ordering::Acquire) == 2 && waker_state.load(Ordering::Acquire) == 1
            {
                break;
            }
            Self::progress_system(cpu_count).await;
        }

        let waiter_result = waiter_state.load(Ordering::Acquire);
        let waker_result = waker_state.load(Ordering::Acquire);
        if waiter_result != 2 || waker_result != 1 {
            warn!(
                "[kernel/tests/futex] wait-wake process {} failed: waiter_state={:#x} \
                 waker_state={:#x}",
                pid, waiter_result, waker_result
            );
        }

        if !Self::wait_for_process_task_count(&process, 0, cpu_count, FUTEX_WAIT_WAKE_TIMEOUT_TICKS)
            .await
        {
            let task_count = process.task_count();
            warn!(
                "[kernel/tests/futex] wait-wake process {} teardown timed out, remaining_tasks={}",
                pid, task_count
            );
        }
    }

    async fn run_futex_timeout_probe(cpu_count: usize) {
        let (process, futex_addr) = match Self::create_futex_probe_process("futex-timeout", 1) {
            Ok(state) => state,
            Err(err) => {
                warn!("[kernel/tests/futex] timeout setup failed: {:?}", err);
                return;
            }
        };
        let pid = process.pid();

        let timeout_state = Arc::new(AtomicUsize::new(0));
        let timeout_probe = Arc::clone(&timeout_state);
        let timeout_ns = FUTEX_TIMEOUT_PROBE_WAIT.as_nanos().min(usize::MAX as u128) as usize;
        let spawn_result = Scheduler::spawn_on(Self::TEARDOWN_CPU, &process, async move {
            Scheduler::allow_current_migration(false);
            let process = match Scheduler::current_process() {
                Ok(process) => process,
                Err(err) => {
                    timeout_probe.store(0x1600 + err.abi_code(), Ordering::Release);
                    return TaskExit::ThreadExited(0x3007);
                }
            };

            match RuntimeServices::global()
                .futex()
                .wait(&process, futex_addr, 1, timeout_ns)
                .await
            {
                Err(FutexError::TimedOut) => timeout_probe.store(1, Ordering::Release),
                Err(err) => timeout_probe.store(0x1700 + err as usize, Ordering::Release),
                Ok(()) => timeout_probe.store(0x1800, Ordering::Release),
            }
            TaskExit::ThreadExited(0x3008)
        });
        if let Err(err) = spawn_result {
            warn!(
                "[kernel/tests/futex] timeout process {} failed to spawn waiter: {:?}",
                pid, err
            );
            return;
        }

        let deadline = Self::cpu0_test_ticks().saturating_add(FUTEX_TIMEOUT_PROBE_TIMEOUT_TICKS);
        while Self::cpu0_test_ticks() < deadline {
            if timeout_state.load(Ordering::Acquire) == 1 {
                break;
            }
            Self::progress_system(cpu_count).await;
        }

        let timeout_result = timeout_state.load(Ordering::Acquire);
        if timeout_result != 1 {
            warn!(
                "[kernel/tests/futex] timeout process {} failed: state={:#x} wait={}ms",
                pid,
                timeout_result,
                FUTEX_TIMEOUT_PROBE_WAIT.as_millis()
            );
        }

        if !Self::wait_for_process_task_count(
            &process,
            0,
            cpu_count,
            FUTEX_TIMEOUT_PROBE_TIMEOUT_TICKS,
        )
        .await
        {
            let task_count = process.task_count();
            warn!(
                "[kernel/tests/futex] timeout process {} teardown timed out, remaining_tasks={}",
                pid, task_count
            );
        }
    }

    async fn run_teardown_probes() {
        let cpu_count = PerCpu::count();
        Scheduler::allow_current_migration(false);
        for round in 0..TEARDOWN_STRESS_ROUNDS {
            Self::run_process_exit_teardown(round, cpu_count).await;
            Self::settle_system(cpu_count).await;
            Self::run_fault_cancel_teardown(round, cpu_count).await;
            Self::settle_system(cpu_count).await;
        }
    }

    async fn run_spawn_race_probes() {
        let cpu_count = PerCpu::count();
        let target_cpu = if cpu_count > 1 { 1 } else { 0 };
        let mut spawn_failures = 0usize;
        let mut teardown_failures = 0usize;

        for round in 0..SPAWN_RACE_STRESS_ROUNDS {
            let process = match RuntimeServices::global()
                .namespaces()
                .scheduler_manager()
                .create_process(
                    &format!("spawn-race-{}", round),
                    Self::test_process_root_range(),
                ) {
                Ok(process) => process,
                Err(err) => {
                    spawn_failures += 1;
                    warn!(
                        "[kernel/tests/spawn-race] round={} failed to create process: {:?}",
                        round, err
                    );
                    continue;
                }
            };
            let pid = process.pid();

            let spawn_result = Scheduler::spawn_on(target_cpu, &process, async move {
                Scheduler::allow_current_migration(false);
                TaskExit::ProcessExited(0x2000 + round)
            });
            let task_id = match spawn_result {
                Ok(task_id) => task_id,
                Err(err) => {
                    spawn_failures += 1;
                    warn!(
                        "[kernel/tests/spawn-race] round={} process {} spawn failed: {:?}",
                        round, pid, err
                    );
                    continue;
                }
            };

            if !Self::wait_for_process_task_count(&process, 0, cpu_count, SPAWN_RACE_TIMEOUT_TICKS)
                .await
            {
                teardown_failures += 1;
                let task_count = process.task_count();
                warn!(
                    "[kernel/tests/spawn-race] round={} process {} task {} teardown timed out, \
                     remaining_tasks={}",
                    round, pid, task_id, task_count
                );
                Self::log_scheduler_diagnostics("spawn-race-timeout");
            }
        }

        if spawn_failures != 0 || teardown_failures != 0 {
            warn!(
                "[kernel/tests/spawn-race] failures: spawn={} teardown={}",
                spawn_failures, teardown_failures
            );
        }
    }

    async fn run_process_exit_teardown(round: usize, cpu_count: usize) {
        let process = match RuntimeServices::global()
            .namespaces()
            .scheduler_manager()
            .create_process(
                &format!("teardown-exit-{}", round),
                Self::test_process_root_range(),
            ) {
            Ok(process) => process,
            Err(err) => {
                warn!(
                    "[kernel/tests/teardown] round={} failed to create exit process: {:?}",
                    round, err
                );
                return;
            }
        };
        if let Err(err) = Self::preflight_process_resources(&process) {
            warn!(
                "[kernel/tests/teardown] round={} failed to preflight exit process resources: {:?}",
                round, err
            );
            return;
        }
        let pid = process.pid();

        let start = Arc::new(AtomicBool::new(false));
        let mut task_ids = Vec::new();

        for sibling in 0..TEARDOWN_SIBLING_TASKS {
            let start = Arc::clone(&start);
            match Scheduler::spawn_on(Self::TEARDOWN_CPU, &process, async move {
                Scheduler::allow_current_migration(false);
                while !start.load(Ordering::Acquire) {
                    Scheduler::yield_now().await;
                }
                loop {
                    Scheduler::yield_now().await;
                }
            }) {
                Ok(task_id) => task_ids.push(task_id),
                Err(err) => warn!(
                    "[kernel/tests/teardown] round={} failed to spawn exit sibling {}: {:?}",
                    round, sibling, err
                ),
            }
        }

        let start_exit = Arc::clone(&start);
        match Scheduler::spawn_on(Self::TEARDOWN_CPU, &process, async move {
            Scheduler::allow_current_migration(false);
            while !start_exit.load(Ordering::Acquire) {
                Scheduler::yield_now().await;
            }
            Scheduler::yield_now().await;
            TaskExit::ProcessExited(0x1000 + round)
        }) {
            Ok(task_id) => task_ids.push(task_id),
            Err(err) => warn!(
                "[kernel/tests/teardown] round={} failed to spawn exit task: {:?}",
                round, err
            ),
        }

        for _ in 0..(cpu_count.max(1) * 8) {
            Self::progress_system(cpu_count).await;
        }

        let expected_tasks = TEARDOWN_SIBLING_TASKS + 1;
        match process.task_count() {
            task_count if task_count != expected_tasks => warn!(
                "[kernel/tests/teardown] round={} process {} registered {} tasks, expected {}",
                round, pid, task_count, expected_tasks
            ),
            _ => {}
        }

        start.store(true, Ordering::Release);
        if !Self::wait_for_process_task_count(&process, 0, cpu_count, TEARDOWN_TIMEOUT_TICKS).await
        {
            let task_count = process.task_count();
            warn!(
                "[kernel/tests/teardown] round={} process {} exit teardown timed out, \
                 remaining_tasks={}",
                round, pid, task_count
            );
            Self::log_scheduler_diagnostics("process-exit-timeout");
        }

        for task_id in task_ids {
            if process.owns_task_id(task_id) {
                warn!(
                    "[kernel/tests/teardown] round={} process {} still owns exited task {}",
                    round, pid, task_id
                );
            }
        }
    }

    async fn run_fault_cancel_teardown(round: usize, cpu_count: usize) {
        let process = match RuntimeServices::global()
            .namespaces()
            .scheduler_manager()
            .create_process(
                &format!("teardown-fault-{}", round),
                Self::test_process_root_range(),
            ) {
            Ok(process) => process,
            Err(err) => {
                warn!(
                    "[kernel/tests/teardown] round={} failed to create fault process: {:?}",
                    round, err
                );
                return;
            }
        };
        if let Err(err) = Self::preflight_process_resources(&process) {
            warn!(
                "[kernel/tests/teardown] round={} failed to preflight fault process resources: \
                 {:?}",
                round, err
            );
            return;
        }
        let pid = process.pid();

        let sibling_id = match Scheduler::spawn_on(Self::TEARDOWN_CPU, &process, async move {
            Scheduler::allow_current_migration(false);
            loop {
                Scheduler::yield_now().await;
            }
        }) {
            Ok(task_id) => task_id,
            Err(err) => {
                warn!(
                    "[kernel/tests/teardown] round={} failed to spawn fault sibling: {:?}",
                    round, err
                );
                return;
            }
        };

        let user_stack = match allocate_user_stack_for_process(&process, TEARDOWN_USER_STACK_PAGES)
        {
            Ok(stack) => stack,
            Err(err) => {
                warn!(
                    "[kernel/tests/teardown] round={} failed to allocate user stack: {:?}",
                    round, err
                );
                return;
            }
        };

        let mut user_ctx = arch::TrapContext::new_user();
        user_ctx.set_instruction_pointer(0);
        user_ctx.set_stack_pointer(user_stack.top());

        let fault_task = match Scheduler::spawn_user_on(Self::TEARDOWN_CPU, &process, user_ctx) {
            Ok(task_id) => task_id,
            Err(err) => {
                warn!(
                    "[kernel/tests/teardown] round={} failed to spawn fault task: {:?}",
                    round, err
                );
                return;
            }
        };

        if !Self::wait_for_process_task_count(&process, 1, cpu_count, TEARDOWN_TIMEOUT_TICKS).await
        {
            let task_count = process.task_count();
            warn!(
                "[kernel/tests/teardown] round={} process {} fault teardown timed out waiting for \
                 sibling-only state, remaining_tasks={}",
                round, pid, task_count
            );
            Self::log_scheduler_diagnostics("fault-teardown-timeout");
        }

        if process.owns_task_id(fault_task) {
            warn!(
                "[kernel/tests/teardown] round={} process {} still owns faulted task {}",
                round, pid, fault_task
            );
        }
        if !process.owns_task_id(sibling_id) {
            warn!(
                "[kernel/tests/teardown] round={} process {} lost sibling task {} before explicit \
                 cancel",
                round, pid, sibling_id
            );
        }

        if let Err(err) = Scheduler::cancel_task(sibling_id) {
            warn!(
                "[kernel/tests/teardown] round={} failed to cancel sibling task {}: {:?}",
                round, sibling_id, err
            );
        }
        if !Self::wait_for_process_task_count(&process, 0, cpu_count, TEARDOWN_TIMEOUT_TICKS).await
        {
            let task_count = process.task_count();
            warn!(
                "[kernel/tests/teardown] round={} process {} cancel teardown timed out, \
                 remaining_tasks={}",
                round, pid, task_count
            );
            Self::log_scheduler_diagnostics("cancel-teardown-timeout");
        }

        if let Err(err) = release_user_stack_for_process(&process, user_stack) {
            warn!(
                "[kernel/tests/teardown] round={} failed to release fault user stack: {:?}",
                round, err
            );
        }

        let recycled_stack =
            match allocate_user_stack_for_process(&process, TEARDOWN_USER_STACK_PAGES) {
                Ok(stack) => stack,
                Err(err) => {
                    warn!(
                        "[kernel/tests/teardown] round={} failed to reallocate user stack after \
                         fault teardown: {:?}",
                        round, err
                    );
                    return;
                }
            };
        if let Err(err) = release_user_stack_for_process(&process, recycled_stack) {
            warn!(
                "[kernel/tests/teardown] round={} failed to release recycled user stack: {:?}",
                round, err
            );
        }
    }

    async fn settle_system(cpu_count: usize) {
        for _ in 0..256 {
            let mut aps_idle = true;
            for cpu_id in 1..cpu_count {
                if Scheduler::cpu_has_work(cpu_id) {
                    aps_idle = false;
                    break;
                }
            }
            if aps_idle {
                break;
            }
            Self::progress_system(cpu_count).await;
        }
    }

    async fn run_equal_cpu_bound_fairness(cpu_count: usize) {
        let task_count = Self::worker_cpu_count(cpu_count) * FAIRNESS_TASKS_PER_CPU;
        let start = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicUsize::new(0));
        let probes: Vec<_> = (0..task_count)
            .map(|_| Arc::new(FairnessProbe::new()))
            .collect();
        let fairness_epoch = Self::arm_fairness_window(FAIRNESS_WINDOW_TICKS);

        for (task_index, probe) in probes.iter().enumerate() {
            let start = Arc::clone(&start);
            let done = Arc::clone(&done);
            let probe = Arc::clone(probe);
            let target_cpu = Self::worker_cpu(task_index, cpu_count);
            match Scheduler::spawn_on(target_cpu, Scheduler::kernel_process(), async move {
                Scheduler::allow_current_migration(false);
                while !start.load(Ordering::Acquire) {
                    Scheduler::yield_now().await;
                }
                let mut local_progress = 0u64;
                while !Self::fairness_window_closed(fairness_epoch) {
                    local_progress += 1;
                    if local_progress >= Self::CPU_BOUND_BATCH {
                        probe.record_progress_batch(PerCpu::id(), local_progress);
                        local_progress = 0;
                    }
                    core::hint::spin_loop();
                }
                if local_progress != 0 {
                    probe.record_progress_batch(PerCpu::id(), local_progress);
                }
                done.fetch_add(1, Ordering::AcqRel);
                TaskExit::Completed
            }) {
                Ok(task_id) => {
                    let _ = task_id;
                }
                Err(err) => warn!(
                    "[kernel/tests] failed to spawn equal fairness task {}: {:?}",
                    task_index, err
                ),
            }
        }

        for _ in 0..(cpu_count.max(1) * 16) {
            Self::progress_system(cpu_count).await;
        }
        start.store(true, Ordering::Release);
        log::info!(
            "[kernel/tests/fairness] equal-cpu-bound window start cpu0_tick={}",
            Self::cpu0_test_ticks()
        );

        while !Self::fairness_window_closed(fairness_epoch) {
            Self::progress_system(cpu_count).await;
        }
        info!(
            "[kernel/tests/fairness] equal-cpu-bound window closed cpu0_tick={}",
            Self::cpu0_test_ticks()
        );

        let deadline = Self::cpu0_test_ticks().saturating_add(FAIRNESS_COMPLETION_TIMEOUT_TICKS);
        while Self::cpu0_test_ticks() < deadline {
            if done.load(Ordering::Acquire) == task_count {
                break;
            }
            Self::progress_system(cpu_count).await;
        }
        if done.load(Ordering::Acquire) != task_count {
            warn!(
                "[kernel/tests/fairness] equal-cpu-bound completion lagged: done={} tasks={}",
                done.load(Ordering::Acquire),
                task_count
            );
            Self::log_scheduler_diagnostics("equal-cpu-bound-completion");
        }

        Self::report_fairness("equal-cpu-bound", &probes);
    }

    async fn run_mixed_sleep_fairness(cpu_count: usize) {
        let cpu_bound_count = Self::worker_cpu_count(cpu_count) * FAIRNESS_MIXED_TASKS_PER_CPU;
        let sleeper_count = cpu_bound_count;
        let task_count = cpu_bound_count + sleeper_count;
        let start = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicUsize::new(0));
        let probes: Vec<_> = (0..task_count)
            .map(|_| Arc::new(FairnessProbe::new()))
            .collect();
        let fairness_epoch = Self::arm_fairness_window(FAIRNESS_WINDOW_TICKS);

        for (task_index, probe) in probes.iter().take(cpu_bound_count).enumerate() {
            let start = Arc::clone(&start);
            let done = Arc::clone(&done);
            let probe = Arc::clone(probe);
            let target_cpu = Self::worker_cpu(task_index, cpu_count);
            match Scheduler::spawn_on(target_cpu, Scheduler::kernel_process(), async move {
                Scheduler::allow_current_migration(false);
                while !start.load(Ordering::Acquire) {
                    Scheduler::yield_now().await;
                }
                let mut local_progress = 0u64;
                while !Self::fairness_window_closed(fairness_epoch) {
                    local_progress += 1;
                    if local_progress >= Self::CPU_BOUND_BATCH {
                        probe.record_progress_batch(PerCpu::id(), local_progress);
                        local_progress = 0;
                    }
                    core::hint::spin_loop();
                }
                if local_progress != 0 {
                    probe.record_progress_batch(PerCpu::id(), local_progress);
                }
                done.fetch_add(1, Ordering::AcqRel);
                TaskExit::Completed
            }) {
                Ok(task_id) => {
                    let _ = task_id;
                }
                Err(err) => warn!(
                    "[kernel/tests] failed to spawn mixed cpu-bound task {}: {:?}",
                    task_index, err
                ),
            }
        }

        for (task_index, probe) in probes.iter().skip(cpu_bound_count).enumerate() {
            let start = Arc::clone(&start);
            let done = Arc::clone(&done);
            let probe = Arc::clone(probe);
            let target_cpu = Self::worker_cpu(task_index, cpu_count);
            match Scheduler::spawn_on(target_cpu, Scheduler::kernel_process(), async move {
                Scheduler::allow_current_migration(false);
                while !start.load(Ordering::Acquire) {
                    Scheduler::yield_now().await;
                }
                while !Self::fairness_window_closed(fairness_epoch) {
                    probe.record_progress(PerCpu::id());
                    Scheduler::sleep(Duration::from_millis(1)).await;
                }
                done.fetch_add(1, Ordering::AcqRel);
                TaskExit::Completed
            }) {
                Ok(task_id) => {
                    let _ = task_id;
                }
                Err(err) => warn!(
                    "[kernel/tests] failed to spawn mixed sleeper task {}: {:?}",
                    task_index, err
                ),
            }
        }

        for _ in 0..(cpu_count.max(1) * 16) {
            Self::progress_system(cpu_count).await;
        }
        start.store(true, Ordering::Release);
        log::info!(
            "[kernel/tests/fairness] mixed window start cpu0_tick={}",
            Self::cpu0_test_ticks()
        );

        while !Self::fairness_window_closed(fairness_epoch) {
            Self::progress_system(cpu_count).await;
        }
        info!(
            "[kernel/tests/fairness] mixed window closed cpu0_tick={}",
            Self::cpu0_test_ticks()
        );

        let deadline = Self::cpu0_test_ticks().saturating_add(FAIRNESS_COMPLETION_TIMEOUT_TICKS);
        while Self::cpu0_test_ticks() < deadline {
            if done.load(Ordering::Acquire) == task_count {
                break;
            }
            Self::progress_system(cpu_count).await;
        }
        if done.load(Ordering::Acquire) != task_count {
            warn!(
                "[kernel/tests/fairness] mixed completion lagged: done={} tasks={}",
                done.load(Ordering::Acquire),
                task_count
            );
            Self::log_scheduler_diagnostics("mixed-completion");
        }

        Self::report_fairness("mixed-cpu-bound", &probes[..cpu_bound_count]);
        Self::report_fairness("mixed-sleeper", &probes[cpu_bound_count..]);
    }

    fn report_fairness(label: &str, probes: &[Arc<FairnessProbe>]) {
        let summary = FairnessSummary::gather(probes);
        let final_cpu_counts = Self::final_cpu_counts(probes);
        let warn_spread = Self::warn_spread_permille(label);
        let level = if summary.min_progress == 0 || summary.spread_permille > warn_spread {
            "warn"
        } else {
            "info"
        };

        match level {
            "warn" => warn!(
                "[kernel/tests/fairness] {} tasks={} min={} max={} mean={} spread={}permille \
                 dev={}permille migrations={} cpus={:#x} final_cpus={:?}",
                label,
                summary.task_count,
                summary.min_progress,
                summary.max_progress,
                summary.mean_progress,
                summary.spread_permille,
                summary.max_deviation_permille,
                summary.total_migrations,
                summary.observed_cpu_mask,
                final_cpu_counts,
            ),
            _ => info!(
                "[kernel/tests/fairness] {} tasks={} min={} max={} mean={} spread={}permille \
                 dev={}permille migrations={} cpus={:#x} final_cpus={:?}",
                label,
                summary.task_count,
                summary.min_progress,
                summary.max_progress,
                summary.mean_progress,
                summary.spread_permille,
                summary.max_deviation_permille,
                summary.total_migrations,
                summary.observed_cpu_mask,
                final_cpu_counts,
            ),
        }

        if level == "warn" {
            Self::log_scheduler_diagnostics(label);
        }
    }

    fn warn_spread_permille(label: &str) -> u64 {
        match label {
            "mixed-cpu-bound" => FAIRNESS_WARN_SPREAD_PERMILLE + 400,
            _ => FAIRNESS_WARN_SPREAD_PERMILLE,
        }
    }

    fn final_cpu_counts(probes: &[Arc<FairnessProbe>]) -> Vec<(usize, usize)> {
        let cpu_count = PerCpu::count();
        let mut counts = vec![0usize; cpu_count];
        for probe in probes {
            let Some(cpu_id) = probe.last_cpu() else {
                continue;
            };
            if let Some(count) = counts.get_mut(cpu_id) {
                *count += 1;
            }
        }

        counts
            .into_iter()
            .enumerate()
            .filter_map(|(cpu_id, count)| (count != 0).then_some((cpu_id, count)))
            .collect()
    }

    fn log_scheduler_diagnostics(label: &str) {
        let snapshots = Scheduler::debug_snapshots();
        warn!(
            "[kernel/tests/fairness] {} scheduler={:?}",
            label, snapshots
        );
    }

    async fn progress_system(_cpu_count: usize) {
        Scheduler::yield_now().await;
    }

    fn expected_cpu_mask(cpu_count: usize) -> usize {
        if cpu_count >= usize::BITS as usize {
            usize::MAX
        } else {
            (1usize << cpu_count) - 1
        }
    }

    fn mark_cpu_observed(mask: &AtomicUsize, cpu_id: usize) {
        if cpu_id < usize::BITS as usize {
            mask.fetch_or(1usize << cpu_id, Ordering::AcqRel);
        }
    }

    fn cpu0_test_ticks() -> u64 {
        Scheduler::cpu0_test_ticks()
    }

    fn worker_cpu(task_index: usize, cpu_count: usize) -> usize {
        if cpu_count <= 1 {
            0
        } else {
            1 + (task_index % Self::worker_cpu_count(cpu_count))
        }
    }

    fn worker_cpu_count(cpu_count: usize) -> usize {
        cpu_count.saturating_sub(1).max(1)
    }

    fn create_futex_probe_process(
        name: &str,
        initial_value: u32,
    ) -> Result<(Arc<Process>, usize), FutexProbeSetupError> {
        let process = RuntimeServices::global()
            .namespaces()
            .scheduler_manager()
            .create_process(name, Self::test_process_root_range())
            .map_err(|error| match error {
                ProcessVmError::Object(object) => FutexProbeSetupError::Object(object),
                ProcessVmError::Underlying(vm) => FutexProbeSetupError::Vm(vm),
            })?;
        Self::preflight_process_resources(&process).map_err(FutexProbeSetupError::Object)?;

        let futex_addr = match {
            let process = process.clone();
            (|| {
                let mapped_slot = process.install_handle_auto(
                    process.derive_segment_vmar_handle(VmLayoutSegment::UserMapped)?,
                );
                let vmo = process.create_vmo(
                    format!("{}-word", name),
                    PAGE_SIZE,
                    PAGE_SIZE,
                    VmFlags::READ | VmFlags::WRITE | VmFlags::USER | VmFlags::MAP,
                )?;
                let vmo_slot = process.install_handle_auto(vmo.acquire_ref()?);
                match process.vmo_op_range(vmo_slot, VmoOpRangeOperation::Commit, 0, PAGE_SIZE) {
                    Ok(_) => {}
                    Err(ProcessVmError::Object(error)) => {
                        warn!(
                            "[kernel/tests/futex] setup process '{}' commit object error: {:?}",
                            name, error
                        );
                        return Err(ProcessVmError::Object(error));
                    }
                    Err(ProcessVmError::Underlying(error)) => {
                        warn!(
                            "[kernel/tests/futex] setup process '{}' commit vm error: {:?}",
                            name, error
                        );
                        return Err(ProcessVmError::Underlying(error));
                    }
                }
                let initial_bytes = initial_value.to_ne_bytes();
                match vmo.write_cp_with::<Vmo, _, _>(|vmo| vmo.write_vm(0, &initial_bytes)) {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => match error.into_object_or_vm() {
                        Ok(object) => return Err(ProcessVmError::Object(object)),
                        Err(vm) => return Err(ProcessVmError::Underlying(vm)),
                    },
                    Err(error) => return Err(ProcessVmError::Object(error)),
                }
                let (child_vmar, range) =
                    match process.allocate_child_vmar_any(mapped_slot, PAGE_SIZE) {
                        Ok(state) => state,
                        Err(ProcessVmError::Object(error)) => {
                            warn!(
                                "[kernel/tests/futex] setup process '{}' allocate_child_vmar_any \
                                 object error: {:?}",
                                name, error
                            );
                            return Err(ProcessVmError::Object(error));
                        }
                        Err(ProcessVmError::Underlying(error)) => {
                            warn!(
                                "[kernel/tests/futex] setup process '{}' allocate_child_vmar_any \
                                 vm error: {:?}",
                                name, error
                            );
                            return Err(ProcessVmError::Underlying(error));
                        }
                    };
                let child_slot = process.install_handle_auto(child_vmar);
                if let Err(error) = process.vmar_map(
                    child_slot,
                    vmo_slot,
                    range,
                    0,
                    VmFlags::READ | VmFlags::WRITE | VmFlags::USER,
                    RegionPurpose::User,
                ) {
                    match error {
                        ProcessVmError::Object(object) => {
                            warn!(
                                "[kernel/tests/futex] setup process '{}' vmar_map object error: \
                                 {:?} range={:?}",
                                name, object, range
                            );
                            return Err(ProcessVmError::Object(object));
                        }
                        ProcessVmError::Underlying(vm) => {
                            warn!(
                                "[kernel/tests/futex] setup process '{}' vmar_map vm error: {:?} \
                                 range={:?}",
                                name, vm, range
                            );
                            return Err(ProcessVmError::Underlying(vm));
                        }
                    }
                }
                Ok::<usize, ProcessVmError>(range.start().as_usize())
            })()
        } {
            Ok(addr) => addr,
            Err(ProcessVmError::Object(error)) => {
                return Err(FutexProbeSetupError::Object(error));
            }
            Err(ProcessVmError::Underlying(error)) => return Err(FutexProbeSetupError::Vm(error)),
        };

        Ok((process, futex_addr))
    }

    fn preflight_process_resources(process: &Arc<Process>) -> Result<(), ObjectError> {
        let kernel_stack =
            process.allocate_kernel_stack(crate::sched::stack::TASK_KERNEL_STACK_PAGES)?;
        process.release_kernel_stack(kernel_stack)?;
        let user_stack = allocate_user_stack_for_process(process, TEARDOWN_USER_STACK_PAGES)?;
        release_user_stack_for_process(process, user_stack)?;
        Ok(())
    }

    fn test_process_root_range() -> VmRange {
        VmRange::new(
            VirtAddr::new(VmLayoutSegment::UserImage.start()),
            VirtAddr::new(
                VmLayoutSegment::UserStack
                    .end_exclusive()
                    .expect("user stack segment is bounded"),
            ),
        )
        .expect("scheduler self-test process root range must be valid")
    }

    async fn wait_for_process_task_count(
        process: &Arc<Process>,
        expected: usize,
        cpu_count: usize,
        timeout_ticks: u64,
    ) -> bool {
        let deadline = Self::cpu0_test_ticks().saturating_add(timeout_ticks);
        while Self::cpu0_test_ticks() < deadline {
            if process.task_count() == expected {
                return true;
            }
            Self::progress_system(cpu_count).await;
        }
        process.task_count() == expected
    }

    fn arm_fairness_window(window_ticks: u64) -> u64 {
        let epoch = FAIRNESS_WINDOW_NEXT_EPOCH.fetch_add(1, Ordering::AcqRel);
        FAIRNESS_WINDOW_DEADLINE_TICKS.store(
            Self::cpu0_test_ticks().saturating_add(window_ticks),
            Ordering::Release,
        );
        FAIRNESS_WINDOW_ACTIVE_EPOCH.store(epoch, Ordering::Release);
        epoch
    }

    fn fairness_window_closed(epoch: u64) -> bool {
        FAIRNESS_WINDOW_ACTIVE_EPOCH.load(Ordering::Acquire) == epoch
            && Self::cpu0_test_ticks() >= FAIRNESS_WINDOW_DEADLINE_TICKS.load(Ordering::Acquire)
    }
}
