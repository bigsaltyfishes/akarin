use alloc::sync::Arc;

use libakarin_machine_core::memory::VirtAddr;
use thiserror::Error;

use crate::memory::{
    PAGE_SIZE, VmFlags, VmLayoutSegment, VmPointerRegion, VmRange, Vmar, VmarError, VmarMapping,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VSpaceInfo {
    pub root: VmRange,
    pub active_entries: usize,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum VSpaceError {
    #[error("invalid range")]
    InvalidRange,
    #[error("range overlaps an existing region")]
    AlreadyMapped,
    #[error("range is not fully mapped")]
    NotMapped,
    #[error("range violates access permissions")]
    PermissionDenied,
}

impl From<VmarError> for VSpaceError {
    fn from(value: VmarError) -> Self {
        match value {
            VmarError::InvalidRange => Self::InvalidRange,
            VmarError::AlreadyMapped => Self::AlreadyMapped,
            VmarError::PermissionDenied => Self::PermissionDenied,
            VmarError::NotMapped => Self::NotMapped,
        }
    }
}

/// Logical address-space description owned by one process.
#[derive(Clone)]
pub struct VSpace {
    root: Arc<Vmar>,
    segments: VSpaceSegments,
}

#[derive(Debug, Clone)]
struct VSpaceSegments {
    user_image: Arc<Vmar>,
    user_heap: Arc<Vmar>,
    user_mapped: Arc<Vmar>,
    user_mmio: Arc<Vmar>,
    user_stack: Arc<Vmar>,
}

impl core::fmt::Debug for VSpace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VSpace")
            .field("root", &self.root.range())
            .field("active_entries", &self.root.child_count())
            .finish()
    }
}

impl VSpace {
    /// Create a logical address-space description from the supplied root VMAR.
    ///
    /// `VSpace` owns the fixed userspace layout tree so process runtime code
    /// never has to track `UserImage`/`UserMapped`/`UserHeap`/`UserStack`/
    /// `UserMmio` segments separately.
    pub fn new(root: Arc<Vmar>) -> Result<Self, VSpaceError> {
        let segments = VSpaceSegments::new(&root)?;
        Ok(Self { root, segments })
    }

    /// Return the root VMAR.
    pub fn root_vmar(&self) -> &Arc<Vmar> {
        &self.root
    }

    /// Return the VMAR reserved for one fixed userspace layout segment.
    ///
    /// Callers must pass one userspace segment owned by `VSpace`.
    pub fn segment(&self, segment: VmLayoutSegment) -> &Arc<Vmar> {
        self.segments.segment(segment)
    }

    /// Return the resolved mapping at one virtual address.
    pub fn mapping_at(&self, addr: VirtAddr) -> Option<VmarMapping> {
        self.root.mapping_at(addr)
    }

    /// Classify one virtual address within this address space.
    pub fn locate_ptr(&self, addr: VirtAddr) -> VmPointerRegion {
        self.root.locate(addr)
    }

    /// Validate that the complete byte range is mapped with the requested
    /// access rights.
    pub fn validate_range(&self, range: VmRange, required: VmFlags) -> Result<(), VSpaceError> {
        if !range.is_page_aligned() && range.len() < PAGE_SIZE {
            // Small buffers are still validated page-by-page below.
        }
        if !self.root.range().contains_range(range) {
            return Err(VSpaceError::InvalidRange);
        }

        let mut cursor = range.start().as_usize();
        while cursor < range.end().as_usize() {
            let mapping = self
                .mapping_at(VirtAddr::new(cursor))
                .ok_or(VSpaceError::NotMapped)?;
            if !mapping.flags.allows_mmu(required) {
                return Err(VSpaceError::PermissionDenied);
            }
            let next = mapping.range.end().as_usize().min(range.end().as_usize());
            if next <= cursor {
                return Err(VSpaceError::InvalidRange);
            }
            cursor = next;
        }

        Ok(())
    }

    /// Return a summary of the current logical address space.
    pub fn info(&self) -> VSpaceInfo {
        VSpaceInfo {
            root: self.root.range(),
            active_entries: self.root.child_count(),
        }
    }
}

impl VSpaceSegments {
    /// Build the fixed userspace segment tree under the supplied root VMAR.
    fn new(root: &Arc<Vmar>) -> Result<Self, VSpaceError> {
        Ok(Self {
            user_image: root.allocate_child(Self::segment_range(VmLayoutSegment::UserImage)?)?,
            user_heap: root.allocate_child(Self::segment_range(VmLayoutSegment::UserHeap)?)?,
            user_mapped: root.allocate_child(Self::segment_range(VmLayoutSegment::UserMapped)?)?,
            user_mmio: root.allocate_child(Self::segment_range(VmLayoutSegment::UserMmio)?)?,
            user_stack: root.allocate_child(Self::segment_range(VmLayoutSegment::UserStack)?)?,
        })
    }

    /// Return the canonical virtual range assigned to one managed segment.
    fn segment_range(segment: VmLayoutSegment) -> Result<VmRange, VSpaceError> {
        let end = segment.end_exclusive().ok_or(VSpaceError::InvalidRange)?;
        VmRange::new(VirtAddr::new(segment.start()), VirtAddr::new(end))
            .ok_or(VSpaceError::InvalidRange)
    }

    /// Return the owned VMAR for one managed userspace segment.
    fn segment(&self, segment: VmLayoutSegment) -> &Arc<Vmar> {
        match segment {
            VmLayoutSegment::UserImage => &self.user_image,
            VmLayoutSegment::UserHeap => &self.user_heap,
            VmLayoutSegment::UserMapped => &self.user_mapped,
            VmLayoutSegment::UserMmio => &self.user_mmio,
            VmLayoutSegment::UserStack => &self.user_stack,
            _ => panic!("VSpace::segment called with unmanaged segment: {segment:?}"),
        }
    }
}
