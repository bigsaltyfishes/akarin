use core::{alloc::Layout, ops::Range, ptr::NonNull};

use libakarin_boot_proto::{
    BootInfo,
    memory::{ArenaKind, MemoryMapIter},
};
use libakarin_machine_core::{
    cpu::PerCpuTrait,
    memory::{
        AddressSpaceTrait, AllocationError, FrameAllocatorTrait, FrameZone, PhysAddr,
        VirtAddr as MachineVirtAddr,
    },
    sync::{NoOp, ScopedGuard},
};
use libakarin_sync::spin::{Once, SpinLock};

use super::{
    AllocPhase, HeapState, PAGE_SIZE, PERCPU_SLUB_ALLOCATOR, SMALL_ALLOC_LIMIT,
    early::{EarlyAllocator, FreeRegionIter, LOW_MEM_THRESHOLD},
    frame::FrameAllocator,
    slab::{BootstrapSlubState, SlubRuntime},
};
use crate::arch::{Machine, PerCpu, guards::IrqSaveGuard};

pub struct MemorySubsystem {
    state: SpinLock<HeapState, IrqSaveGuard>,
    boot_heap: SpinLock<EarlyAllocator, IrqSaveGuard>,
    frame_allocator: Once<FrameAllocator, ScopedGuard<NoOp>>,
    slub_runtime: Once<SlubRuntime, ScopedGuard<NoOp>>,
    bootstrap_slub: SpinLock<BootstrapSlubState, IrqSaveGuard>,
}

impl MemorySubsystem {
    pub const fn new() -> Self {
        Self {
            state: SpinLock::new(HeapState::new()),
            boot_heap: SpinLock::new(EarlyAllocator::new()),
            frame_allocator: Once::new(),
            slub_runtime: Once::new(),
            bootstrap_slub: SpinLock::new(BootstrapSlubState::new()),
        }
    }

    pub fn init_boot_heap(&self, info: &BootInfo) {
        let total = {
            let mut heap = self.boot_heap.lock();
            unsafe { heap.init_from_map(info.memory_map.iter()) };
            if heap.is_empty() {
                panic!("early allocator has no usable region");
            }
            heap.iter_regions().map(|(_, size)| size).sum()
        };

        {
            let mut state = self.state.lock();
            state.phase = AllocPhase::BootHeap;
            state.early_total_bytes = total;
        }

        log::info!("[memory] early allocator ready: total={:#x}", total);
    }

    pub fn init_frame_allocator(&self, info: &BootInfo, cpu_count: usize) {
        let cpu_count = cpu_count.max(1);
        let max_phys = info
            .memory_map
            .iter()
            .filter(|arena| arena.kind == ArenaKind::Usable)
            .map(|arena| arena.end)
            .max()
            .unwrap_or(0);
        let frame_allocator = FrameAllocator::new(cpu_count, PhysAddr::new(max_phys));

        let (slub_runtime, total_frames, used_frames) = {
            let mut heap = self.boot_heap.lock();
            let (section_count, total_pages) = self.boot_usable_range_stats(&heap, info);
            let storage = SlubRuntime::alloc_storage(&mut heap, section_count, total_pages)
                .expect("failed to allocate SLUB sparse metadata storage");
            let slub_runtime =
                SlubRuntime::from_storage(storage, self.boot_usable_ranges(&heap, info))
                    .expect("failed to build SLUB sparse metadata");
            frame_allocator.feed(self.boot_usable_ranges(&heap, info));
            (
                slub_runtime,
                frame_allocator.total_frames(),
                frame_allocator.used_frames(),
            )
        };

        self.frame_allocator.init(frame_allocator);
        self.slub_runtime.init(slub_runtime);

        log::info!(
            "[memory] frame allocator ready: total_frames={}, used_frames={}",
            total_frames,
            used_frames
        );
    }

