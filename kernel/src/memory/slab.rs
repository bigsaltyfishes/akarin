use core::{
    alloc::Layout,
    ops::Range,
    ptr::NonNull,
    slice,
    sync::atomic::{AtomicUsize, Ordering},
};

use libakarin_machine_core::{
    memory::{AddressSpaceTrait, FrameAllocatorTrait, FrameZone, PhysAddr, VirtAddr},
    sync::RawScopedGuard,
};

use super::{PAGE_SIZE, early::EarlyAllocator};
use crate::arch::{Machine, guards::IrqSaveGuard};

const SIZE_CLASSES: [usize; 9] = [8, 16, 32, 64, 128, 256, 512, 1024, 2048];

#[repr(C)]
struct SlubPageMeta {
    free_head: AtomicUsize,
    refcount: AtomicUsize,
}

struct SlubSection {
    start_phys: usize,
    end_phys: usize,
    metas: &'static [SlubPageMeta],
}

pub(super) struct SlubStorage {
    sections: &'static mut [SlubSection],
    all_metas: &'static mut [SlubPageMeta],
}

pub(super) struct SlubRuntime {
    sections: &'static [SlubSection],
}

impl SlubRuntime {
    pub(super) fn alloc_storage(
        boot_heap: &mut EarlyAllocator,
        section_count: usize,
        total_pages: usize,
    ) -> Option<SlubStorage> {
        if section_count == 0 || total_pages == 0 {
            return None;
        }

        Some(SlubStorage {
            sections: Self::alloc_slice::<SlubSection>(boot_heap, section_count)?,
            all_metas: Self::alloc_slice::<SlubPageMeta>(boot_heap, total_pages)?,
        })
    }

    pub(super) fn from_storage<I>(storage: SlubStorage, ranges: I) -> Option<Self>
    where
        I: IntoIterator<Item = Range<PhysAddr>>,
    {
        let reserved_sections = storage.sections.len();
        let reserved_pages = storage.all_metas.len();
        let sections = storage.sections;
        let all_metas = storage.all_metas;
        let mut next_section = 0usize;
        let mut next_meta = 0usize;
        for range in ranges {
            let start = Self::align_up(range.start.as_usize(), PAGE_SIZE);
            let end = Self::align_down(range.end.as_usize(), PAGE_SIZE);
            if end <= start {
                continue;
            }

            let page_count = (end - start) / PAGE_SIZE;
            if next_section >= sections.len() || next_meta + page_count > all_metas.len() {
                return None;
            }
            let metas = &all_metas[next_meta..next_meta + page_count];
            sections[next_section] = SlubSection {
                start_phys: start,
                end_phys: end,
                metas: &*metas,
            };
            next_section += 1;
            next_meta += page_count;
        }

        let sections = &sections[..next_section];

        log::info!(
            "[memory/slub] sparse metadata ready: sections={}, tracked_pages={}, \
             reserved_sections={}, reserved_pages={}",
            sections.len(),
            sections
                .iter()
                .map(|section| section.metas.len())
                .sum::<usize>(),
            reserved_sections,
            reserved_pages,
        );

        Some(Self { sections })
    }

    pub(super) fn class_index(layout: Layout) -> Option<usize> {
        let needed = layout.size().max(layout.align()).max(SIZE_CLASSES[0]);
        SIZE_CLASSES.iter().position(|&size| needed <= size)
    }

    pub(super) fn alloc(
        &self,
        current_pages: &mut [usize; SIZE_CLASSES.len()],
        frame_allocator: &super::frame::FrameAllocator,
        class_index: usize,
    ) -> Option<NonNull<u8>> {
        let _guard = IrqSaveGuard::enter();
        let object_size = SIZE_CLASSES[class_index];

        loop {
            let current_page = current_pages[class_index];
            if current_page == 0 {
                let page_virt = frame_allocator.alloc(None, FrameZone::Normal, 1).ok()?;
                let page_phys = Machine::virt_to_phys(page_virt).expect("invalid slab page");
                let meta = self
                    .meta_for_phys(page_phys)
                    .expect("missing slab metadata for allocated page");
                unsafe { self.initialize_page(meta, page_virt.as_usize(), object_size) };
                meta.refcount.store(1, Ordering::Release);
                current_pages[class_index] = page_phys.as_usize();
                continue;
            }

            let page_phys = PhysAddr::new(current_page);
            let meta = self
                .meta_for_phys(page_phys)
                .expect("missing slab metadata for current page");
            let head = meta.free_head.load(Ordering::Acquire);
            if head == 0 {
                current_pages[class_index] = 0;
                self.release_cache_reference(meta, page_phys, frame_allocator);
                continue;
            }

            let next = unsafe { *(head as *const usize) };
            if meta
                .free_head
                .compare_exchange(head, next, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                core::hint::spin_loop();
                continue;
            }

            meta.refcount.fetch_add(1, Ordering::AcqRel);
            if next == 0 {
                current_pages[class_index] = 0;
                self.release_cache_reference(meta, page_phys, frame_allocator);
            }

            return NonNull::new(head as *mut u8);
        }
    }

