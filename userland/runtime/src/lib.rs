#![no_std]

extern crate alloc;

mod allocator;
mod futex;
mod startup;
mod syscall;

pub use allocator::{GLOBAL_ALLOCATOR, HeapAllocatorError};
pub use futex::{Futex, FutexCallError, FutexMutex, FutexMutexGuard};
pub use libakarin_core::{
    clock::time::Duration,
    memory::{RegionPurpose, VmFlags},
};
pub use libakarin_syscall::{
    FutexWaitFlags, FutexWakeFlags, SyscallStatus, VmoChildMode, VmoOpRangeOperation,
    errno::FutexError,
};
pub use startup::{RUNTIME, Runtime, RuntimeInitError, StartupInfo};
pub use syscall::{RawSyscallInvoker, SyscallFailure};
