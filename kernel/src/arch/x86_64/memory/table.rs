use libakarin_machine_core::memory::{
    self, AddressSpaceTrait, FrameAllocatorTrait, FrameZone, PageSizeTrait, PageTrait, PhysAddr,
    PhysFrame, VirtAddr,
    paging::{
        CachePolicy, MMUFlags, PageTableEntryTrait, PageTableTrait, PagingError, PagingResult,
        PhysFrameTrait, TlbInvalidator,
    },
};
use x86_64::structures::paging::PageTableFlags;

use super::page::{Page, PageSize};

const PHYS_ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

trait Convert<T> {
    fn convert(&self) -> T;
}

impl Convert<MMUFlags> for PageTableFlags {
    fn convert(&self) -> MMUFlags {
        let mut flags = MMUFlags::empty();
        if self.contains(PageTableFlags::WRITABLE) {
            flags |= MMUFlags::WRITE;
        }
        if !self.contains(PageTableFlags::NO_EXECUTE) {
            flags |= MMUFlags::EXECUTE;
        }
        if self.contains(PageTableFlags::USER_ACCESSIBLE) {
            flags |= MMUFlags::USER;
        }
        if self.contains(PageTableFlags::HUGE_PAGE) {
            flags |= MMUFlags::HUGE_PAGE;
        }
        if self.contains(PageTableFlags::GLOBAL) {
            flags |= MMUFlags::GLOBAL;
        }
        if self.contains(PageTableFlags::NO_CACHE | PageTableFlags::WRITE_THROUGH) {
            flags |= MMUFlags::CACHE_1;
        }
        flags
    }
}

impl Convert<PageTableFlags> for MMUFlags {
    fn convert(&self) -> PageTableFlags {
        if self.is_empty() {
            return PageTableFlags::empty();
        }
        let mut flags = PageTableFlags::PRESENT;
        if self.contains(MMUFlags::WRITE) {
            flags |= PageTableFlags::WRITABLE;
        }
        if !self.contains(MMUFlags::EXECUTE) {
            flags |= PageTableFlags::NO_EXECUTE;
        }
        if self.contains(MMUFlags::USER) {
            flags |= PageTableFlags::USER_ACCESSIBLE;
        }
        if self.contains(MMUFlags::HUGE_PAGE) {
            flags |= PageTableFlags::HUGE_PAGE;
        }
        if self.contains(MMUFlags::GLOBAL) {
            flags |= PageTableFlags::GLOBAL;
        }
        match self.cache_policy() {
            CachePolicy::Cached => {
                flags.remove(PageTableFlags::WRITE_THROUGH);
            }
            CachePolicy::Uncached | CachePolicy::UncachedDevice | CachePolicy::WriteCombining => {
                flags |= PageTableFlags::NO_CACHE | PageTableFlags::WRITE_THROUGH;
            }
        }
        flags
    }
}

impl Convert<PageSize> for usize {
    fn convert(&self) -> PageSize {
        match *self {
            0x1000 => PageSize::Size4K,
            0x20_0000 => PageSize::Size2M,
            0x4000_0000 => PageSize::Size1G,
            _ => unreachable!("invalid page size"),
        }
    }
}

#[derive(Debug, Clone, Copy)]
#[repr(transparent)]
pub struct PageTableEntry(u64);

impl PageTableEntry {
    fn raw_addr(&self) -> PhysAddr {
        PhysAddr::new((self.0 & PHYS_ADDR_MASK) as usize)
    }

    fn raw_flags(&self) -> PageTableFlags {
        PageTableFlags::from_bits_truncate(self.0)
    }

    fn set_table(&mut self, phys_addr: PhysAddr) {
        self.0 = (phys_addr.as_usize() as u64 & PHYS_ADDR_MASK)
            | (PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::USER_ACCESSIBLE)
                .bits();
    }
}

impl PageTableEntryTrait for PageTableEntry {
    fn phys_addr(&self) -> PhysAddr {
        self.raw_addr()
    }

    fn flags(&self) -> MMUFlags {
        self.raw_flags().convert()
    }

    fn cache_policy(&self) -> CachePolicy {
        self.flags().cache_policy()
    }

    fn is_unused(&self) -> bool {
        self.0 == 0
    }

    fn is_present(&self) -> bool {
        self.raw_flags().contains(PageTableFlags::PRESENT)
    }

    fn is_leaf(&self) -> bool {
        self.raw_flags().contains(PageTableFlags::HUGE_PAGE)
    }

