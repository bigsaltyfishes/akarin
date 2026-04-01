mod page;
mod table;

use core::arch::asm;

use libakarin_machine_core::memory::{
    AddressSpaceTrait, PageTableTrait, PhysAddr, UaccessError, VirtAddr,
};

use crate::{RuntimeServices, arch::Machine};

impl AddressSpaceTrait for Machine {
    type Page = page::Page;
    type PageSize = page::PageSize;
    type PageTable = table::PageTable<Self>;
    type PageTableEntry = table::PageTableEntry;

    fn virt_to_phys(virt_addr: VirtAddr) -> Option<PhysAddr> {
        let offset = RuntimeServices::boot_info().physical_memory_offset;
        Some(PhysAddr::new(virt_addr.as_usize().checked_sub(offset)?))
    }

    fn phys_to_virt(phys_addr: PhysAddr) -> Option<VirtAddr> {
        let offset = RuntimeServices::boot_info().physical_memory_offset;
        Some(VirtAddr::new(phys_addr.as_usize().checked_add(offset)?))
    }

    unsafe fn read_phys(phys_addr: PhysAddr, buffer: &mut [u8]) {
        let virt_addr = Self::phys_to_virt(phys_addr).expect("Invalid physical address");
        unsafe {
            core::ptr::copy_nonoverlapping(virt_addr.as_ptr(), buffer.as_mut_ptr(), buffer.len());
        }
    }

    unsafe fn write_phys(phys_addr: PhysAddr, data: &[u8]) {
        let virt_addr = Self::phys_to_virt(phys_addr).expect("Invalid physical address");
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), virt_addr.as_mut_ptr(), data.len());
        }
    }

    unsafe fn zero_phys(phys_addr: PhysAddr, size: usize) {
        let virt_addr = Self::phys_to_virt(phys_addr).expect("Invalid physical address");
        unsafe {
            core::ptr::write_bytes(virt_addr.as_mut_ptr(), 0, size);
        }
    }

    unsafe fn copy_phys(src_phys_addr: PhysAddr, dest_phys_addr: PhysAddr, size: usize) {
        let src_virt_addr =
            Self::phys_to_virt(src_phys_addr).expect("Invalid source physical address");
        let dest_virt_addr =
            Self::phys_to_virt(dest_phys_addr).expect("Invalid destination physical address");
        unsafe {
            core::ptr::copy_nonoverlapping(
                src_virt_addr.as_ptr(),
                dest_virt_addr.as_mut_ptr(),
                size,
            );
        }
    }

    fn current_base() -> PhysAddr {
        let cr3: usize;
        unsafe {
            core::arch::asm!("mov {}, cr3", out(reg) cr3);
        }
        PhysAddr::new(cr3)
    }

    unsafe fn switch_base(new_base: PhysAddr) {
        unsafe {
            core::arch::asm!("mov cr3, {}", in(reg) new_base.as_usize());
        }
    }

    fn invalidate_tlb(virt_addr: VirtAddr) {
        unsafe {
            core::arch::asm!("invlpg [{}]", in(reg) virt_addr.as_usize(), options(nostack, preserves_flags));
        }
    }

    fn flush_tlb() {
        let cr3: usize;
        unsafe {
            core::arch::asm!("mov {}, cr3", out(reg) cr3);
            core::arch::asm!("mov cr3, {}", in(reg) cr3);
        }
    }

    fn map_kernel_space(page_table: &mut Self::PageTable) {
        let entry_range = 0x100..0x200; // 0xFFFF_8000_0000_0000 .. 0xFFFF_FFFF_FFFF_FFFF
        let active_raw = Self::phys_to_virt(Self::current_base())
            .expect("Current page table base address is invalid")
            .as_mut_ptr() as *mut Self::PageTableEntry;
        let target_phys = Self::PageTable::phys_addr(page_table);
        let target_raw = Self::phys_to_virt(target_phys)
            .expect("Target page table base address is invalid")
            .as_mut_ptr() as *mut Self::PageTableEntry;
        let src_table = unsafe { core::slice::from_raw_parts(active_raw, 512) };
        let dst_table = unsafe { core::slice::from_raw_parts_mut(target_raw, 512) };
        for i in entry_range {
            dst_table[i] = src_table[i];
        }
    }

    fn copy_from_user(src: VirtAddr, buffer: &mut [u8]) -> Result<(), UaccessError> {
        uaccess_copy(
            src,
            VirtAddr::new(buffer.as_mut_ptr() as usize),
            buffer.len(),
        )
    }

    fn copy_to_user(dst: VirtAddr, data: &[u8]) -> Result<(), UaccessError> {
        uaccess_copy(VirtAddr::new(data.as_ptr() as usize), dst, data.len())
    }
}

fn uaccess_copy(src: VirtAddr, dst: VirtAddr, len: usize) -> Result<(), UaccessError> {
    let mut err: usize = 0;

    unsafe {
        asm!(
            "xor {err}, {err}",
            "2: nop",
            "3: rep movsb",
            "4: nop",
            "5: jmp 7f",
            "6: mov {err}, 1",
            "7: nop",
            ".pushsection __DATA,__extable,regular,no_dead_strip",
            ".balign 8",
            ".quad 2b",
            ".quad 4b",
            ".quad 6b",
            ".popsection",
            err = out(reg) err,
            inout("rcx") len => _,
            inout("rsi") src.as_usize() => _,
            inout("rdi") dst.as_usize() => _,
            options(nostack)
        )
    }

    if err != 0 {
        Err(UaccessError::Fault)
    } else {
        Ok(())
    }
}