    pub unsafe fn enter_next_phase(&self) {
        match self.phase() {
            AllocPhase::Uninit => panic!("memory subsystem is not initialized"),
            AllocPhase::BootHeap => {
                self.frame_allocator.get();
                self.slub_runtime.get();
                self.state.lock().phase = AllocPhase::BootstrapSlub;
                log::info!("[memory] allocator phase -> BootstrapSlub");
            }
            AllocPhase::BootstrapSlub => {
                let slub = self.slub_runtime.get();
                let frame_allocator = self.frame_allocator.get();
                {
                    let mut bootstrap = self.bootstrap_slub.lock();
                    slub.release(bootstrap.current_pages_mut(), frame_allocator);
                }

                let cpu_count = PerCpu::count();
                let current_cpu_id = PerCpu::id();
                for cpu_id in 0..cpu_count {
                    if cpu_id == current_cpu_id {
                        PERCPU_SLUB_ALLOCATOR.with_current(|state| {
                            state.ready = true;
                        });
                        continue;
                    }

                    let state = unsafe { PERCPU_SLUB_ALLOCATOR.remote_ref_mut_raw(cpu_id) }
                        .expect("missing remote per-CPU SLUB state");
                    state.ready = true;
                }

                self.state.lock().phase = AllocPhase::PerCpuSlub;
                log::info!(
                    "[memory] allocator phase -> PerCpuSlub ({} CPU states ready)",
                    cpu_count
                );
            }
            AllocPhase::PerCpuSlub => {}
        }
    }

    pub fn alloc_global(&self, layout: Layout) -> *mut u8 {
        if layout.size() == 0 {
            return NonNull::<u8>::dangling().as_ptr();
        }

        match self.phase() {
            AllocPhase::Uninit => core::ptr::null_mut(),
            AllocPhase::BootHeap => self
                .boot_heap_alloc(layout)
                .map(NonNull::as_ptr)
                .unwrap_or(core::ptr::null_mut()),
            AllocPhase::BootstrapSlub => self
                .alloc_runtime(layout, false)
                .map(NonNull::as_ptr)
                .unwrap_or(core::ptr::null_mut()),
            AllocPhase::PerCpuSlub => self
                .alloc_runtime(layout, true)
                .map(NonNull::as_ptr)
                .unwrap_or(core::ptr::null_mut()),
        }
    }

    pub unsafe fn dealloc_global(&self, ptr: *mut u8, layout: Layout) {
        if ptr.is_null() {
            return;
        }

        match self.phase() {
            AllocPhase::Uninit => {}
            AllocPhase::BootHeap => unsafe { self.boot_heap_dealloc(ptr as usize, layout) },
            AllocPhase::BootstrapSlub => self.dealloc_runtime(ptr as usize, layout, false),
            AllocPhase::PerCpuSlub => self.dealloc_runtime(ptr as usize, layout, true),
        }
    }

    pub fn alloc_pages(&self, num_pages: usize, alignment: usize) -> Option<usize> {
        self.alloc_pages_with_zone(None, None, num_pages, alignment)
    }

    #[allow(dead_code)]
    pub fn alloc_pages_in_zone(
        &self,
        num_pages: usize,
        alignment: usize,
        zone: FrameZone,
    ) -> Option<usize> {
        self.alloc_pages_with_zone(None, Some(zone), num_pages, alignment)
    }

    #[allow(dead_code)]
    pub fn alloc_pages_at(
        &self,
        phys_addr: usize,
        num_pages: usize,
        alignment: usize,
        zone: FrameZone,
    ) -> Option<usize> {
        self.alloc_pages_with_zone(
            Some(PhysAddr::new(phys_addr)),
            Some(zone),
            num_pages,
            alignment,
        )
    }

    pub unsafe fn dealloc_pages(&self, addr: usize, num_pages: usize) {
        if num_pages == 0 {
            return;
        }

        if let Some(frame_allocator) = self.try_frame_allocator() {
            unsafe {
                frame_allocator.dealloc(MachineVirtAddr::new(addr), num_pages);
            }
            return;
        }

        let size = Self::align_up(num_pages * PAGE_SIZE, PAGE_SIZE);
        if let Ok(layout) = Layout::from_size_align(size, PAGE_SIZE) {
            unsafe { self.boot_heap_dealloc(addr, layout) };
        }
    }

    pub fn used_frames(&self) -> usize {
        if let Some(frame_allocator) = self.try_frame_allocator() {
            frame_allocator.used_frames()
        } else {
            let total = self.state.lock().early_total_bytes;
            let free = self
                .boot_heap
                .lock()
                .iter_regions()
                .map(|(_, size)| size)
                .sum::<usize>();
            total.saturating_sub(free).div_ceil(PAGE_SIZE)
        }
    }

