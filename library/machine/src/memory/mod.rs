mod addr;
mod frame;
pub mod paging;

pub use addr::{PhysAddr, VirtAddr};
pub use frame::{FrameAllocatorTrait, FrameZone};
pub use paging::{
    PageSizeTrait, PageTableEntryTrait, PageTableTrait, PageTrait, PhysFrame, PhysRun,
};
use thiserror::Error;

use crate::memory::paging::PhysFrameTrait;

/// Error type for memory allocation failures.
#[derive(Error, Debug)]
pub enum AllocationError {
    #[error("out of memory")]
    OutOfMemory,
    #[error("address {0:#x} is not managed by this allocator")]
    UnmanagedAddress(PhysAddr),
    #[error("unknown error")]
    Unknown,
}

/// Error returned by architecture-backed user-memory access helpers.
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum UaccessError {
    #[error("invalid user range")]
    InvalidRange,
    #[error("user address is not mapped")]
    NotMapped,
    #[error("user address does not have the required permission")]
    PermissionDenied,
    #[error("kernel fault while touching user memory")]
    Fault,
}

/// Trait defining an address space with paging capabilities.
pub trait AddressSpaceTrait: 'static + Sized {
    type Page: PageTrait<Self::PageSize>;
    type PageSize: PageSizeTrait;
    type PageTable: PageTableTrait<Self, Self::PageSize, Self::Page, Self::PageTableEntry>;
    type PageTableEntry: PageTableEntryTrait;
    type PhysFrame: PhysFrameTrait = PhysFrame<Self>;

    /// Convert a virtual address to a physical address.
    fn virt_to_phys(virt_addr: VirtAddr) -> Option<PhysAddr>;

    /// Convert a physical address to a virtual address.
    fn phys_to_virt(phys_addr: PhysAddr) -> Option<VirtAddr>;

    /// Read from a physical address.
    unsafe fn read_phys(phys_addr: PhysAddr, buffer: &mut [u8]);

    /// Write to a physical address.
    unsafe fn write_phys(phys_addr: PhysAddr, data: &[u8]);

    /// Zero out a physical memory region.
    unsafe fn zero_phys(phys_addr: PhysAddr, size: usize);

    /// Copy memory between physical addresses.
    unsafe fn copy_phys(src_phys_addr: PhysAddr, dest_phys_addr: PhysAddr, size: usize);

    /// Get current page table base address.
    fn current_base() -> PhysAddr;

    /// Switch to a new page table base address.
    unsafe fn switch_base(new_base: PhysAddr);

    /// Invalidate the TLB entry for a given virtual address.
    fn invalidate_tlb(virt_addr: VirtAddr);

    /// Flush the entire TLB.
    fn flush_tlb();

    /// Map kernel space to target page table.
    fn map_kernel_space(page_table: &mut Self::PageTable);

    /// Copy one byte slice from the current task's user address space into a
    /// kernel buffer.
    fn copy_from_user(src: VirtAddr, buffer: &mut [u8]) -> Result<(), UaccessError>;

    /// Copy one byte slice from the kernel into the current task's user
    /// address space.
    fn copy_to_user(dst: VirtAddr, data: &[u8]) -> Result<(), UaccessError>;

    /// Read one plain-old-data value from user memory.
    fn read_user<T>(src: VirtAddr) -> Result<T, UaccessError>
    where
        T: Copy,
    {
        let mut value = core::mem::MaybeUninit::<T>::uninit();
        let bytes = unsafe {
            core::slice::from_raw_parts_mut(
                value.as_mut_ptr().cast::<u8>(),
                core::mem::size_of::<T>(),
            )
        };
        Self::copy_from_user(src, bytes)?;
        Ok(unsafe { value.assume_init() })
    }

    /// Write one plain-old-data value into user memory.
    fn write_user<T>(dst: VirtAddr, value: T) -> Result<(), UaccessError>
    where
        T: Copy,
    {
        let bytes = unsafe {
            core::slice::from_raw_parts(
                (&value as *const T).cast::<u8>(),
                core::mem::size_of::<T>(),
            )
        };
        Self::copy_to_user(dst, bytes)
    }
}
