use core::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::{Context, Poll},
};

use libakarin_macros::cpu_local;

use crate::scheduler;

cpu_local! {
    static PREEMPT_COUNT: AtomicUsize = AtomicUsize::new(0);
    static NEED_RESCHED: AtomicBool = AtomicBool::new(false);
}

/// Enter one local non-preemptible section on the current CPU.
pub fn preempt_enter_local() {
    PREEMPT_COUNT.with_current(|count: &mut AtomicUsize| {
        count.fetch_add(1, Ordering::AcqRel);
    });
}

/// Leave one local non-preemptible section and return whether the nesting
/// level reached zero.
pub fn preempt_leave_local() -> bool {
    PREEMPT_COUNT.with_current(|count: &mut AtomicUsize| {
        let previous = count.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "preempt count underflow");
        previous == 1
    })
}

/// Return the current CPU-local preemption nesting depth.
pub fn preempt_count_local() -> usize {
    PREEMPT_COUNT.with_current(|count: &mut AtomicUsize| count.load(Ordering::Acquire))
}

/// Request one local reschedule on the current CPU.
pub fn preempt_request_local() {
    NEED_RESCHED.with_current(|flag: &mut AtomicBool| {
        flag.store(true, Ordering::Release);
    });
}

/// Return whether one local reschedule is pending.
pub fn preempt_pending_local() -> bool {
    NEED_RESCHED.with_current(|flag: &mut AtomicBool| flag.load(Ordering::Acquire))
}

/// Take and clear one pending local reschedule flag.
pub fn preempt_take_local() -> bool {
    NEED_RESCHED.with_current(|flag: &mut AtomicBool| flag.swap(false, Ordering::AcqRel))
}

/// Bookkeeping guard for sections that must not be rescheduled.
///
/// This guard only tracks preemption state. Callers that also require IRQ
/// exclusion must combine it with the appropriate machine guard.
pub struct PreemptGuard {
    service_on_drop: bool,
}

impl PreemptGuard {
    /// Enter a non-preemptible section.
    pub fn enter() -> Self {
        Self::enter_with_service(true)
    }

    /// Enter a non-preemptible section without servicing the scheduler
    /// boundary on drop.
    ///
    /// This is used when the caller is already executing inside one scheduler
    /// boundary and must defer any pending reschedule until the outer boundary
    /// regains control.
    pub fn enter_deferred() -> Self {
        Self::enter_with_service(false)
    }

    fn enter_with_service(service_on_drop: bool) -> Self {
        preempt_enter_local();
        Self { service_on_drop }
    }
}

impl Drop for PreemptGuard {
    fn drop(&mut self) {
        if preempt_leave_local() && self.service_on_drop && preempt_pending_local() {
            scheduler::Scheduler::invoke_local_reschedule();
        }
    }
}

/// One kernel-side cooperative yield point.
pub struct Yield {
    yielded: bool,
}

impl Yield {
    /// Create one pending-once yield future.
    pub const fn once() -> Self {
        Self { yielded: false }
    }
}

impl Future for Yield {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.yielded {
            return Poll::Ready(());
        }

        self.yielded = true;
        scheduler::Scheduler::yield_current();
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}