    pub(super) fn dealloc(
        &self,
        current_pages: &mut [usize; SIZE_CLASSES.len()],
        frame_allocator: &super::frame::FrameAllocator,
        ptr: usize,
        layout: Layout,
    ) {
        let _guard = IrqSaveGuard::enter();
        let Some(class_index) = Self::class_index(layout) else {
            return;
        };
        let Some(page_phys) = self.page_base_for_ptr(ptr) else {
            return;
        };
        let Some(meta) = self.meta_for_phys(page_phys) else {
            return;
        };

        let page_was_full = self.push_free_object(meta, ptr);
        let old_refcount = meta.refcount.fetch_sub(1, Ordering::AcqRel);
        if old_refcount == 1 {
            self.free_page(meta, page_phys, frame_allocator);
            return;
        }

        if page_was_full && current_pages[class_index] == 0 {
            meta.refcount.fetch_add(1, Ordering::AcqRel);
            current_pages[class_index] = page_phys.as_usize();
        }
    }

    pub(super) fn release(
        &self,
        current_pages: &mut [usize; SIZE_CLASSES.len()],
        frame_allocator: &super::frame::FrameAllocator,
    ) {
        let _guard = IrqSaveGuard::enter();
        for page in current_pages.iter_mut() {
            if *page == 0 {
                continue;
            }

            let page_phys = PhysAddr::new(*page);
            let meta = self
                .meta_for_phys(page_phys)
                .expect("missing slab metadata for current page");
            self.release_cache_reference(meta, page_phys, frame_allocator);
            *page = 0;
        }
    }

    fn meta_for_phys(&self, phys: PhysAddr) -> Option<&SlubPageMeta> {
        let phys = phys.as_usize();
        for section in self.sections {
            if !(section.start_phys..section.end_phys).contains(&phys) {
                continue;
            }

            let page_index = (phys - section.start_phys) / PAGE_SIZE;
            return section.metas.get(page_index);
        }
        None
    }

    fn page_base_for_ptr(&self, ptr: usize) -> Option<PhysAddr> {
        let phys = Machine::virt_to_phys(VirtAddr::new(ptr))?;
        Some(PhysAddr::new(Self::align_down(phys.as_usize(), PAGE_SIZE)))
    }

    fn push_free_object(&self, meta: &SlubPageMeta, ptr: usize) -> bool {
        loop {
            let head = meta.free_head.load(Ordering::Acquire);
            unsafe {
                *(ptr as *mut usize) = head;
            }
            if meta
                .free_head
                .compare_exchange(head, ptr, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return head == 0;
            }

            core::hint::spin_loop();
        }
    }

    unsafe fn initialize_page(&self, meta: &SlubPageMeta, page_virt: usize, object_size: usize) {
        let object_count = PAGE_SIZE / object_size;
        let mut head = 0usize;
        for index in (0..object_count).rev() {
            let object = page_virt + index * object_size;
            unsafe {
                *(object as *mut usize) = head;
            }
            head = object;
        }
        meta.free_head.store(head, Ordering::Release);
    }

    fn release_cache_reference(
        &self,
        meta: &SlubPageMeta,
        page_phys: PhysAddr,
        frame_allocator: &super::frame::FrameAllocator,
    ) {
        if meta.refcount.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.free_page(meta, page_phys, frame_allocator);
        }
    }

    fn free_page(
        &self,
        meta: &SlubPageMeta,
        page_phys: PhysAddr,
        frame_allocator: &super::frame::FrameAllocator,
    ) {
        meta.free_head.store(0, Ordering::Release);
        meta.refcount.store(0, Ordering::Release);
        let page_virt = Machine::phys_to_virt(page_phys).expect("invalid slab page");
        unsafe {
            frame_allocator.dealloc(page_virt, 1);
        }
    }

    fn alloc_slice<T>(boot_heap: &mut EarlyAllocator, len: usize) -> Option<&'static mut [T]> {
        let layout = Layout::array::<T>(len.max(1)).ok()?;
        let phys = boot_heap.alloc(layout)?;
        let virt = Machine::phys_to_virt(PhysAddr::new(phys))?.as_usize();
        unsafe {
            core::ptr::write_bytes(virt as *mut u8, 0, layout.size());
            Some(slice::from_raw_parts_mut(virt as *mut T, len))
        }
    }

    fn align_up(value: usize, align: usize) -> usize {
        value.next_multiple_of(align)
    }

    fn align_down(value: usize, align: usize) -> usize {
        (value / align) * align
    }
}

pub(super) struct BootstrapSlubState {
    current_pages: [usize; SIZE_CLASSES.len()],
}

impl BootstrapSlubState {
    pub(super) const fn new() -> Self {
        Self {
            current_pages: [0; SIZE_CLASSES.len()],
        }
    }

    pub(super) fn current_pages_mut(&mut self) -> &mut [usize; SIZE_CLASSES.len()] {
        &mut self.current_pages
    }
}

pub(super) struct PerCpuSlubState {
    pub(super) ready: bool,
    current_pages: [usize; SIZE_CLASSES.len()],
}

impl PerCpuSlubState {
    pub(super) const fn new() -> Self {
        Self {
            ready: false,
            current_pages: [0; SIZE_CLASSES.len()],
        }
    }

    pub(super) fn current_pages_mut(&mut self) -> &mut [usize; SIZE_CLASSES.len()] {
        &mut self.current_pages
    }
}
