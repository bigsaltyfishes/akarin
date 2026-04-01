use alloc::boxed::Box;
use core::{fmt::Debug, ptr::NonNull};

use bitflags::bitflags;
use libakarin_macros::abstraction;
use numeric_enum_macro::numeric_enum;

use crate::memory::{
    AddressSpaceTrait, AllocationError, FrameAllocatorTrait, FrameZone, PhysAddr, VirtAddr,
};

pub struct PhysFrame<A>
where
    A: AddressSpaceTrait,
{
    addr: PhysAddr,
    number: usize,
    allocator: Option<&'static dyn FrameAllocatorTrait>,
    _marker: core::marker::PhantomData<A>,
}

#[abstraction(PhysFrameTrait, visibility = "public")]
impl<A> PhysFrameTrait for PhysFrame<A>
where
    Self: Sized,
    A: AddressSpaceTrait,
{
    fn new(
        allocator: &'static dyn FrameAllocatorTrait,
        addr: Option<PhysAddr>,
        prefer_zone: FrameZone,
    ) -> Result<Self, AllocationError> {
        Self::new_contiguous(allocator, addr, prefer_zone, 1)
    }

    /// Allocate a new contiguous physical frame of `num` pages.
    ///
    /// The `num` will be rounded up to the next power of two.
    fn new_contiguous(
        allocator: &'static dyn FrameAllocatorTrait,
        addr: Option<PhysAddr>,
        prefer_zone: FrameZone,
        num: usize,
    ) -> Result<Self, AllocationError> {
        assert_eq!(
            allocator.unit_page_size(),
            <A::PageSize as PageSizeTrait>::UNIT_PAGE_SIZE,
            "The unit page size of the allocator must match the page size used in the address \
             space."
        );
        let num = num.next_power_of_two();
        allocator.alloc(addr, prefer_zone, num).map(|virt_addr| {
            let phys_addr =
                A::virt_to_phys(virt_addr).expect("Allocated frame must have a physical address");
            PhysFrame {
                addr: phys_addr,
                number: num,
                allocator: Some(allocator),
                _marker: core::marker::PhantomData,
            }
        })
    }

    /// Create a physical frame from a given physical address and number of
    /// pages.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the provided physical address is valid and
    /// that the number of pages is a power of two. Only provide allocator if
    /// the frame is allocated by the kernel and should be automatically freed
    /// when dropped.
    unsafe fn from_addr(
        allocator: Option<&'static dyn FrameAllocatorTrait>,
        addr: PhysAddr,
        num: usize,
    ) -> Self {
        assert!(
            num.is_power_of_two(),
            "Number of pages must be a power of two"
        );
        PhysFrame {
            addr,
            number: num,
            allocator,
            _marker: core::marker::PhantomData,
        }
    }

    /// Consume `num` pages from the frame, returning the starting physical
    /// address.
    ///
    /// Returns `None` if there are not enough pages left.
    fn consume(&mut self, num: usize) -> Option<PhysAddr> {
        if num > self.number {
            return None;
        }
        let current_addr = self.addr;
        self.addr += num * <A::PageSize as PageSizeTrait>::UNIT_PAGE_SIZE;
        self.number -= num;
        Some(current_addr)
    }

    /// Get the number of pages in this frame.
    #[inline]
    fn number(&self) -> usize {
        self.number
    }

    /// Get the physical address of this frame.
    #[inline]
    fn phys_addr(&self) -> PhysAddr {
        self.addr
    }
}

impl<A> Drop for PhysFrame<A>
where
    A: AddressSpaceTrait,
{
    fn drop(&mut self) {
        unsafe {
            match (self.allocator, self.number) {
                (Some(allocator), num) if num > 0 => {
                    let frame_addr = A::phys_to_virt(self.addr)
                        .expect("Failed to convert `PhysAddr` to `VirtAddr`.");
                    allocator.dealloc(frame_addr, num);
                }
                _ => {}
            }
        }
    }
}

impl<A> Debug for PhysFrame<A>
where
    A: AddressSpaceTrait,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PhysFrame")
            .field("addr", &self.addr)
            .field("number", &self.number)
            .field("allocate_by_kernel", &self.allocator.is_some())
            .finish()
    }
}

