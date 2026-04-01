//! A Garbage Collector based on [seize](https://github.com/ibraheemdev/seize)
//! `atomic.rs` and `list.rs` are taken from [crossbeam-epoch](https://github.com/crossbeam-rs/crossbeam)
//! `seize` and `crossbeam-epoch` are under MIT license, full text see
//! [LICENCE-crossbeam](../../third_party/crossbeam-epoch/LICENSE) and
//! [LICENSE-seize](../../third_party/seize/LICENSE)
//!
//! Based commit:
//! - `seize`: 4e746342f6b8a383234b491d3cf4cae697fdad28
//! - `crossbeam-epoch`: 983d56b6007ca4c22b56a665a7785f40f55c2a53

mod atomic;
mod collector;
mod guard;
mod internal;
mod list;
mod traits;

pub mod reclaim;

#[cfg(not(feature = "host-thread-local-gc"))]
use core::cell::UnsafeCell;
#[cfg(feature = "host-thread-local-gc")]
use std::{cell::RefCell, thread_local};

pub use atomic::{Atomic, Owned, Pointer, Shared};
pub use collector::{Collector, LocalHandle};
pub use guard::{Guard, unprotected};
use libakarin_machine_core::sync::{NoOp, ScopedGuard};
pub use list::{Entry, IsElement, Iter, IterError, List};
pub use traits::GarbageCollector;

use crate::spin::Once;

static GLOBAL_COLLECTOR: Once<Collector, ScopedGuard<NoOp>> = Once::new();

#[cfg(not(feature = "host-thread-local-gc"))]
static CURRENT_LOCAL: LocalSlot = LocalSlot::new();

#[cfg(not(feature = "host-thread-local-gc"))]
struct LocalSlot(UnsafeCell<Option<LocalHandle>>);

#[cfg(not(feature = "host-thread-local-gc"))]
unsafe impl Sync for LocalSlot {}

#[cfg(not(feature = "host-thread-local-gc"))]
impl LocalSlot {
    const fn new() -> Self {
        Self(UnsafeCell::new(None))
    }

    fn set(&self, value: LocalHandle) {
        unsafe { *self.0.get() = Some(value) }
    }

    fn clone_current(&self) -> Option<LocalHandle> {
        unsafe { (&*self.0.get()).as_ref().cloned() }
    }
}

#[cfg(feature = "host-thread-local-gc")]
thread_local! {
    static CURRENT_LOCAL: RefCell<Option<LocalHandle>> = const { RefCell::new(None) };
}

pub struct GlobalGc;

impl GarbageCollector for GlobalGc {
    fn global_handle() -> Collector {
        global_collector().clone()
    }

    fn local_handle() -> LocalHandle {
        current_local().expect("current GC local handle is not installed")
    }

    fn pin() -> Guard {
        Self::local_handle().pin()
    }
}

pub fn install_global_collector(collector: Collector) -> &'static Collector {
    GLOBAL_COLLECTOR.get_or_else(|| collector.clone())
}

pub fn global_collector() -> &'static Collector {
    GLOBAL_COLLECTOR.get()
}

pub fn set_current_local(local: LocalHandle) {
    #[cfg(not(feature = "host-thread-local-gc"))]
    {
        CURRENT_LOCAL.set(local);
    }

    #[cfg(feature = "host-thread-local-gc")]
    {
        CURRENT_LOCAL.with(|slot| {
            *slot.borrow_mut() = Some(local);
        });
    }
}

pub fn current_local() -> Option<LocalHandle> {
    #[cfg(not(feature = "host-thread-local-gc"))]
    {
        CURRENT_LOCAL.clone_current()
    }

    #[cfg(feature = "host-thread-local-gc")]
    {
        CURRENT_LOCAL.with(|slot| slot.borrow().as_ref().cloned())
    }
}
