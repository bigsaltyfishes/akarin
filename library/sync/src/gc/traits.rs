use crate::gc::{Collector, Guard, LocalHandle};

/// Advanced hook for custom garbage-collector backends.
///
/// Most code should use the default [`crate::gc::GlobalGc`]-backed collection
/// types and should not need to mention this trait explicitly. It remains
/// public so tests, benchmarks, and low-level experiments can provide custom
/// collector wiring when necessary.
pub trait GarbageCollector: Send + Sync + 'static {
    /// Get the global collector handle.
    fn global_handle() -> Collector;

    /// Get the local handle for current CPU.
    fn local_handle() -> LocalHandle;

    /// Pin the current thread to prevent reclamation.
    fn pin() -> Guard;
}