    pub fn total_frames(&self) -> usize {
        if let Some(frame_allocator) = self.try_frame_allocator() {
            frame_allocator.total_frames()
        } else {
            self.state.lock().early_total_bytes.div_ceil(PAGE_SIZE)
        }
    }

    pub fn reclaim_reserved_range(&self, range: Range<PhysAddr>) {
        self.frame_allocator.get().feed(core::iter::once(range));
    }

    fn phase(&self) -> AllocPhase {
        self.state.lock().phase
    }

    fn alloc_runtime(&self, layout: Layout, per_cpu: bool) -> Option<NonNull<u8>> {
        if Self::is_small_layout(layout) {
            return self.alloc_small(layout, per_cpu);
        }

        let page_count = layout.size().div_ceil(PAGE_SIZE);
        self.alloc_pages_with_zone(None, Some(FrameZone::Normal), page_count, layout.align())
            .map(|addr| NonNull::new(addr as *mut u8).expect("non-null large allocation"))
    }

    fn dealloc_runtime(&self, ptr: usize, layout: Layout, per_cpu: bool) {
        if Self::is_small_layout(layout) {
            self.dealloc_small(ptr, layout, per_cpu);
            return;
        }

        let page_count = layout.size().div_ceil(PAGE_SIZE);
        unsafe { self.dealloc_pages(ptr, page_count) };
    }

    fn alloc_small(&self, layout: Layout, per_cpu: bool) -> Option<NonNull<u8>> {
        let Some(class_index) = SlubRuntime::class_index(layout) else {
            return None;
        };
        let frame_allocator = self.frame_allocator.get();
        let slub = self.slub_runtime.get();

        if per_cpu {
            return PERCPU_SLUB_ALLOCATOR.with_current(|state| {
                if !state.ready {
                    return None;
                }
                slub.alloc(state.current_pages_mut(), frame_allocator, class_index)
            });
        }

        let mut bootstrap = self.bootstrap_slub.lock();
        slub.alloc(bootstrap.current_pages_mut(), frame_allocator, class_index)
    }

    fn dealloc_small(&self, ptr: usize, layout: Layout, per_cpu: bool) {
        let frame_allocator = self.frame_allocator.get();
        let slub = self.slub_runtime.get();

        if per_cpu {
            PERCPU_SLUB_ALLOCATOR.with_current(|state| {
                if !state.ready {
                    return;
                }
                slub.dealloc(state.current_pages_mut(), frame_allocator, ptr, layout);
            });
            return;
        }

        let mut bootstrap = self.bootstrap_slub.lock();
        slub.dealloc(bootstrap.current_pages_mut(), frame_allocator, ptr, layout);
    }

    fn alloc_pages_with_zone(
        &self,
        addr: Option<PhysAddr>,
        prefer_zone: Option<FrameZone>,
        num_pages: usize,
        alignment: usize,
    ) -> Option<usize> {
        if num_pages == 0 {
            return None;
        }

        if let Some(frame_allocator) = self.try_frame_allocator() {
            let zone = prefer_zone.unwrap_or(FrameZone::Normal);
            let page_count = Self::aligned_page_count(num_pages, alignment)?;
            return frame_allocator
                .alloc(addr, zone, page_count)
                .ok()
                .map(|addr| addr.as_usize());
        }

        if addr.is_some() {
            return None;
        }

        let size = Self::align_up(num_pages * PAGE_SIZE, PAGE_SIZE);
        let layout = Layout::from_size_align(size, alignment.max(PAGE_SIZE)).ok()?;
        self.boot_heap_alloc(layout)
            .map(|ptr| ptr.as_ptr() as usize)
    }

    fn boot_heap_alloc(&self, layout: Layout) -> Option<NonNull<u8>> {
        let phys = self.boot_heap.lock().alloc(layout)?;
        let virt = Machine::phys_to_virt(PhysAddr::new(phys))?.as_usize();
        NonNull::new(virt as *mut u8)
    }