struct PhysRunVTable {
    drop_fn: Option<unsafe fn(*mut ())>,
    read_fn: unsafe fn(PhysAddr, &mut [u8]),
    write_fn: unsafe fn(PhysAddr, &[u8]),
}

/// Type-erased physical frame/run with RAII semantics preserved.
///
/// Instances created with [`PhysRun::from_frame`] own an underlying
/// [`PhysFrame`] and will return it to the originating frame allocator when
/// dropped. Borrowed runs keep only the physical range and ISA-specific access
/// functions.
pub struct PhysRun {
    ptr: Option<NonNull<()>>,
    phys_addr: PhysAddr,
    number: usize,
    unit_page_size: usize,
    vtable: &'static PhysRunVTable,
}

unsafe impl Send for PhysRun {}
unsafe impl Sync for PhysRun {}

impl PhysRun {
    /// Erase one owned [`PhysFrame`] while preserving its drop behavior.
    pub fn from_frame<A>(frame: PhysFrame<A>) -> Self
    where
        A: AddressSpaceTrait,
    {
        let phys_addr = frame.addr;
        let number = frame.number;
        let raw = Box::into_raw(Box::new(frame)) as *mut ();
        Self {
            ptr: Some(NonNull::new(raw).expect("boxed frame pointer must not be null")),
            phys_addr,
            number,
            unit_page_size: <A::PageSize as PageSizeTrait>::UNIT_PAGE_SIZE,
            vtable: &PhysRunVTable {
                drop_fn: Some(drop_phys_frame::<A>),
                read_fn: A::read_phys,
                write_fn: A::write_phys,
            },
        }
    }

    /// Build one borrowed physical run.
    pub fn borrowed<A>(phys_addr: PhysAddr, number: usize) -> Self
    where
        A: AddressSpaceTrait,
    {
        Self {
            ptr: None,
            phys_addr,
            number,
            unit_page_size: <A::PageSize as PageSizeTrait>::UNIT_PAGE_SIZE,
            vtable: &PhysRunVTable {
                drop_fn: None,
                read_fn: A::read_phys,
                write_fn: A::write_phys,
            },
        }
    }

    /// Return the first physical address covered by this run.
    pub fn phys_addr(&self) -> PhysAddr {
        self.phys_addr
    }

    /// Return the number of pages in this run.
    pub fn number(&self) -> usize {
        self.number
    }

    /// Return the unit page size used by the underlying address-space
    /// implementation.
    pub fn unit_page_size(&self) -> usize {
        self.unit_page_size
    }

    /// Return the total byte size covered by this run.
    pub fn len_bytes(&self) -> usize {
        self.number * self.unit_page_size
    }

    /// Read bytes from this physical run.
    ///
    /// Returns `false` when the requested byte range exceeds the run.
    pub unsafe fn read(&self, offset: usize, buffer: &mut [u8]) -> bool {
        let Some(end) = offset.checked_add(buffer.len()) else {
            return false;
        };
        if end > self.len_bytes() {
            return false;
        }
        unsafe { (self.vtable.read_fn)(self.phys_addr + offset, buffer) };
        true
    }

    /// Write bytes into this physical run.
    ///
    /// Returns `false` when the requested byte range exceeds the run.
    pub unsafe fn write(&self, offset: usize, data: &[u8]) -> bool {
        let Some(end) = offset.checked_add(data.len()) else {
            return false;
        };
        if end > self.len_bytes() {
            return false;
        }
        unsafe { (self.vtable.write_fn)(self.phys_addr + offset, data) };
        true
    }
}

impl Drop for PhysRun {
    fn drop(&mut self) {
        if let (Some(ptr), Some(drop_fn)) = (self.ptr.take(), self.vtable.drop_fn) {
            unsafe { drop_fn(ptr.as_ptr()) };
        }
    }
}

impl Debug for PhysRun {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PhysRun")
            .field("phys_addr", &self.phys_addr)
            .field("number", &self.number)
            .field("unit_page_size", &self.unit_page_size)
            .field("owned", &self.ptr.is_some())
            .finish()
    }
}

