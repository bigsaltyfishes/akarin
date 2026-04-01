use alloc::{collections::BTreeMap, sync::Arc};
use core::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll, Waker},
};

use libakarin_core::clock::{
    Clock,
    source::ClockSource,
    time::{Duration, Instant},
};
use libakarin_machine_core::sync::{NoOp, ScopedGuard};
use libakarin_sync::{
    collections::IdAllocator,
    spin::{Once, SpinLock, TryInitError},
};

use crate::arch::guards::IrqSaveGuard;

static TIMER: Once<Timer, ScopedGuard<NoOp>> = Once::new();

fn sleep_ids() -> &'static IdAllocator<u64> {
    static SLEEP_IDS: Once<IdAllocator<u64>, ScopedGuard<NoOp>> = Once::new();
    SLEEP_IDS.get_or_else(|| IdAllocator::new(1, 1))
}

struct SleepEntry {
    id: u64,
    deadline: Instant,
    registered: AtomicBool,
    completed: AtomicBool,
    waker: SpinLock<Option<Waker>, IrqSaveGuard>,
}

impl SleepEntry {
    fn new(deadline: Instant) -> Arc<Self> {
        Arc::new(Self {
            id: sleep_ids().allocate(),
            deadline,
            registered: AtomicBool::new(false),
            completed: AtomicBool::new(false),
            waker: SpinLock::new(None),
        })
    }

    fn key(&self) -> (Instant, u64) {
        (self.deadline, self.id)
    }

    fn register(&self) -> bool {
        !self.registered.swap(true, Ordering::AcqRel)
    }

    fn unregister(&self) -> bool {
        self.registered.swap(false, Ordering::AcqRel)
    }

    fn is_completed(&self) -> bool {
        self.completed.load(Ordering::Acquire)
    }

    fn complete(&self) -> bool {
        !self.completed.swap(true, Ordering::AcqRel)
    }

    fn install_waker(&self, waker: &Waker) {
        *self.waker.lock() = Some(waker.clone());
    }

    fn wake(&self) {
        if let Some(waker) = self.waker.lock().take() {
            waker.wake();
        }
    }
}

/// Runtime timer queue used by async task sleeps.
pub struct Timer {
    queue: SpinLock<BTreeMap<(Instant, u64), Arc<SleepEntry>>, IrqSaveGuard>,
}

impl Timer {
    const fn new() -> Self {
        Self {
            queue: SpinLock::new(BTreeMap::new()),
        }
    }

    fn current_time() -> Instant {
        crate::RuntimeServices::global()
            .namespaces()
            .clock_source_manager()
            .with_default_clock(|clock: &Clock| Ok(clock.now()))
            .expect("failed to get current time from default clock")
    }

    /// Create one async sleep future for a relative duration.
    pub fn sleep(&'static self, duration: Duration) -> Sleep {
        self.sleep_until(Self::current_time() + duration)
    }

    /// Create one async sleep future for an absolute deadline.
    pub fn sleep_until(&'static self, deadline: Instant) -> Sleep {
        Sleep {
            timer: self,
            entry: SleepEntry::new(deadline),
        }
    }

    fn register(&self, entry: Arc<SleepEntry>) {
        self.queue.lock().insert(entry.key(), entry);
    }

    fn unregister(&self, entry: &SleepEntry) {
        self.queue.lock().remove(&entry.key());
    }

    /// Advance the timer queue to `now` and wake every expired sleep.
    pub fn tick(&self, now: Instant) -> usize {
        let mut woke = 0usize;
        loop {
            let key = {
                let queue = self.queue.lock();
                let Some((key, _)) = queue.first_key_value() else {
                    break;
                };
                if key.0 > now {
                    break;
                }
                *key
            };

            let Some(entry) = self.queue.lock().remove(&key) else {
                continue;
            };
            entry.unregister();
            if entry.complete() {
                entry.wake();
                woke = woke.saturating_add(1);
            }
        }
        woke
    }
}

/// Future returned by [`Timer::sleep`] and [`Timer::sleep_until`].
pub struct Sleep {
    timer: &'static Timer,
    entry: Arc<SleepEntry>,
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.entry.is_completed() {
            return Poll::Ready(());
        }

        self.entry.install_waker(cx.waker());
        if self.entry.deadline <= Timer::current_time() {
            self.entry.unregister();
            if self.entry.complete() {
                return Poll::Ready(());
            }
        }

        if self.entry.register() {
            self.timer.register(self.entry.clone());
        }

        if self.entry.is_completed() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        if !self.entry.is_completed() && self.entry.unregister() {
            self.timer.unregister(&self.entry);
        }
        sleep_ids().recycle(self.entry.id);
    }
}

/// Install the global scheduler timer.
pub fn init_timer() -> &'static Timer {
    match TIMER.try_init(Timer::new()) {
        Ok(()) | Err(TryInitError::AlreadyInitialized(_)) => TIMER.get(),
        Err(TryInitError::Initializing(_)) => {
            panic!("scheduler timer initialization in progress")
        }
    }
}

/// Return the installed scheduler timer, if available.
pub fn try_timer() -> Option<&'static Timer> {
    TIMER.is_initialized().then(|| TIMER.get())
}

/// Return the installed scheduler timer.
pub fn timer() -> &'static Timer {
    TIMER.get()
}
