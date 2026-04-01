use crate::memory::{AllocationError, PhysAddr, VirtAddr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FrameZone {
    LowMem,
    Dma32,
    #[default]
    Normal,
}

impl FrameZone {
    pub const fn lower_zone(&self) -> Option<Self> {
        match self {
            FrameZone::LowMem => None,
            FrameZone::Dma32 => Some(FrameZone::LowMem),
            FrameZone::Normal => Some(FrameZone::Dma32),
        }
    }

    pub const fn higher_zone(&self) -> Option<Self> {
        match self {
            FrameZone::LowMem => Some(FrameZone::Dma32),
            FrameZone::Dma32 => Some(FrameZone::Normal),
            FrameZone::Normal => None,
        }
    }
}

pub trait FrameAllocatorTrait: Sync + Send {
    fn unit_page_size(&self) -> usize;
    fn alloc(
        &self,
        addr: Option<PhysAddr>,
        prefer_zone: FrameZone,
        num: usize,
    ) -> Result<VirtAddr, AllocationError>;
    unsafe fn dealloc(&self, frame_addr: VirtAddr, num: usize);
    fn used_frames(&self) -> usize;
    fn total_frames(&self) -> usize;
}
