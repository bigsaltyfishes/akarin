use core::{alloc::Layout, cell::Cell, ops::Range};

use libakarin_machine_core::{
    cpu::PerCpuTrait,
    memory::{
        AddressSpaceTrait, AllocationError, FrameAllocatorTrait, FrameZone,
        PhysAddr, VirtAddr,
    },
    sync::RawScopedGuard,
};
use libakarin_macros::{align_down, align_up, prev_power_of_two};
use llfree::{
    Alloc, Flags, FrameId, HUGE_ORDER, Init, Kind, KindDesc, LLFree, MAX_ORDER, MetaData,
};

use crate::arch::{Machine, PerCpu, guards::IrqSaveGuard, vm::UNIT_PAGE_SIZE};

const ONE_MIB: usize = 0x10_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocationZone {
    Low,
    Dma32,
    Normal,
}

impl AllocationZone {
    const LOW_MEM_RANGE: Range<PhysAddr> = PhysAddr::new(0)..PhysAddr::new(ONE_MIB);
    const DMA32_RANGE: Range<PhysAddr> = PhysAddr::new(ONE_MIB)..PhysAddr::new(0x1_0000_0000);
    const NORMAL_RANGE: Range<PhysAddr> = PhysAddr::new(0x1_0000_0000)..PhysAddr::new(usize::MAX);

    pub fn contains(&self, addr: PhysAddr) -> bool {
        match self {
            AllocationZone::Low => Self::LOW_MEM_RANGE.contains(&addr),
            AllocationZone::Dma32 => Self::DMA32_RANGE.contains(&addr),
            AllocationZone::Normal => Self::NORMAL_RANGE.contains(&addr),
        }
    }

    pub fn max_frame_num(&self) -> usize {
        match self {
            AllocationZone::Low => Self::LOW_MEM_RANGE.end.as_usize() / 4096,
            AllocationZone::Dma32 => {
                (Self::DMA32_RANGE.end.as_usize() - Self::DMA32_RANGE.start.as_usize()) / 4096
            }
            AllocationZone::Normal => {
                (Self::NORMAL_RANGE.end.as_usize() - Self::NORMAL_RANGE.start.as_usize()) / 4096
            }
        }
    }

    pub fn frame_num_until(&self, max_phys_addr: PhysAddr) -> usize {
        let start = self.range().start.as_usize();
        let end = self
            .range()
            .end
            .as_usize()
            .min(align_up!(max_phys_addr.as_usize(), UNIT_PAGE_SIZE));
        end.saturating_sub(start) / UNIT_PAGE_SIZE
    }

    pub fn range(&self) -> Range<PhysAddr> {
        match self {
            AllocationZone::Low => Self::LOW_MEM_RANGE,
            AllocationZone::Dma32 => Self::DMA32_RANGE,
            AllocationZone::Normal => Self::NORMAL_RANGE,
        }
    }
}

pub struct LLFreeAllocator {
    zone: AllocationZone,
    total_frames: Cell<usize>,
    inner: LLFree<'static>,
}

impl LLFreeAllocator {
    pub fn new(zone: AllocationZone, frame_num: usize, cpu_num: usize) -> Self {
        if frame_num > zone.max_frame_num() {
            panic!(
                "[memory/llfree]: frame_num {} exceeds the maximum for zone {:?}",
                frame_num, zone
            );
        }

        let local_count = cpu_num.min(u8::MAX as usize);
        let kinds = [
            KindDesc(Kind::from_bits(0), local_count as _),
            KindDesc(Kind::HUGE, local_count as _),
        ];
        let meta_size = LLFree::metadata_size(&kinds, frame_num);
        let aligend_buf = |size: usize| {
            let align = align_of::<llfree::util::Align>();
            let layout =
                Layout::from_size_align(size, align).expect("invalid layout for LLFree metadata");
            let ptr = unsafe { alloc::alloc::alloc(layout) };
            if ptr.is_null() {
                alloc::alloc::handle_alloc_error(layout);
            }
            unsafe { core::slice::from_raw_parts_mut(ptr, size) }
        };
        let local = aligend_buf(meta_size.local);
        let trees = aligend_buf(meta_size.trees);
        let lower = aligend_buf(meta_size.lower);
        let metadata = MetaData {
            local,
            trees,
            lower,
        };

        let ret = Self {
            zone,
            total_frames: Cell::new(0),
            inner: LLFree::new(&kinds, frame_num, Init::AllocAll, metadata)
                .expect("failed to initialize LLFree allocator"),
        };

        info!(
            "[memory/llfree]: zone {:?} initialized, range={:?}, meta_size={:?}",
            zone,
            zone.range(),
            meta_size
        );

        ret
    }

    pub fn is_managed(&self, addr: PhysAddr) -> bool {
        self.zone.contains(addr)
    }

    pub fn total_frames(&self) -> usize {
        self.total_frames.get()
    }

