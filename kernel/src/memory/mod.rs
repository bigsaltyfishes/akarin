use libakarin_boot_proto::BootInfo;
use libakarin_macros::cpu_local;

mod early;
mod frame;
mod heap;
mod slab;

pub use heap::MemorySubsystem;

#[global_allocator]
pub static MEMORY_SUBSYSTEM: MemorySubsystem = MemorySubsystem::new();

pub fn init_boot_heap(info: &BootInfo) {
    MEMORY_SUBSYSTEM.init_boot_heap(info);
}

pub fn init_frame_allocator(info: &BootInfo, cpu_count: usize) {
    MEMORY_SUBSYSTEM.init_frame_allocator(info, cpu_count);
}

pub unsafe fn enter_next_phase() {
    unsafe { MEMORY_SUBSYSTEM.enter_next_phase() };
}

pub fn alloc_pages(num_pages: usize, alignment: usize) -> Option<usize> {
    MEMORY_SUBSYSTEM.alloc_pages(num_pages, alignment)
}

const PAGE_SIZE: usize = 0x1000;
const SMALL_ALLOC_LIMIT: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AllocPhase {
    Uninit,
    BootHeap,
    BootstrapSlub,
    PerCpuSlub,
}

struct HeapState {
    phase: AllocPhase,
    early_total_bytes: usize,
}

impl HeapState {
    const fn new() -> Self {
        Self {
            phase: AllocPhase::Uninit,
            early_total_bytes: 0,
        }
    }
}

cpu_local! {
    static PERCPU_SLUB_ALLOCATOR: slab::PerCpuSlubState = slab::PerCpuSlubState::new();
}