    fn set_flags(&mut self, flags: MMUFlags) {
        let phys = self.raw_addr().as_usize() as u64 & PHYS_ADDR_MASK;
        self.0 = phys | flags.convert().bits();
    }

    fn set_cache_policy(&mut self, policy: CachePolicy) {
        let mut flags = self.flags();
        flags.set_cache_policy(policy);
        self.set_flags(flags);
    }

    fn set_phys_addr(&mut self, phys_addr: PhysAddr, flags: MMUFlags) {
        self.0 = (phys_addr.as_usize() as u64 & PHYS_ADDR_MASK) | flags.convert().bits();
    }

    fn clear(&mut self) {
        self.0 = 0;
    }
}

pub struct PageTable<A: AddressSpaceTrait> {
    root_phys: PhysAddr,
    allocator: &'static dyn FrameAllocatorTrait,
    _marker: core::marker::PhantomData<A>,
}

impl<A> PageTable<A>
where
    A: memory::AddressSpaceTrait<
            Page = Page,
            PageSize = PageSize,
            PageTableEntry = PageTableEntry,
            PhysFrame = PhysFrame<A>,
        >,
{
    pub fn from_active(allocator: &'static dyn FrameAllocatorTrait) -> Self {
        let root_virt =
            A::phys_to_virt(A::current_base()).expect("active page table root must be mapped");
        Self::from_raw(allocator, root_virt.as_mut_ptr())
    }

    #[inline]
    fn indices(virt: VirtAddr) -> [usize; 4] {
        let va = virt.as_usize();
        [
            (va >> 39) & 0x1ff,
            (va >> 30) & 0x1ff,
            (va >> 21) & 0x1ff,
            (va >> 12) & 0x1ff,
        ]
    }

    #[inline]
    fn level_of(size: PageSize) -> usize {
        match size {
            PageSize::Size1G => 3,
            PageSize::Size2M => 2,
            PageSize::Size4K => 1,
        }
    }

    #[inline]
    fn page_size_of_level(level: usize) -> PageSize {
        (1usize << (12 + (level - 1) * 9)).convert()
    }

    fn table_ptr_from_phys(phys: PhysAddr) -> PagingResult<*mut PageTableEntry> {
        let virt = A::phys_to_virt(phys).ok_or(PagingError::InvalidFrameAddress)?;
        Ok(virt.as_mut_ptr() as *mut PageTableEntry)
    }

    unsafe fn entry_ptr(table_phys: PhysAddr, index: usize) -> PagingResult<*mut PageTableEntry> {
        let table_ptr = Self::table_ptr_from_phys(table_phys)?;
        Ok(unsafe { table_ptr.add(index) })
    }

    fn alloc_table(&self) -> PagingResult<PhysAddr> {
        let virt = self
            .allocator
            .alloc(None, FrameZone::default(), 1)
            .map_err(|_| PagingError::NoMemory)?;
        let phys = A::virt_to_phys(virt).ok_or(PagingError::InvalidFrameAddress)?;
        unsafe { core::ptr::write_bytes(virt.as_mut_ptr(), 0, 0x1000) };
        Ok(phys)
    }

    unsafe fn walk_to_leaf_mut(
        &mut self,
        virt: VirtAddr,
        target_level: Option<usize>,
        create: bool,
    ) -> PagingResult<(*mut PageTableEntry, PageSize)> {
        let idx = Self::indices(virt);
        let mut table_phys = self.root_phys;

        for level in (1..=4).rev() {
            let index = idx[4 - level];
            let entry_ptr = unsafe { Self::entry_ptr(table_phys, index)? };
            let entry = unsafe { &mut *entry_ptr };

            // For map path: once we reach target level, return the entry even if
            // it's currently unused. Caller decides whether this is an overwrite.
            if let Some(target) = target_level
                && level == target
            {
                return Ok((entry_ptr, Self::page_size_of_level(level)));
            }

            if entry.is_unused() {
                if create {
                    let new_table = self.alloc_table()?;
                    entry.set_table(new_table);
                } else {
                    return Err(PagingError::NotMapped);
                }
            }

            if !entry.is_present() {
                return Err(PagingError::NotMapped);
            }

            if target_level.is_some() {
                if entry.is_leaf() {
                    return Err(PagingError::ParentIsHugePage);
                }
            } else {
                if level == 1 {
                    return Ok((entry_ptr, PageSize::Size4K));
                }
                if entry.is_leaf() {
                    return Ok((entry_ptr, Self::page_size_of_level(level)));
                }
            }

            table_phys = entry.phys_addr();
        }

        Err(PagingError::NotMapped)
    }
}

