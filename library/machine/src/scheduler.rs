/// Machine-owned helpers used by the kernel scheduler.
///
/// These entry points are architecture-specific because they must obey the ISA
/// ABI and unwind/frame conventions required by the low-level context restore
/// path.
pub trait SchedulerHelper {
    /// Architecture task-runner trampoline.
    ///
    /// The scheduler installs this function as the first instruction pointer
    /// for a freshly-started kernel task. Implementations should establish a
    /// regular stack frame and tail into the kernel's per-task runner loop so
    /// stack unwinding stops cleanly at the synthetic task boundary.
    ///
    /// # Safety
    ///
    /// The caller must ensure that execution enters this trampoline on a
    /// properly prepared kernel task stack.
    unsafe extern "C" fn task_runner_trampoline() -> !;

    /// Enter the scheduler's per-CPU idle shell on one supplied stack.
    ///
    /// This is used during initial CPU handoff before the scheduler has
    /// resumed any task-owned runner frame on the local CPU. Implementations
    /// should install a clean sentinel return frame on `stack_top` and jump
    /// into `entry` without returning.
    ///
    /// # Safety
    ///
    /// The caller must ensure `stack_top` points at writable kernel stack
    /// memory dedicated to the local CPU and that `entry` never returns.
    unsafe fn enter_idle_shell(stack_top: usize, entry: extern "C" fn() -> !) -> !;

    /// Synchronously enter the local CPU's reschedule trap boundary.
    ///
    /// Implementations should trigger the same architectural path that timer
    /// or reschedule IPI handling would take on the current CPU.
    ///
    /// # Safety
    ///
    /// The caller must ensure the current continuation may be discarded after
    /// the trap boundary finalizes the running task's scheduler state.
    unsafe fn invoke_local_reschedule();
}
