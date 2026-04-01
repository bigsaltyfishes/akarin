use core::{
    alloc::{GlobalAlloc, Layout},
    cmp, ptr,
};

use dlmalloc::{Allocator as DlmallocAllocator, Dlmalloc};
use libakarin_core::memory::{PAGE_SIZE, RegionPurpose, VmFlags};
use libakarin_syscall::VmoOpRangeOperation;
use spin::Mutex;

use crate::{
    startup::StartupInfo,
    syscall::{RawSyscallInvoker, SyscallFailure},
};

const USER_HEAP_MAX_SEGMENTS: usize = 1024;

/// Heap allocator initialization failures visible to runtime bootstrap code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeapAllocatorError {
    AlreadyInitialized,
    InvalidHeapWindow,
    InvalidPageSize,
    SegmentTableFull,
    SegmentNotFound,
    UnexpectedMapping,
    Syscall(SyscallFailure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UserHeapSegment {
    base: usize,
    mapped_size: usize,
    reserved_size: usize,
    vmar_slot: u32,
    vmo_slot: u32,
}

#[derive(Debug, Clone, Copy)]
struct UserHeapState {
    initialized: bool,
    heap_vmar_slot: u32,
    heap_base: usize,
    heap_limit: usize,
    page_size: usize,
    segment_count: usize,
    segments: [Option<UserHeapSegment>; USER_HEAP_MAX_SEGMENTS],
}

impl UserHeapState {
    const fn new() -> Self {
        Self {
            initialized: false,
            heap_vmar_slot: 0,
            heap_base: 0,
            heap_limit: 0,
            page_size: 0,
            segment_count: 0,
            segments: [None; USER_HEAP_MAX_SEGMENTS],
        }
    }

    fn initialize_from_startup(&mut self, startup: &StartupInfo) -> Result<(), HeapAllocatorError> {
        if self.initialized {
            return Err(HeapAllocatorError::AlreadyInitialized);
        }
        if startup.page_size == 0 || !startup.page_size.is_power_of_two() {
            return Err(HeapAllocatorError::InvalidPageSize);
        }
        if !startup.heap_base.is_multiple_of(startup.page_size)
            || !startup.heap_limit.is_multiple_of(startup.page_size)
            || startup.heap_base >= startup.heap_limit
        {
            return Err(HeapAllocatorError::InvalidHeapWindow);
        }

        self.initialized = true;
        self.heap_vmar_slot = startup.heap_vmar_slot;
        self.heap_base = startup.heap_base;
        self.heap_limit = startup.heap_limit;
        self.page_size = startup.page_size;
        self.segment_count = 0;
        self.segments.fill(None);
        Ok(())
    }

    fn page_aligned_size(&self, requested: usize) -> Result<usize, HeapAllocatorError> {
        if !self.initialized || self.page_size == 0 {
            return Err(HeapAllocatorError::InvalidHeapWindow);
        }

        let aligned = requested
            .max(self.page_size)
            .checked_add(self.page_size - 1)
            .map(|value| value & !(self.page_size - 1))
            .ok_or(HeapAllocatorError::InvalidHeapWindow)?;
        if aligned == 0 {
            return Err(HeapAllocatorError::InvalidHeapWindow);
        }

        Ok(aligned)
    }

    fn insert_segment(&mut self, segment: UserHeapSegment) -> Result<(), HeapAllocatorError> {
        if self.segment_count >= USER_HEAP_MAX_SEGMENTS {
            return Err(HeapAllocatorError::SegmentTableFull);
        }

        for slot in &mut self.segments {
            if slot.is_none() {
                *slot = Some(segment);
                self.segment_count += 1;
                return Ok(());
            }
        }

        Err(HeapAllocatorError::SegmentTableFull)
    }

    fn find_segment_index(&self, base: usize) -> Result<usize, HeapAllocatorError> {
        for (index, segment) in self.segments.iter().enumerate() {
            if matches!(segment, Some(segment) if segment.base == base) {
                return Ok(index);
            }
        }

        Err(HeapAllocatorError::SegmentNotFound)
    }
}

struct UserHeapSystem {
    state: Mutex<UserHeapState>,
}

impl UserHeapSystem {
    const fn new() -> Self {
        Self {
            state: Mutex::new(UserHeapState::new()),
        }
    }

    fn initialize(&self, startup: &StartupInfo) -> Result<(), HeapAllocatorError> {
        let mut state = self.state.lock();
        state.initialize_from_startup(startup)
    }

    fn tear_down_segment_locked(
        state: &UserHeapState,
        segment: UserHeapSegment,
    ) -> Result<(), HeapAllocatorError> {
        let invoker = RawSyscallInvoker;
        invoker
            .unmap_vmar(state.heap_vmar_slot, segment.base)
            .map_err(HeapAllocatorError::Syscall)?;
        if segment.vmo_slot != 0 {
            invoker
                .close_handle(segment.vmo_slot)
                .map_err(HeapAllocatorError::Syscall)?;
        }
        invoker
            .close_handle(segment.vmar_slot)
            .map_err(HeapAllocatorError::Syscall)?;
        Ok(())
    }

    fn allocate_segment_locked(
        state: &mut UserHeapState,
        size: usize,
    ) -> Result<(*mut u8, usize), HeapAllocatorError> {
        let invoker = RawSyscallInvoker;
        let aligned_size = state.page_aligned_size(size)?;
        let (segment_vmar_slot, base, span) = invoker
            .allocate_child_vmar_any(state.heap_vmar_slot, aligned_size)
            .map_err(HeapAllocatorError::Syscall)?;
        let end = base
            .checked_add(span)
            .ok_or(HeapAllocatorError::InvalidHeapWindow)?;
        if base < state.heap_base
            || end > state.heap_limit
            || span < aligned_size
            || !base.is_multiple_of(state.page_size)
            || !span.is_multiple_of(state.page_size)
        {
            let segment = UserHeapSegment {
                base,
                mapped_size: 0,
                reserved_size: span,
                vmar_slot: segment_vmar_slot,
                vmo_slot: 0,
            };
            let _ = Self::tear_down_segment_locked(state, segment);
            return Err(HeapAllocatorError::InvalidHeapWindow);
        }

        let create_flags = VmFlags::READ | VmFlags::WRITE | VmFlags::USER | VmFlags::MAP;
        let vmo_slot = match invoker.create_paged_vmo(span, state.page_size, create_flags) {
            Ok(slot) => slot,
            Err(err) => {
                let segment = UserHeapSegment {
                    base,
                    mapped_size: 0,
                    reserved_size: span,
                    vmar_slot: segment_vmar_slot,
                    vmo_slot: 0,
                };
                let _ = Self::tear_down_segment_locked(state, segment);
                return Err(HeapAllocatorError::Syscall(err));
            }
        };

        let map_flags = VmFlags::READ | VmFlags::WRITE | VmFlags::USER;
        let (mapped_base, mapped_len) = match invoker.map_vmo(
            segment_vmar_slot,
            vmo_slot,
            base,
            span,
            0,
            map_flags,
            RegionPurpose::User,
        ) {
            Ok(mapping) => mapping,
            Err(err) => {
                let segment = UserHeapSegment {
                    base,
                    mapped_size: 0,
                    reserved_size: span,
                    vmar_slot: segment_vmar_slot,
                    vmo_slot,
                };
                let _ = Self::tear_down_segment_locked(state, segment);
                return Err(HeapAllocatorError::Syscall(err));
            }
        };
        if mapped_base != base || mapped_len != span {
            let segment = UserHeapSegment {
                base,
                mapped_size: span,
                reserved_size: span,
                vmar_slot: segment_vmar_slot,
                vmo_slot,
            };
            let _ = Self::tear_down_segment_locked(state, segment);
            return Err(HeapAllocatorError::UnexpectedMapping);
        }

        let segment = UserHeapSegment {
            base,
            mapped_size: span,
            reserved_size: span,
            vmar_slot: segment_vmar_slot,
            vmo_slot,
        };
        if let Err(err) = state.insert_segment(segment) {
            let _ = Self::tear_down_segment_locked(state, segment);
            return Err(err);
        }

        Ok((base as *mut u8, span))
    }

    fn restore_segment_mapping_locked(
        segment: UserHeapSegment,
        mapping_size: usize,
    ) -> Result<(), HeapAllocatorError> {
        let invoker = RawSyscallInvoker;
        let map_flags = VmFlags::READ | VmFlags::WRITE | VmFlags::USER;
        let (mapped_base, mapped_len) = invoker
            .map_vmo(
                segment.vmar_slot,
                segment.vmo_slot,
                segment.base,
                mapping_size,
                0,
                map_flags,
                RegionPurpose::User,
            )
            .map_err(HeapAllocatorError::Syscall)?;
        if mapped_base != segment.base || mapped_len != mapping_size {
            return Err(HeapAllocatorError::UnexpectedMapping);
        }
        Ok(())
    }

    fn resize_segment_in_place_locked(
        state: &mut UserHeapState,
        index: usize,
        requested_size: usize,
        release_tail: bool,
    ) -> Result<*mut u8, HeapAllocatorError> {
        let invoker = RawSyscallInvoker;
        let mut segment = state.segments[index].ok_or(HeapAllocatorError::SegmentNotFound)?;
        let aligned_size = state.page_aligned_size(requested_size)?;
        if aligned_size > segment.reserved_size {
            return Err(HeapAllocatorError::InvalidHeapWindow);
        }
        if aligned_size == segment.mapped_size {
            return Ok(segment.base as *mut u8);
        }

        let unmapped_len = invoker
            .unmap_vmar(segment.vmar_slot, segment.base)
            .map_err(HeapAllocatorError::Syscall)?;
        if unmapped_len != segment.mapped_size {
            return Err(HeapAllocatorError::UnexpectedMapping);
        }

        if let Err(err) = Self::restore_segment_mapping_locked(segment, aligned_size) {
            let _ = Self::restore_segment_mapping_locked(segment, segment.mapped_size);
            return Err(err);
        }

        if release_tail && aligned_size < segment.mapped_size {
            let released_len = segment.mapped_size - aligned_size;
            if let Err(err) = invoker.vmo_op_range(
                segment.vmo_slot,
                VmoOpRangeOperation::Decommit,
                aligned_size,
                released_len,
            ) {
                let _ = invoker.unmap_vmar(segment.vmar_slot, segment.base);
                let _ = Self::restore_segment_mapping_locked(segment, segment.mapped_size);
                return Err(HeapAllocatorError::Syscall(err));
            }
        }

        segment.mapped_size = aligned_size;
        state.segments[index] = Some(segment);
        Ok(segment.base as *mut u8)
    }

    fn release_segment_locked(
        state: &mut UserHeapState,
        ptr: *mut u8,
    ) -> Result<bool, HeapAllocatorError> {
        if ptr.is_null() {
            return Ok(true);
        }

        let index = state.find_segment_index(ptr as usize)?;
        let segment = state.segments[index].ok_or(HeapAllocatorError::SegmentNotFound)?;
        Self::tear_down_segment_locked(state, segment)?;
        state.segments[index] = None;
        state.segment_count = state.segment_count.saturating_sub(1);
        Ok(true)
    }

    fn allocate_segment(&self, size: usize) -> Result<(*mut u8, usize, u32), HeapAllocatorError> {
        let mut state = self.state.lock();
        let (ptr, actual_size) = Self::allocate_segment_locked(&mut state, size)?;
        Ok((ptr, actual_size, 0))
    }

    fn remap_segment(
        &self,
        ptr: *mut u8,
        oldsize: usize,
        newsize: usize,
        can_move: bool,
    ) -> Result<*mut u8, HeapAllocatorError> {
        let mut state = self.state.lock();
        if ptr.is_null() {
            let (allocated, _) = Self::allocate_segment_locked(&mut state, newsize)?;
            return Ok(allocated);
        }

        let index = state.find_segment_index(ptr as usize)?;
        let segment = state.segments[index].ok_or(HeapAllocatorError::SegmentNotFound)?;
        let aligned_new = state.page_aligned_size(newsize)?;
        if aligned_new <= segment.reserved_size {
            let release_tail = aligned_new < segment.mapped_size;
            return Self::resize_segment_in_place_locked(&mut state, index, newsize, release_tail);
        }
        if !can_move {
            return Err(HeapAllocatorError::InvalidHeapWindow);
        }

        let old_bytes = cmp::min(oldsize, newsize);
        let (new_ptr, _) = Self::allocate_segment_locked(&mut state, newsize)?;
        unsafe {
            ptr::copy_nonoverlapping(ptr, new_ptr, old_bytes);
        }
        if Self::release_segment_locked(&mut state, ptr).is_err() {
            let _ = Self::release_segment_locked(&mut state, new_ptr);
            return Err(HeapAllocatorError::InvalidHeapWindow);
        }

        Ok(new_ptr)
    }

    fn release_segment_tail(
        &self,
        ptr: *mut u8,
        _oldsize: usize,
        newsize: usize,
    ) -> Result<bool, HeapAllocatorError> {
        let mut state = self.state.lock();
        let index = state.find_segment_index(ptr as usize)?;
        let segment = state.segments[index].ok_or(HeapAllocatorError::SegmentNotFound)?;
        let aligned_new = state.page_aligned_size(newsize)?;
        if aligned_new >= segment.mapped_size {
            return Ok(false);
        }

        Self::resize_segment_in_place_locked(&mut state, index, newsize, true)?;
        Ok(true)
    }

    fn free_segment(&self, ptr: *mut u8) -> Result<bool, HeapAllocatorError> {
        let mut state = self.state.lock();
        Self::release_segment_locked(&mut state, ptr)
    }
}

unsafe impl DlmallocAllocator for UserHeapSystem {
    fn alloc(&self, size: usize) -> (*mut u8, usize, u32) {
        match self.allocate_segment(size) {
            Ok(result) => result,
            Err(_) => (ptr::null_mut(), 0, 0),
        }
    }

    fn remap(&self, ptr: *mut u8, oldsize: usize, newsize: usize, can_move: bool) -> *mut u8 {
        match self.remap_segment(ptr, oldsize, newsize, can_move) {
            Ok(result) => result,
            Err(_) => ptr::null_mut(),
        }
    }

    fn free_part(&self, ptr: *mut u8, oldsize: usize, newsize: usize) -> bool {
        match self.release_segment_tail(ptr, oldsize, newsize) {
            Ok(result) => result,
            Err(_) => false,
        }
    }

    fn free(&self, ptr: *mut u8, _size: usize) -> bool {
        match self.free_segment(ptr) {
            Ok(result) => result,
            Err(_) => false,
        }
    }

    fn can_release_part(&self, _flags: u32) -> bool {
        true
    }

    fn allocates_zeros(&self) -> bool {
        false
    }

    fn page_size(&self) -> usize {
        let state = self.state.lock();
        if state.page_size != 0 {
            return state.page_size;
        }

        PAGE_SIZE
    }
}

/// dlmalloc-backed global allocator used by early Akarin userspace programs.
pub struct RuntimeDlmalloc {
    inner: Mutex<Dlmalloc<UserHeapSystem>>,
}

#[global_allocator]
pub static GLOBAL_ALLOCATOR: RuntimeDlmalloc = RuntimeDlmalloc {
    inner: Mutex::new(Dlmalloc::new_with_allocator(UserHeapSystem::new())),
};

impl RuntimeDlmalloc {
    /// Seed the allocator backend with the heap window passed by the kernel.
    pub fn initialize(&self, startup: &StartupInfo) -> Result<(), HeapAllocatorError> {
        let mut allocator = self.inner.lock();
        allocator.allocator_mut().initialize(startup)
    }

    /// Ask dlmalloc to release trailing free pages from the current heap back
    /// to the kernel while leaving `pad` bytes available on the top chunk.
    pub fn trim(&self, pad: usize) -> bool {
        let mut allocator = self.inner.lock();
        unsafe { allocator.trim(pad) }
    }
}

unsafe impl GlobalAlloc for RuntimeDlmalloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut allocator = self.inner.lock();
        unsafe { allocator.malloc(layout.size(), layout.align()) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let mut allocator = self.inner.lock();
        unsafe {
            allocator.free(ptr, layout.size(), layout.align());
        }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let mut allocator = self.inner.lock();
        unsafe { allocator.calloc(layout.size(), layout.align()) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let mut allocator = self.inner.lock();
        unsafe { allocator.realloc(ptr, layout.size(), layout.align(), new_size) }
    }
}
