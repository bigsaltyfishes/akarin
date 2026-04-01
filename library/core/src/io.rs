use libakarin_machine_core::memory::{PhysAddr, VirtAddr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MmioRegion {
    pub phys_base: PhysAddr,
    pub virt_base: VirtAddr,
    pub size: usize,
}

impl MmioRegion {
    #[inline]
    fn checked_addr(&self, offset: usize, width: usize) -> Option<usize> {
        let end = offset.checked_add(width)?;
        if end > self.size {
            return None;
        }
        Some(self.virt_base.as_usize() + offset)
    }

    pub unsafe fn read_u8(&self, offset: usize) -> Option<u8> {
        let addr = self.checked_addr(offset, 1)?;
        Some(unsafe { core::ptr::read_volatile(addr as *const u8) })
    }

    pub unsafe fn read_u16(&self, offset: usize) -> Option<u16> {
        let addr = self.checked_addr(offset, 2)?;
        Some(unsafe { core::ptr::read_volatile(addr as *const u16) })
    }

    pub unsafe fn read_u32(&self, offset: usize) -> Option<u32> {
        let addr = self.checked_addr(offset, 4)?;
        Some(unsafe { core::ptr::read_volatile(addr as *const u32) })
    }

    pub unsafe fn read_u64(&self, offset: usize) -> Option<u64> {
        let addr = self.checked_addr(offset, 8)?;
        Some(unsafe { core::ptr::read_volatile(addr as *const u64) })
    }

    pub unsafe fn write_u8(&self, offset: usize, value: u8) -> bool {
        let Some(addr) = self.checked_addr(offset, 1) else {
            return false;
        };
        unsafe { core::ptr::write_volatile(addr as *mut u8, value) };
        true
    }

    pub unsafe fn write_u16(&self, offset: usize, value: u16) -> bool {
        let Some(addr) = self.checked_addr(offset, 2) else {
            return false;
        };
        unsafe { core::ptr::write_volatile(addr as *mut u16, value) };
        true
    }

    pub unsafe fn write_u32(&self, offset: usize, value: u32) -> bool {
        let Some(addr) = self.checked_addr(offset, 4) else {
            return false;
        };
        unsafe { core::ptr::write_volatile(addr as *mut u32, value) };
        true
    }

    pub unsafe fn write_u64(&self, offset: usize, value: u64) -> bool {
        let Some(addr) = self.checked_addr(offset, 8) else {
            return false;
        };
        unsafe { core::ptr::write_volatile(addr as *mut u64, value) };
        true
    }
}
