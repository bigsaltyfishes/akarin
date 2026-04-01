use core::{alloc::Layout, mem};

use libakarin_boot_proto::memory::{Arena, ArenaKind};
use libakarin_machine_core::memory::{AddressSpaceTrait, PhysAddr, VirtAddr};

use crate::arch::Machine;

/// Low memory threshold (64KB).
/// Memory below this is excluded to preserve BIOS/real-mode data.
pub const LOW_MEM_THRESHOLD: usize = 0x10000;

/// Intrusive free region node.
/// Stored at the start of each free memory block.
#[repr(C)]
pub struct FreeRegion {
    /// Next region (lower address), null if none.
    next: *mut FreeRegion,
    /// Size of this region in bytes (including this header).
    size: usize,
}

impl FreeRegion {
    /// Initialize a free region at the given virtual address.
    ///
    /// # Safety
    /// `addr` must be valid, aligned, and have at least `size` bytes available.
    #[inline]
    unsafe fn init(addr: usize, size: usize) -> *mut Self {
        let ptr = addr as *mut FreeRegion;
        unsafe {
            (*ptr).next = core::ptr::null_mut();
            (*ptr).size = size;
        }
        ptr
    }

    /// Get the end address of this region.
    #[inline]
    fn end(&self) -> usize {
        (self as *const _ as usize) + self.size
    }

    /// Get the start address of this region.
    #[inline]
    fn start(&self) -> usize {
        self as *const _ as usize
    }
}

/// Early boot first-fit allocator.
///
/// Maintains a linked list of free regions sorted by address (highest first).
/// Allocates from the end of the highest suitable region.
pub struct EarlyAllocator {
    /// Head of free list (highest address first).
    head: *mut FreeRegion,
}

unsafe impl Send for EarlyAllocator {}

impl EarlyAllocator {
    #[inline]
    pub const fn new() -> Self {
        Self {
            head: core::ptr::null_mut(),
        }
    }

    pub unsafe fn init_from_map<I>(&mut self, map: I)
    where
        I: IntoIterator<Item = Arena>,
    {
        for arena in map {
            if arena.kind == ArenaKind::Usable {
                unsafe { self.add_region(arena.start, arena.end - arena.start) };
            }
        }
    }

    pub unsafe fn add_region(&mut self, phys_start: usize, size: usize) {
        let (start, size) = if phys_start < LOW_MEM_THRESHOLD {
            let end = phys_start + size;
            if end <= LOW_MEM_THRESHOLD {
                return;
            }
            (LOW_MEM_THRESHOLD, end - LOW_MEM_THRESHOLD)
        } else {
            (phys_start, size)
        };

        let align = mem::align_of::<FreeRegion>();
        let aligned_start = (start + align - 1) & !(align - 1);
        let end = start + size;
        if aligned_start >= end {
            return;
        }

        let aligned_size = end - aligned_start;
        if aligned_size < mem::size_of::<FreeRegion>() {
            return;
        }

        let virt_addr = Self::phys_to_virt(aligned_start);
        let new_node = unsafe { FreeRegion::init(virt_addr, aligned_size) };
        unsafe { self.insert_node(new_node) };
    }

    pub fn alloc(&mut self, layout: Layout) -> Option<usize> {
        let size = layout.size().max(1);
        let align = layout.align().max(mem::align_of::<usize>());

        let mut prev: *mut FreeRegion = core::ptr::null_mut();
        let mut curr = self.head;

        while !curr.is_null() {
            let region = unsafe { &mut *curr };
            let region_start = region.start();
            let region_end = region.end();

            // 修复 2：如果当前内存块空间不足，跳过并检查下一个节点，而不是直接退出函数
            let alloc_start_unaligned = match region_end.checked_sub(size) {
                Some(addr) => addr,
                None => {
                    prev = curr;
                    curr = region.next;
                    continue;
                }
            };

            // 向下对齐到 layout 要求的边界
            let alloc_start = alloc_start_unaligned & !(align - 1);

            // 确保对齐后的起始地址依然没有超出当前内存块的下界
            if alloc_start >= region_start {
                let leftover = alloc_start - region_start;

                if leftover < mem::size_of::<FreeRegion>() {
                    // 剩余空间不足以存下 FreeRegion 的 header，将整块区域从空闲链表中摘除
                    if prev.is_null() {
                        self.head = region.next;
                    } else {
                        unsafe { (*prev).next = region.next };
                    }
                    // 修复 1：移除了修改 alloc_start 的逻辑。
                    // region_start 到 alloc_start 之间产生的零星字节作为对齐
                    // padding 被舍弃，
                    // 从而保证返回的 alloc_start 绝对符合 Layout 规范。
                } else {
                    // 剩余空间足够，缩小当前空闲块的边界
                    region.size = leftover;
                }

                return Some(Self::virt_to_phys(alloc_start));
            }

            prev = curr;
            curr = region.next;
        }

        None
    }

    pub fn iter_regions(&self) -> FreeRegionIter<'_> {
        FreeRegionIter {
            current: self.head,
            _marker: core::marker::PhantomData,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.head.is_null()
    }

    fn phys_to_virt(phys: usize) -> usize {
        Machine::phys_to_virt(PhysAddr::new(phys))
            .expect("invalid physical address")
            .as_usize()
    }

    fn virt_to_phys(virt: usize) -> usize {
        Machine::virt_to_phys(VirtAddr::new(virt))
            .expect("invalid virtual address")
            .as_usize()
    }

    unsafe fn insert_node(&mut self, new_node: *mut FreeRegion) {
        let new_addr = new_node as usize;
        if self.head.is_null() || new_addr > self.head as usize {
            unsafe { (*new_node).next = self.head };
            self.head = new_node;
            return;
        }

        let mut prev = self.head;
        unsafe {
            while !(*prev).next.is_null() && ((*prev).next as usize) > new_addr {
                prev = (*prev).next;
            }
            (*new_node).next = (*prev).next;
            (*prev).next = new_node;
        }
    }
}

pub struct FreeRegionIter<'a> {
    current: *mut FreeRegion,
    _marker: core::marker::PhantomData<&'a EarlyAllocator>,
}

impl Iterator for FreeRegionIter<'_> {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<Self::Item> {
        if self.current.is_null() {
            return None;
        }
        let region = unsafe { &*self.current };
        let phys_start = Machine::virt_to_phys(VirtAddr::new(region.start()))
            .expect("invalid virtual address in early free list")
            .as_usize();
        let size = region.size;
        self.current = region.next;
        Some((phys_start, size))
    }
}
