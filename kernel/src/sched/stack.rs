use libakarin_core::memory::{GuardedStackLayout, VmLayoutSegment};
use libakarin_machine_core::memory::{
    AddressSpaceTrait, PageTableTrait, VirtAddr,
    paging::{CachePolicy, MMUFlags, PhysFrameTrait},
};

use crate::{
    RuntimeServices,
    arch::{
        Machine,
        vm::{Page, PageSize, PageTable, PhysFrame},
    },
    memory,
};

pub const PAGE_SIZE: usize = 0x1000;
pub const DEFAULT_STACK_PAGES: usize = 4;
pub const TASK_KERNEL_STACK_PAGES: usize = 8;
const GUARD_PAGES: usize = 1;
const KERNEL_STACK_SEGMENT_START: usize = VmLayoutSegment::KernelStack.start();

/// Fixed per-CPU fallback stacks that are installed during BSP bringup.
pub struct CpuFallbackStack;

impl CpuFallbackStack {
    fn layout(cpu_id: usize) -> Option<GuardedStackLayout> {
        let reserved_pages = DEFAULT_STACK_PAGES.checked_add(GUARD_PAGES)?;
        let slot_size = reserved_pages.checked_mul(PAGE_SIZE)?;
        let base = KERNEL_STACK_SEGMENT_START.checked_add(cpu_id.checked_mul(slot_size)?)?;
        GuardedStackLayout::from_low_base(
            VirtAddr::new(base),
            DEFAULT_STACK_PAGES.checked_mul(PAGE_SIZE)?,
        )
    }

    fn entry_stack_pointer(stack_top: usize) -> Option<usize> {
        let rsp = stack_top.checked_sub(core::mem::size_of::<usize>())?;
        unsafe {
            (rsp as *mut usize).write(0);
        }
        Some(rsp)
    }

    pub fn install(cpu_id: usize) -> Option<usize> {
        let layout = Self::layout(cpu_id)?;
        let backing_base = memory::alloc_pages(DEFAULT_STACK_PAGES, PAGE_SIZE)?;
        let mut page_table = PageTable::from_active(RuntimeServices::global().frame_allocator());

        for page_index in 0..DEFAULT_STACK_PAGES {
            let virt = layout.stack.start().as_usize() + page_index * PAGE_SIZE;
            let backing = backing_base + page_index * PAGE_SIZE;
            let phys = Machine::virt_to_phys(VirtAddr::new(backing))?;
            let frame = unsafe { PhysFrame::from_addr(None, phys, 1) };
            let invalidator = unsafe {
                page_table
                    .map(
                        Page::new(VirtAddr::new(virt), PageSize::Size4K),
                        frame,
                        MMUFlags::WRITE | MMUFlags::GLOBAL,
                        CachePolicy::Cached,
                    )
                    .ok()?
            };
            invalidator.invalidate();
        }

        Some(layout.top().as_usize())
    }

    pub fn top(cpu_id: usize) -> usize {
        Self::layout(cpu_id)
            .expect("fallback stack layout must be valid")
            .top()
            .as_usize()
    }

    pub fn boot_entry_stack_pointer(cpu_id: usize) -> usize {
        Self::entry_stack_pointer(Self::top(cpu_id))
            .expect("fallback stack top must accommodate a sentinel return address")
    }

    pub fn contains(cpu_id: usize, rsp: usize) -> bool {
        let layout = Self::layout(cpu_id).expect("fallback stack layout must be valid");
        layout.guard.contains(VirtAddr::new(rsp)) || layout.stack.contains(VirtAddr::new(rsp))
    }
}