impl<A> PageTableTrait<A, PageSize, Page, PageTableEntry> for PageTable<A>
where
    A: memory::AddressSpaceTrait<
            Page = Page,
            PageSize = PageSize,
            PageTableEntry = PageTableEntry,
            PhysFrame = PhysFrame<A>,
        >,
{
    fn empty(allocator: &'static dyn FrameAllocatorTrait) -> Self {
        let virt = allocator
            .alloc(None, FrameZone::default(), 1)
            .map_err(|_| PagingError::NoMemory)
            .expect("Failed to allocate frame for page table root");
        let phys = A::virt_to_phys(virt)
            .expect("Failed to convert page table root virtual address to physical address");
        unsafe { core::ptr::write_bytes(virt.as_mut_ptr(), 0, 0x1000) };
        Self {
            root_phys: phys,
            allocator,
            _marker: core::marker::PhantomData,
        }
    }

    fn from_raw(allocator: &'static dyn FrameAllocatorTrait, root_ptr: *mut u8) -> Self {
        let root_phys = A::virt_to_phys(VirtAddr::new(root_ptr as usize))
            .expect("Failed to convert page table root pointer to physical address");
        Self {
            root_phys,
            allocator,
            _marker: core::marker::PhantomData,
        }
    }

    fn phys_addr(&self) -> PhysAddr {
        self.root_phys
    }

    unsafe fn map(
        &mut self,
        page: Page,
        frame: <A as AddressSpaceTrait>::PhysFrame,
        flags: MMUFlags,
        cache: CachePolicy,
    ) -> PagingResult<TlbInvalidator<A>> {
        let mut final_flags = flags;
        final_flags.set_cache_policy(cache);
        if !matches!(page.size(), PageSize::Size4K) {
            final_flags |= MMUFlags::HUGE_PAGE;
        } else {
            final_flags.remove(MMUFlags::HUGE_PAGE);
        }

        let (entry_ptr, _) = unsafe {
            self.walk_to_leaf_mut(page.virt_addr(), Some(Self::level_of(page.size())), true)?
        };
        let entry = unsafe { &mut *entry_ptr };
        if !entry.is_unused() {
            return Err(PagingError::AlreadyMapped);
        }
        entry.set_phys_addr(frame.phys_addr(), final_flags);

        Ok(TlbInvalidator::new(
            false,
            Some((page.virt_addr(), page.virt_addr() + page.size().size())),
            page.size(),
        ))
    }

    unsafe fn unmap(
        &mut self,
        page: Page,
    ) -> PagingResult<(<A as AddressSpaceTrait>::PhysFrame, TlbInvalidator<A>)> {
        let (entry_ptr, actual_size) =
            unsafe { self.walk_to_leaf_mut(page.virt_addr(), None, false)? };
        let entry = unsafe { &mut *entry_ptr };
        if entry.is_unused() {
            return Err(PagingError::NotMapped);
        }
        let phys = entry.phys_addr();
        let num = actual_size.size() / <PageSize as PageSizeTrait>::UNIT_PAGE_SIZE;
        entry.clear();

        let frame = unsafe { PhysFrame::<A>::from_addr(Some(self.allocator), phys, num) };
        Ok((
            frame,
            TlbInvalidator::new(
                false,
                Some((page.virt_addr(), page.virt_addr() + actual_size.size())),
                actual_size,
            ),
        ))
    }

    fn entry(&self, page: Page) -> PagingResult<(&PageTableEntry, PageSize)> {
        let this = self as *const Self as *mut Self;
        let (entry_ptr, size) = unsafe { (*this).walk_to_leaf_mut(page.virt_addr(), None, false)? };
        Ok((unsafe { &*entry_ptr }, size))
    }

    fn entry_mut(&mut self, page: Page) -> PagingResult<(&mut PageTableEntry, PageSize)> {
        let (entry_ptr, size) = unsafe { self.walk_to_leaf_mut(page.virt_addr(), None, false)? };
        Ok((unsafe { &mut *entry_ptr }, size))
    }

    fn query(&self, page: Page) -> PagingResult<(PhysAddr, MMUFlags, CachePolicy, PageSize)> {
        let (entry, size) = self.entry(page)?;
        if !entry.is_present() {
            return Err(PagingError::NotMapped);
        }
        Ok((entry.phys_addr(), entry.flags(), entry.cache_policy(), size))
    }
}