    pub fn alloc(
        &self,
        addr: Option<PhysAddr>,
        order: usize,
        cpu_id: usize,
    ) -> Result<PhysAddr, AllocationError> {
        let id = if let Some(phys) = addr {
            if !self.zone.contains(phys) {
                return Err(AllocationError::UnmanagedAddress(phys));
            }

            Some(FrameId(
                (phys.as_usize() - self.zone.range().start.as_usize()) / UNIT_PAGE_SIZE,
            ))
        } else {
            None
        };

        let flags = Flags::with(order, cpu_id);
        loop {
            let result = self
                .inner
                .get(id, flags)
                .map(|frame_id| self.zone.range().start + (frame_id.0 as usize * UNIT_PAGE_SIZE));
            match result {
                Ok(addr) => {
                    return Ok(addr);
                }
                Err(llfree::Error::Retry) => {
                    // CAS failed, retry the allocation
                    core::hint::spin_loop();
                    continue;
                }
                Err(llfree::Error::Memory) => {
                    return Err(AllocationError::OutOfMemory);
                }
                _ => unreachable!(),
            }
        }
    }

    pub fn dealloc(
        &self,
        addr: PhysAddr,
        order: usize,
        cpu_id: usize,
    ) -> Result<(), AllocationError> {
        if !self.zone.contains(addr) {
            return Err(AllocationError::UnmanagedAddress(addr));
        }

        let frame_id =
            FrameId((addr.as_usize() - self.zone.range().start.as_usize()) / UNIT_PAGE_SIZE);
        let flags = Flags::with(order, cpu_id);

        loop {
            match self.inner.put(frame_id, flags) {
                Ok(()) => return Ok(()),
                Err(llfree::Error::Retry) => {
                    // CAS failed, retry the deallocation
                    core::hint::spin_loop();
                    continue;
                }
                Err(e) => {
                    panic!(
                        "failed to deallocate frame at {:#x} (frame_id={:?}, flags={:?}), error: \
                         {:?}",
                        addr, frame_id, flags, e
                    );
                }
            }
        }
    }

    pub fn used_frames(&self) -> usize {
        let stats = self.inner.fast_stats();
        self.total_frames() - stats.free_frames
    }

    /// Mark a range of physical addresses as usable for allocation.
    ///
    /// This should be called during initialization to populate the allocator
    /// with available memory regions. The `cpuid` and `cpu_num` parameters
    /// are used to set the appropriate flags for the frames based on their
    /// order, allowing for potential optimizations in multi-core scenarios.
    /// The function will align the provided range to page boundaries and
    /// insert frames into the allocator in batches according to their
    /// order, starting from the lowest order (largest blocks) to maximize
    /// efficiency.
    pub unsafe fn mark_usable(&self, cpuid: usize, cpu_num: usize, range: Range<PhysAddr>) {
        let start = align_up!(range.start.as_usize(), UNIT_PAGE_SIZE);
        let end = align_down!(range.end.as_usize(), UNIT_PAGE_SIZE);

        assert!(
            self.zone.contains(range.start) && self.zone.contains(PhysAddr::new(end - 1)),
            "range {:?} is not fully contained in zone {:?}",
            range,
            self.zone
        );

        if end <= start || (end - start) < UNIT_PAGE_SIZE {
            return;
        }

        let mut frame = (start - self.zone.range().start.as_usize()) / UNIT_PAGE_SIZE;
        let frame_end = (end - self.zone.range().start.as_usize()) / UNIT_PAGE_SIZE;
        while frame < frame_end {
            let mut order = frame.trailing_zeros() as usize;
            if order > MAX_ORDER {
                order = MAX_ORDER;
            }

            order = order.min(prev_power_of_two!(usize, frame_end - frame).trailing_zeros() as _);

            self.inner
                .put(
                    FrameId(frame),
                    Flags::with(order, cpuid + if order >= HUGE_ORDER { cpu_num } else { 0 }),
                )
                .map_err(|e| {
                    panic!(
                        "Failed to marking address as usable: frame={}, order={}, reason={:?}",
                        frame, order, e
                    );
                });

            frame += 1 << order;
        }

        self.total_frames
            .set(self.total_frames.get() + (end - start) / UNIT_PAGE_SIZE);
    }
}

pub struct FrameAllocator {
    cpu_num: usize,
    low_zone: LLFreeAllocator,
    dma32_zone: LLFreeAllocator,
    normal_zone: Option<LLFreeAllocator>,
}

impl FrameAllocator {
    pub fn new(cpu_num: usize, max_phys_addr: PhysAddr) -> Self {
        assert_ne!(cpu_num, 0, "cpu_num must be greater than 0");

        let low_zone = LLFreeAllocator::new(
            AllocationZone::Low,
            AllocationZone::Low.frame_num_until(max_phys_addr),
            cpu_num,
        );
        let dma32_zone = LLFreeAllocator::new(
            AllocationZone::Dma32,
            AllocationZone::Dma32.frame_num_until(max_phys_addr),
            cpu_num,
        );
        let normal_zone = if max_phys_addr > AllocationZone::Normal.range().start {
            Some(LLFreeAllocator::new(
                AllocationZone::Normal,
                AllocationZone::Normal.frame_num_until(max_phys_addr),
                cpu_num,
            ))
        } else {
            None
        };

        FrameAllocator {
            cpu_num,
            low_zone,
            dma32_zone,
            normal_zone,
        }
    }