unsafe fn drop_phys_frame<A>(ptr: *mut ())
where
    A: AddressSpaceTrait,
{
    unsafe {
        drop(Box::from_raw(ptr as *mut PhysFrame<A>));
    }
}

/// TLB invalidation descriptor.
pub struct TlbInvalidator<A>
where
    A: AddressSpaceTrait,
{
    full: bool,
    range: Option<(VirtAddr, VirtAddr)>,
    page_size: A::PageSize,
}

impl<A> TlbInvalidator<A>
where
    A: AddressSpaceTrait,
{
    /// Create a TLB invalidator for a full TLB flush.
    pub fn new(full: bool, range: Option<(VirtAddr, VirtAddr)>, page_size: A::PageSize) -> Self {
        Self {
            full,
            range,
            page_size,
        }
    }

    /// Invalidate the TLB entries.
    pub fn invalidate(&self) {
        if self.full {
            A::flush_tlb();
        } else if let Some((start, end)) = self.range {
            let mut addr = start;
            while addr < end {
                A::invalidate_tlb(addr);
                addr += self.page_size.size(); // Assuming 4KiB pages for iteration
            }
        }
    }
}

#[derive(Debug)]
pub enum PagingError {
    NoMemory,
    NotMapped,
    AlreadyMapped,
    ParentIsHugePage,
    InvalidFrameAddress,
    UnsupportedPageSize,
}

pub type PagingResult<T = ()> = Result<T, PagingError>;

/// The [`PagingError::NotMapped`] can be ignored.
pub trait IgnoreNotMappedErr {
    /// If self is `Err(PagingError::NotMapped`, ignores the error and returns
    /// `Ok(())`, otherwise remain unchanged.
    fn ignore(self) -> PagingResult;
}

