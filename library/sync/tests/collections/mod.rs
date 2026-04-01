use lazy_static::lazy_static;
use libakarin_sync::gc::{Collector, GarbageCollector, Guard, LocalHandle};

mod btree;
mod skiplist;

lazy_static! {
    static ref CPU_NUM: usize = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
}

lazy_static! {
    static ref COLLECTOR: Collector = Collector::new(*CPU_NUM).batch_size(32);
}

thread_local! {
    static LOCAL_HANDLE: LocalHandle = COLLECTOR.register();
}

#[derive(Clone)]
struct GC;

impl GarbageCollector for GC {
    fn global_handle() -> Collector {
        COLLECTOR.clone()
    }

    fn local_handle() -> LocalHandle {
        LOCAL_HANDLE.with(|handle| handle.clone())
    }

    fn pin() -> Guard {
        LOCAL_HANDLE.with(|handle| handle.pin())
    }
}