    pub fn feed<I>(&self, ranges: I)
    where
        I: IntoIterator<Item = Range<PhysAddr>>,
    {
        let _guard = IrqSaveGuard::enter();
        let marker = |range: &mut Range<PhysAddr>, curr_zone: &LLFreeAllocator| {
            let zone_end = curr_zone.zone.range().end;
            let feed_end = if range.end <= zone_end {
                range.end
            } else {
                zone_end
            };
            let feed = range.start..feed_end;
            range.start = feed_end;

            unsafe {
                curr_zone.mark_usable(0, self.cpu_num, feed);
            }
        };

        for mut range in ranges {
            while !range.is_empty() {
                match range.start {
                    addr if self.low_zone.is_managed(addr) => marker(&mut range, &self.low_zone),
                    addr if self.dma32_zone.is_managed(addr) => {
                        marker(&mut range, &self.dma32_zone)
                    }
                    addr if self
                        .normal_zone
                        .as_ref()
                        .map_or(false, |zone| zone.is_managed(addr)) =>
                    {
                        marker(&mut range, self.normal_zone.as_ref().unwrap());
                    }
                    _ => warn!("memory {:#x?} is not managed by any zone", range),
                }
            }
        }
    }

    pub fn total_frames(&self) -> usize {
        self.low_zone.total_frames()
            + self.dma32_zone.total_frames()
            + self
                .normal_zone
                .as_ref()
                .map_or(0, |zone| zone.total_frames())
    }

    pub fn used_frames(&self) -> usize {
        self.low_zone.used_frames()
            + self.dma32_zone.used_frames()
            + self
                .normal_zone
                .as_ref()
                .map_or(0, |zone| zone.used_frames())
    }

    /// Return a reference to the allocator for the specified zone, or `None` if
    /// the zone is not available.
    fn zone_ref(&self, zone: FrameZone) -> Option<&LLFreeAllocator> {
        match zone {
            FrameZone::LowMem => Some(&self.low_zone),
            FrameZone::Dma32 => Some(&self.dma32_zone),
            FrameZone::Normal => self.normal_zone.as_ref(),
        }
    }
}

impl FrameAllocatorTrait for FrameAllocator {
    fn unit_page_size(&self) -> usize {
        UNIT_PAGE_SIZE
    }

    fn alloc(
        &self,
        addr: Option<PhysAddr>,
        mut prefer_zone: FrameZone,
        num: usize,
    ) -> Result<VirtAddr, AllocationError> {
        let _guard = IrqSaveGuard::enter();
        let cpu_id = PerCpu::id();
        let order = num.next_power_of_two().trailing_zeros() as usize;
        if order > MAX_ORDER {
            error!(
                "requested allocation order {} exceeds llfree max order {}",
                order, MAX_ORDER
            );
            return Err(AllocationError::OutOfMemory);
        }
        loop {
            if let Some(zone) = self.zone_ref(prefer_zone) {
                match zone.alloc(addr, order, cpu_id) {
                    Ok(phys_addr) => {
                        return Machine::phys_to_virt(phys_addr).ok_or(AllocationError::Unknown);
                    }
                    Err(AllocationError::OutOfMemory) => {
                        prefer_zone = prefer_zone
                            .lower_zone()
                            .ok_or(AllocationError::OutOfMemory)?
                    }
                    Err(err) => return Err(err),
                }
            } else {
                prefer_zone = prefer_zone
                    .lower_zone()
                    .ok_or(AllocationError::OutOfMemory)?;
            }
        }
    }

    unsafe fn dealloc(&self, frame_addr: VirtAddr, num: usize) {
        let order = num.next_power_of_two().trailing_zeros() as usize;
        let phys_addr = Machine::virt_to_phys(frame_addr).expect("invalid frame address");
        let cpu_id = PerCpu::id();
        let zone = if self.low_zone.is_managed(phys_addr) {
            &self.low_zone
        } else if self.dma32_zone.is_managed(phys_addr) {
            &self.dma32_zone
        } else if let Some(normal_zone) = &self.normal_zone {
            if normal_zone.is_managed(phys_addr) {
                normal_zone
            } else {
                panic!("address {:#x} is not managed by any zone", phys_addr);
            }
        } else {
            panic!("address {:#x} is not managed by any zone", phys_addr);
        };

        zone.dealloc(phys_addr, order, cpu_id)
            .expect("failed to deallocate frame");
    }

    fn used_frames(&self) -> usize {
        self.used_frames()
    }

    fn total_frames(&self) -> usize {
        self.total_frames()
    }
}

unsafe impl Send for FrameAllocator {}
unsafe impl Sync for FrameAllocator {}
