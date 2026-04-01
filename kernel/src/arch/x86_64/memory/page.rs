use libakarin_machine_core::memory::{
    VirtAddr,
    paging::{PageSizeTrait, PageTrait},
};
use libakarin_macros::align_down;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PageSize {
    Size4K = 1 << 12,
    Size2M = 1 << 21,
    Size1G = 1 << 30,
}

impl TryFrom<usize> for PageSize {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            x if x == (1 << 12) => Ok(Self::Size4K),
            x if x == (1 << 21) => Ok(Self::Size2M),
            x if x == (1 << 30) => Ok(Self::Size1G),
            _ => Err(()),
        }
    }
}

impl PageSizeTrait for PageSize {
    const UNIT_PAGE_SIZE: usize = 1 << 12;

    fn validate(size: usize) -> bool {
        Self::try_from(size).is_ok()
    }

    fn size(&self) -> usize {
        *self as usize
    }
}

pub struct Page {
    start_vaddr: VirtAddr,
    size: PageSize,
}

impl Page {
    pub fn new(start_vaddr: VirtAddr, size: PageSize) -> Self {
        assert!(
            start_vaddr.as_usize() % size.size() == 0,
            "Page start address must be aligned to its size"
        );
        Self { start_vaddr, size }
    }
}

impl PageTrait<PageSize> for Page {
    fn containing(virt_addr: VirtAddr, size: PageSize) -> Self {
        let start_vaddr = VirtAddr::new(align_down!(virt_addr.as_usize(), size.size()));
        Self::new(start_vaddr, size)
    }

    fn size(&self) -> PageSize {
        self.size
    }

    fn virt_addr(&self) -> VirtAddr {
        self.start_vaddr
    }
}