impl<T> IgnoreNotMappedErr for PagingResult<T> {
    fn ignore(self) -> PagingResult {
        match self {
            Ok(_) | Err(PagingError::NotMapped) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

bitflags! {
    /// Generic mem flags.
    #[derive(Debug, Copy, Clone, PartialEq, Eq)]
    pub struct MMUFlags: u16 {
        #[allow(clippy::identity_op)]
        // The first two bits are reserved for cache policy,
        // the rest are generic flags.
        const CACHE_1   = 1 << 0;
        const CACHE_2   = 1 << 1;

        const READ      = 1 << 2;
        const WRITE     = 1 << 3;
        const EXECUTE   = 1 << 4;
        const USER      = 1 << 5;
        const HUGE_PAGE = 1 << 6;
        const DEVICE    = 1 << 7;
        const GLOBAL    = 1 << 8;
        const RXW = Self::READ.bits() | Self::WRITE.bits() | Self::EXECUTE.bits();
    }
}

numeric_enum! {
    #[repr(u32)]
    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    /// Generic cache policy.
    pub enum CachePolicy {
        Cached = 0,
        Uncached = 1,
        UncachedDevice = 2,
        WriteCombining = 3,
    }
}

impl MMUFlags {
    /// Extract the cache policy from the MMU flags.
    pub fn cache_policy(&self) -> CachePolicy {
        match self.bits() & 0b11 {
            0 => CachePolicy::Cached,
            1 => CachePolicy::Uncached,
            2 => CachePolicy::UncachedDevice,
            3 => CachePolicy::WriteCombining,
            _ => unreachable!(),
        }
    }

    /// Set the cache policy in the MMU flags.
    pub fn set_cache_policy(&mut self, policy: CachePolicy) {
        *self = MMUFlags::from_bits_truncate((self.bits() & !0b11) | (policy as u16));
    }
}

/// Trait defining page size characteristics.
///
/// A `PageSize` can be a `enum` or a `struct` depending on the architecture.
pub trait PageSizeTrait {
    const UNIT_PAGE_SIZE: usize;

    /// Validate if the given size is a valid page size.
    fn validate(size: usize) -> bool;

    /// Get the size of the page.
    fn size(&self) -> usize;
}

pub trait PageTrait<S: PageSizeTrait> {
    /// Construct from a virtual address and page size.
    fn containing(virt_addr: VirtAddr, size: S) -> Self;

    /// Get the size of the page.
    fn size(&self) -> S;

    /// Get the virtual address of the page.
    fn virt_addr(&self) -> VirtAddr;
}

/// Trait defining a page table entry.
pub trait PageTableEntryTrait: Copy + Clone {
    /// Get the physical address mapped by this entry.
    fn phys_addr(&self) -> PhysAddr;
    /// Get the MMU flags associated with this entry.
    fn flags(&self) -> MMUFlags;
    /// Get the cache policy of this entry.
    fn cache_policy(&self) -> CachePolicy;
    /// Check if the entry is unused.
    fn is_unused(&self) -> bool;
    /// Check if the entry is present.
    fn is_present(&self) -> bool;
    /// Check if the entry is a leaf entry.
    fn is_leaf(&self) -> bool;
    /// Set the flags and leaf status for this entry.
    fn set_flags(&mut self, flags: MMUFlags);
    /// Set the cache policy for this entry.
    fn set_cache_policy(&mut self, policy: CachePolicy);
    /// Set the physical address for this entry.
    fn set_phys_addr(&mut self, phys_addr: PhysAddr, flags: MMUFlags);
    /// Set the physical address of the next level page table.
    fn set_table(&mut self, phys_addr: PhysAddr) {
        self.set_phys_addr(phys_addr, MMUFlags::WRITE | MMUFlags::USER);
    }
    /// Clear the entry.
    fn clear(&mut self);
}

/// Trait defining a page table with generic page size, page, and entry types.
pub trait PageTableTrait<A, S, P, E>
where
    A: AddressSpaceTrait,
    S: PageSizeTrait,
    P: PageTrait<S>,
    E: PageTableEntryTrait,
    Self: Sized,
{
    /// Create an empty page table.
    fn empty(allocator: &'static dyn FrameAllocatorTrait) -> Self;

    /// Create a page table from current active page table root.
    fn from_active(allocator: &'static dyn FrameAllocatorTrait) -> Self {
        let root_phys = A::current_base();
        Self::from_raw(
            allocator,
            A::phys_to_virt(root_phys)
                .expect("active table must be mapped")
                .as_mut_ptr(),
        )
    }

    /// Create a page table from a raw pointer to the root table.
    fn from_raw(allocator: &'static dyn FrameAllocatorTrait, root_ptr: *mut u8) -> Self;

    /// Get the physical address of the root table.
    fn phys_addr(&self) -> PhysAddr;

    /// Map a page to a physical frame with given flags and cache policy.
    ///
    /// # Safety
    ///
    /// The caller must ensure the TLB is invalidated after mapping to
    /// maintain consistency.
    unsafe fn map(
        &mut self,
        page: P,
        frame: A::PhysFrame,
        flags: MMUFlags,
        cache: CachePolicy,
    ) -> PagingResult<TlbInvalidator<A>>;

    /// Unmap a page.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the returned `PhysFrame` is correctly freed
    /// after use to avoid memory leaks.
    unsafe fn unmap(&mut self, page: P) -> PagingResult<(A::PhysFrame, TlbInvalidator<A>)>;

    /// Get a reference to the page table entry for a given page.
    fn entry(&self, page: P) -> PagingResult<(&E, S)>;

    /// Get a mutable reference to the page table entry for a given page.
    fn entry_mut(&mut self, page: P) -> PagingResult<(&mut E, S)>;

    /// Update entry for a mapped page.
    fn update<F>(&mut self, page: P, update_fn: F) -> PagingResult
    where
        F: FnOnce(&mut E, S),
    {
        match self.entry_mut(page) {
            Ok((entry, size)) => {
                update_fn(entry, size);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Query the mapping of a page.
    ///
    /// Returns the physical address, MMU flags, cache policy and page size if
    /// mapped.
    fn query(&self, page: P) -> PagingResult<(PhysAddr, MMUFlags, CachePolicy, S)> {
        match self.entry(page) {
            Ok((entry, size)) => {
                if entry.is_present() && entry.is_leaf() {
                    let phys_addr = entry.phys_addr();
                    let flags = entry.flags();
                    let cache = entry.cache_policy();
                    Ok((phys_addr, flags, cache, size))
                } else {
                    Err(PagingError::NotMapped)
                }
            }
            Err(e) => Err(e),
        }
    }
}