    unsafe fn boot_heap_dealloc(&self, ptr: usize, layout: Layout) {
        let Some(phys) = Machine::virt_to_phys(MachineVirtAddr::new(ptr)).map(|p| p.as_usize())
        else {
            return;
        };
        unsafe { self.boot_heap.lock().add_region(phys, layout.size().max(1)) };
    }

    fn boot_usable_ranges<'a>(
        &self,
        heap: &'a EarlyAllocator,
        info: &'a BootInfo,
    ) -> BootUsableRanges<'a> {
        BootUsableRanges {
            heap_iter: heap.iter_regions(),
            map_iter: info.memory_map.iter(),
            stage: BootUsableRangeStage::Heap,
        }
    }

    fn boot_usable_range_stats(&self, heap: &EarlyAllocator, info: &BootInfo) -> (usize, usize) {
        self.boot_usable_ranges(heap, info)
            .fold((0usize, 0usize), |(sections, pages), range| {
                (
                    sections + 1,
                    pages + (range.end.as_usize() - range.start.as_usize()) / PAGE_SIZE,
                )
            })
    }

    fn try_frame_allocator(&self) -> Option<&FrameAllocator> {
        self.frame_allocator
            .is_initialized()
            .then(|| self.frame_allocator.get())
    }

    fn is_small_layout(layout: Layout) -> bool {
        layout.size() <= SMALL_ALLOC_LIMIT && layout.align() <= SMALL_ALLOC_LIMIT
    }

    fn aligned_page_count(num_pages: usize, alignment: usize) -> Option<usize> {
        let pages_pow2 = num_pages.max(1).checked_next_power_of_two()?;
        let align_pages = alignment
            .max(PAGE_SIZE)
            .div_ceil(PAGE_SIZE)
            .checked_next_power_of_two()?;
        Some(pages_pow2.max(align_pages))
    }

    fn align_up(value: usize, align: usize) -> usize {
        value.next_multiple_of(align)
    }
}

unsafe impl core::alloc::GlobalAlloc for MemorySubsystem {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.alloc_global(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.dealloc_global(ptr, layout) };
    }
}

impl FrameAllocatorTrait for MemorySubsystem {
    fn unit_page_size(&self) -> usize {
        PAGE_SIZE
    }

    fn alloc(
        &self,
        addr: Option<PhysAddr>,
        prefer_zone: FrameZone,
        num: usize,
    ) -> Result<MachineVirtAddr, AllocationError> {
        self.alloc_pages_with_zone(addr, Some(prefer_zone), num, PAGE_SIZE)
            .map(MachineVirtAddr::new)
            .ok_or(AllocationError::OutOfMemory)
    }

    unsafe fn dealloc(&self, frame_addr: MachineVirtAddr, num: usize) {
        unsafe { self.dealloc_pages(frame_addr.as_usize(), num) };
    }

    fn used_frames(&self) -> usize {
        MemorySubsystem::used_frames(self)
    }

    fn total_frames(&self) -> usize {
        MemorySubsystem::total_frames(self)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BootUsableRangeStage {
    Heap,
    LowMem,
    Done,
}

struct BootUsableRanges<'a> {
    heap_iter: FreeRegionIter<'a>,
    map_iter: MemoryMapIter<'a>,
    stage: BootUsableRangeStage,
}

impl Iterator for BootUsableRanges<'_> {
    type Item = Range<PhysAddr>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.stage {
                BootUsableRangeStage::Heap => {
                    if let Some((start, size)) = self.heap_iter.next() {
                        let end = start.saturating_add(size);
                        return Some(PhysAddr::new(start)..PhysAddr::new(end));
                    }
                    self.stage = BootUsableRangeStage::LowMem;
                }
                BootUsableRangeStage::LowMem => {
                    for arena in self.map_iter.by_ref() {
                        if arena.kind != ArenaKind::Usable {
                            continue;
                        }
                        let low_end = arena.end.min(LOW_MEM_THRESHOLD);
                        if arena.start < low_end {
                            return Some(PhysAddr::new(arena.start)..PhysAddr::new(low_end));
                        }
                    }
                    self.stage = BootUsableRangeStage::Done;
                }
                BootUsableRangeStage::Done => return None,
            }
        }
    }
}
