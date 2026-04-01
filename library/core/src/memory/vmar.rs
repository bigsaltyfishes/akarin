use alloc::{boxed::Box, collections::BTreeMap, sync::Arc, vec::Vec};
use core::convert::TryFrom;

use async_trait::async_trait;
use libakarin_collections::tree::{SegmentState, SegmentTree};
use libakarin_machine_core::{
    memory::{VirtAddr, paging::MMUFlags},
    sync::{NoOp, ScopedGuard},
};
use libakarin_object::{ControlPlane, ObjectError, SyscallDispatch};
use libakarin_sync::spin::SpinRwLock;
use libakarin_syscall::SyscallResult;
use thiserror::Error;

use crate::memory::{
    VmControlError, VmFaultResolution, VmFlags, VmInvokeError, VmInvokeFrame, Vmo, VmoFaultPolicy,
};

pub const PAGE_SIZE: usize = 0x1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmLayoutSegment {
    UserLowGuard,
    UserImage,
    UserHeap,
    UserMapped,
    UserMmio,
    UserStack,
    KernelDirectMap,
    KernelGuard,
    KernelMapped,
    KernelStack,
    KernelReserved,
    KernelImage,
}

impl VmLayoutSegment {
    pub const fn start(self) -> usize {
        match self {
            Self::UserLowGuard => 0x0000_0000_0000_0000,
            Self::UserImage => 0x0000_0000_0040_0000,
            Self::UserHeap => 0x0000_0100_0000_0000,
            Self::UserMapped => 0x0000_1000_0000_0000,
            Self::UserMmio => 0x0000_7000_0000_0000,
            Self::UserStack => 0x0000_7800_0000_0000,
            Self::KernelDirectMap => 0xFFFF_8000_0000_0000,
            Self::KernelGuard => 0xFFFF_C000_0000_0000,
            Self::KernelMapped => 0xFFFF_C100_0000_0000,
            Self::KernelStack => 0xFFFF_FF00_0000_0000,
            Self::KernelReserved => 0xFFFF_FF80_0000_0000,
            Self::KernelImage => 0xFFFF_FFFF_8000_0000,
        }
    }

    pub const fn end_exclusive(self) -> Option<usize> {
        match self {
            Self::UserLowGuard => Some(0x0000_0000_0020_0000),
            Self::UserImage => Some(0x0000_0100_0000_0000),
            Self::UserHeap => Some(0x0000_1000_0000_0000),
            Self::UserMapped => Some(0x0000_7000_0000_0000),
            Self::UserMmio => Some(0x0000_7800_0000_0000),
            Self::UserStack => Some(0x0000_8000_0000_0000),
            Self::KernelDirectMap => Some(0xFFFF_C000_0000_0000),
            Self::KernelGuard => Some(0xFFFF_C100_0000_0000),
            Self::KernelMapped => Some(0xFFFF_FF00_0000_0000),
            Self::KernelStack => Some(0xFFFF_FF80_0000_0000),
            Self::KernelReserved => Some(0xFFFF_FFFF_8000_0000),
            Self::KernelImage => None,
        }
    }

    pub const fn is_guard(self) -> bool {
        matches!(self, Self::UserLowGuard | Self::KernelGuard)
    }

    pub const fn is_user(self) -> bool {
        matches!(
            self,
            Self::UserLowGuard
                | Self::UserImage
                | Self::UserHeap
                | Self::UserMapped
                | Self::UserMmio
                | Self::UserStack
        )
    }

    pub fn user_space_range() -> VmRange {
        VmRange::new(
            VirtAddr::new(Self::UserLowGuard.start()),
            VirtAddr::new(Self::UserStack.end_exclusive().unwrap()),
        )
        .unwrap()
    }

    pub fn kernel_space_range() -> VmRange {
        VmRange::new(
            VirtAddr::new(Self::KernelDirectMap.start()),
            VirtAddr::new(Self::KernelImage.end_exclusive().unwrap()),
        )
        .unwrap()
    }

    pub fn max_flags(self) -> VmFlags {
        match self {
            Self::UserLowGuard | Self::KernelGuard => VmFlags::empty(),
            Self::UserImage | Self::UserMapped => {
                VmFlags::READ | VmFlags::WRITE | VmFlags::EXECUTE | VmFlags::USER
            }
            Self::UserHeap => VmFlags::READ | VmFlags::WRITE | VmFlags::USER,
            Self::UserMmio => VmFlags::READ | VmFlags::WRITE | VmFlags::USER | VmFlags::DEVICE,
            Self::UserStack => VmFlags::READ | VmFlags::WRITE | VmFlags::USER,
            Self::KernelDirectMap => VmFlags::READ | VmFlags::WRITE,
            Self::KernelMapped => VmFlags::READ | VmFlags::WRITE | VmFlags::EXECUTE,
            Self::KernelStack => VmFlags::READ | VmFlags::WRITE,
            Self::KernelReserved => VmFlags::empty(),
            Self::KernelImage => {
                VmFlags::READ | VmFlags::WRITE | VmFlags::EXECUTE | VmFlags::GLOBAL
            }
        }
    }

    pub fn classify(addr: VirtAddr) -> Option<Self> {
        let v = addr.as_usize();
        const ORDERED: [VmLayoutSegment; 12] = [
            VmLayoutSegment::UserLowGuard,
            VmLayoutSegment::UserImage,
            VmLayoutSegment::UserHeap,
            VmLayoutSegment::UserMapped,
            VmLayoutSegment::UserMmio,
            VmLayoutSegment::UserStack,
            VmLayoutSegment::KernelDirectMap,
            VmLayoutSegment::KernelGuard,
            VmLayoutSegment::KernelMapped,
            VmLayoutSegment::KernelStack,
            VmLayoutSegment::KernelReserved,
            VmLayoutSegment::KernelImage,
        ];

        for segment in ORDERED {
            if v < segment.start() {
                continue;
            }
            match segment.end_exclusive() {
                Some(end) if v < end => return Some(segment),
                None => return Some(segment),
                _ => {}
            }
        }

        None
    }

    pub fn classify_range(range: VmRange) -> Option<Self> {
        let start = Self::classify(range.start())?;
        let end_addr = VirtAddr::new(range.end().as_usize().saturating_sub(1));
        let end = Self::classify(end_addr)?;
        (start == end).then_some(start)
    }
}

#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionPurpose {
    General,
    KernelText,
    KernelData,
    KernelHeap,
    KernelStack,
    PageTable,
    DeviceMmio,
    User,
}

impl RegionPurpose {
    pub fn is_resident(self) -> bool {
        matches!(
            self,
            RegionPurpose::KernelText | RegionPurpose::KernelData | RegionPurpose::PageTable
        )
    }
}

impl TryFrom<usize> for RegionPurpose {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            x if x == Self::General as usize => Ok(Self::General),
            x if x == Self::KernelText as usize => Ok(Self::KernelText),
            x if x == Self::KernelData as usize => Ok(Self::KernelData),
            x if x == Self::KernelHeap as usize => Ok(Self::KernelHeap),
            x if x == Self::KernelStack as usize => Ok(Self::KernelStack),
            x if x == Self::PageTable as usize => Ok(Self::PageTable),
            x if x == Self::DeviceMmio as usize => Ok(Self::DeviceMmio),
            x if x == Self::User as usize => Ok(Self::User),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct VmRange {
    start: VirtAddr,
    end: VirtAddr,
}

impl VmRange {
    pub fn new(start: VirtAddr, end: VirtAddr) -> Option<Self> {
        (start < end).then_some(Self { start, end })
    }

    pub fn start(self) -> VirtAddr {
        self.start
    }

    pub fn end(self) -> VirtAddr {
        self.end
    }

    pub fn len(self) -> usize {
        self.end.as_usize() - self.start.as_usize()
    }

    pub fn contains(self, addr: VirtAddr) -> bool {
        self.start <= addr && addr < self.end
    }

    pub fn contains_range(self, other: VmRange) -> bool {
        self.start <= other.start && other.end <= self.end
    }

    pub fn overlaps(self, other: VmRange) -> bool {
        self.start < other.end && other.start < self.end
    }

    pub fn is_page_aligned(self) -> bool {
        self.start.as_usize().is_multiple_of(PAGE_SIZE)
            && self.end.as_usize().is_multiple_of(PAGE_SIZE)
    }
}

#[derive(Debug, Clone)]
pub struct VmarMapping {
    pub range: VmRange,
    pub flags: VmFlags,
    pub purpose: RegionPurpose,
    pub vmo: Arc<Vmo>,
    pub vmo_offset: usize,
}

impl VmarMapping {
    pub fn validate_layout(&self) -> bool {
        let Some(segment) = VmLayoutSegment::classify_range(self.range) else {
            return false;
        };

        if segment.is_guard() {
            return false;
        }

        if !segment.max_flags().allows_mmu(self.flags) {
            return false;
        }

        if self.flags.contains(VmFlags::GLOBAL)
            && (!segment.is_user() && !self.purpose.is_resident())
        {
            return false;
        }

        if self.flags.contains(VmFlags::DEVICE) && self.purpose != RegionPurpose::DeviceMmio {
            return false;
        }

        if !self.vmo.can_map(self.flags) {
            return false;
        }

        let end_offset = self.vmo_offset.saturating_add(self.range.len());
        end_offset <= self.vmo.size()
    }

    /// Return the fault-servicing strategy implied by the backing VMO.
    pub fn fault_policy(&self) -> VmoFaultPolicy {
        self.vmo.fault_policy()
    }

    /// Translate one virtual address inside this mapping into a VMO-relative
    /// byte offset.
    pub fn vmo_offset_for_addr(&self, addr: VirtAddr) -> Option<usize> {
        if !self.range.contains(addr) {
            return None;
        }

        let relative = addr.as_usize() - self.range.start().as_usize();
        self.vmo_offset.checked_add(relative)
    }

    /// Classify the servicing work required for one fault against this mapping.
    pub fn resolve_fault(&self, addr: VirtAddr, access: MMUFlags) -> VmFaultResolution {
        let required = VmFlags::from_mmu_flags(access);
        if !self.flags.allows_mmu(required) {
            return VmFaultResolution::ProtectionDenied;
        }

        let offset = match self.vmo_offset_for_addr(addr) {
            Some(offset) => offset,
            None => return VmFaultResolution::InvalidRange,
        };
        self.vmo.resolve_fault(offset, access)
    }
}

#[derive(Debug, Clone)]
pub enum VmarEntry {
    Guard(VmRange),
    Mapping(VmarMapping),
    Region { range: VmRange, child: Vmar },
}

impl VmarEntry {
    /// Return the virtual range covered by this entry.
    pub fn range(&self) -> VmRange {
        match self {
            Self::Guard(range) => *range,
            Self::Mapping(mapping) => mapping.range,
            Self::Region { range, .. } => *range,
        }
    }
}

/// Object-specific VMAR slow-path method identifiers.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmarMethod {
    Allocate = 0,
    Destroy = 1,
    Map = 2,
    MapClock = 3,
    MapIob = 4,
    OpRange = 5,
    Protect = 6,
    Unmap = 7,
}

pub const VM_ALLOCATE: u32 = 1 << 0;
pub const VM_MAP: u32 = 1 << 1;
pub const VM_PROTECT: u32 = 1 << 2;
pub const VM_UNMAP: u32 = 1 << 3;
pub const VM_DESTROY: u32 = 1 << 4;
pub const VM_QUERY: u32 = 1 << 5;

pub const VMAR_DEFAULT_INTERFACE_CAPS: u32 =
    VM_ALLOCATE | VM_MAP | VM_PROTECT | VM_UNMAP | VM_QUERY;
pub const VMAR_ADMIN_INTERFACE_CAPS: u32 = VM_DESTROY;

impl TryFrom<usize> for VmarMethod {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            x if x == Self::Allocate as usize => Ok(Self::Allocate),
            x if x == Self::Destroy as usize => Ok(Self::Destroy),
            x if x == Self::Map as usize => Ok(Self::Map),
            x if x == Self::MapClock as usize => Ok(Self::MapClock),
            x if x == Self::MapIob as usize => Ok(Self::MapIob),
            x if x == Self::OpRange as usize => Ok(Self::OpRange),
            x if x == Self::Protect as usize => Ok(Self::Protect),
            x if x == Self::Unmap as usize => Ok(Self::Unmap),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone)]
pub enum VmPointerRegion {
    Unmapped,
    GuardPage,
    Static(VmLayoutSegment),
    Reserved(VmRange),
    Mapping(VmarMapping),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuardedStackLayout {
    pub guard: VmRange,
    pub stack: VmRange,
}

impl GuardedStackLayout {
    pub fn from_low_base(base: VirtAddr, stack_size: usize) -> Option<Self> {
        if stack_size == 0 || !base.as_usize().is_multiple_of(PAGE_SIZE) {
            return None;
        }
        let guard_end = base.as_usize().checked_add(PAGE_SIZE)?;
        let stack_end = guard_end.checked_add(stack_size)?;
        let guard = VmRange::new(base, VirtAddr::new(guard_end))?;
        let stack = VmRange::new(VirtAddr::new(guard_end), VirtAddr::new(stack_end))?;
        Some(Self { guard, stack })
    }

    pub fn top(self) -> VirtAddr {
        self.stack.end()
    }
}

fn allow_interface(interface_caps: u32, required: u32) -> Result<(), ObjectError> {
    if interface_caps == u32::MAX || (interface_caps & required) == required {
        Ok(())
    } else {
        Err(ObjectError::InsufficientCapabilities)
    }
}

pub struct VmarReadGuard<'a> {
    vmar: &'a Vmar,
    interface_caps: u32,
}

impl<'a> VmarReadGuard<'a> {
    fn new(vmar: &'a Vmar, interface_caps: u32) -> Self {
        Self {
            vmar,
            interface_caps,
        }
    }

    pub fn range(&self) -> Result<VmRange, ObjectError> {
        allow_interface(self.interface_caps, VM_QUERY)?;
        Ok(self.vmar.range())
    }
}

pub struct VmarWriteGuard<'a> {
    vmar: &'a Vmar,
    interface_caps: u32,
}

impl<'a> VmarWriteGuard<'a> {
    fn new(vmar: &'a Vmar, interface_caps: u32) -> Self {
        Self {
            vmar,
            interface_caps,
        }
    }

    pub fn allocate_child(&self, range: VmRange) -> Result<Arc<Vmar>, ObjectError> {
        allow_interface(self.interface_caps, VM_ALLOCATE)?;
        self.vmar
            .allocate_child(range)
            .map_err(|_| ObjectError::InvalidArgument)
    }

    /// Create one child VMAR while preserving VM subsystem errors.
    pub fn allocate_child_vm(&self, range: VmRange) -> Result<Arc<Vmar>, VmControlError> {
        allow_interface(self.interface_caps, VM_ALLOCATE).map_err(VmControlError::Object)?;
        self.vmar
            .allocate_child(range)
            .map_err(|error| VmControlError::Vm(error.into()))
    }

    pub fn allocate_child_any(&self, size: usize) -> Result<Arc<Vmar>, ObjectError> {
        allow_interface(self.interface_caps, VM_ALLOCATE)?;
        self.vmar
            .allocate_child_any(size)
            .map_err(|_| ObjectError::InvalidArgument)
    }

    /// Allocate one child VMAR from the first fitting gap while preserving VM
    /// subsystem errors.
    pub fn allocate_child_any_vm(&self, size: usize) -> Result<Arc<Vmar>, VmControlError> {
        allow_interface(self.interface_caps, VM_ALLOCATE).map_err(VmControlError::Object)?;
        self.vmar
            .allocate_child_any(size)
            .map_err(|error| VmControlError::Vm(error.into()))
    }

    pub fn map_vmo(
        &self,
        range: VmRange,
        vmo: Arc<Vmo>,
        vmo_offset: usize,
        flags: VmFlags,
        purpose: RegionPurpose,
    ) -> Result<(), ObjectError> {
        allow_interface(self.interface_caps, VM_MAP)?;
        self.vmar
            .map_vmo(range, vmo, vmo_offset, flags, purpose)
            .map_err(|_| ObjectError::InvalidArgument)
    }

    /// Insert one VMO mapping while preserving VM subsystem errors.
    pub fn map_vmo_vm(
        &self,
        range: VmRange,
        vmo: Arc<Vmo>,
        vmo_offset: usize,
        flags: VmFlags,
        purpose: RegionPurpose,
    ) -> Result<(), VmControlError> {
        allow_interface(self.interface_caps, VM_MAP).map_err(VmControlError::Object)?;
        self.vmar
            .map_vmo(range, vmo, vmo_offset, flags, purpose)
            .map_err(|error| VmControlError::Vm(error.into()))
    }

    pub fn protect(
        &self,
        range: VmRange,
        flags: VmFlags,
    ) -> Result<(VmarMapping, VmarMapping), ObjectError> {
        allow_interface(self.interface_caps, VM_PROTECT)?;
        let old = self
            .vmar
            .mapping_at(range.start())
            .ok_or(ObjectError::ObjectNotFound)?;
        if old.range != range {
            return Err(ObjectError::ObjectNotFound);
        }
        self.vmar
            .protect(range, flags)
            .map_err(|_| ObjectError::InvalidArgument)?;
        let new = self
            .vmar
            .mapping_at(range.start())
            .ok_or(ObjectError::ObjectNotFound)?;
        Ok((old, new))
    }

    /// Protect one mapping while preserving VM subsystem errors.
    pub fn protect_vm(
        &self,
        range: VmRange,
        flags: VmFlags,
    ) -> Result<(VmarMapping, VmarMapping), VmControlError> {
        allow_interface(self.interface_caps, VM_PROTECT).map_err(VmControlError::Object)?;
        let old = self
            .vmar
            .mapping_at(range.start())
            .ok_or(VmControlError::Vm(VmInvokeError::NotMapped))?;
        if old.range != range {
            return Err(VmControlError::Vm(VmInvokeError::NotMapped));
        }
        self.vmar
            .protect(range, flags)
            .map_err(|error| VmControlError::Vm(error.into()))?;
        let new = self
            .vmar
            .mapping_at(range.start())
            .ok_or(VmControlError::Vm(VmInvokeError::NotMapped))?;
        Ok((old, new))
    }

    pub fn unmap(&self, start: VirtAddr) -> Result<VmarEntry, ObjectError> {
        allow_interface(self.interface_caps, VM_UNMAP)?;
        self.vmar
            .unmap(start)
            .map_err(|_| ObjectError::ObjectNotFound)
    }

    /// Unmap one child entry while preserving VM subsystem errors.
    pub fn unmap_vm(&self, start: VirtAddr) -> Result<VmarEntry, VmControlError> {
        allow_interface(self.interface_caps, VM_UNMAP).map_err(VmControlError::Object)?;
        self.vmar
            .unmap(start)
            .map_err(|error| VmControlError::Vm(error.into()))
    }
}

pub struct VmarAdminGuard<'a> {
    _vmar: &'a Vmar,
    interface_caps: u32,
}

impl<'a> VmarAdminGuard<'a> {
    fn new(vmar: &'a Vmar, interface_caps: u32) -> Self {
        Self {
            _vmar: vmar,
            interface_caps,
        }
    }

    pub fn destroy_allowed(&self) -> Result<(), ObjectError> {
        allow_interface(self.interface_caps, VM_DESTROY)
    }
}

pub struct VmarDeniedGuard<'a> {
    _vmar: &'a Vmar,
}

impl<'a> VmarDeniedGuard<'a> {
    fn new(vmar: &'a Vmar) -> Self {
        Self { _vmar: vmar }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct VmarNode {
    len: usize,
    prefix_free: usize,
    suffix_free: usize,
    max_free: usize,
    fill: Option<bool>,
}

impl VmarNode {
    fn free() -> Self {
        Self {
            fill: Some(true),
            ..Self::default()
        }
    }

    fn used() -> Self {
        Self {
            fill: Some(false),
            ..Self::default()
        }
    }

    fn fully_free(&self) -> bool {
        self.max_free == self.len
    }
}

impl SegmentState for VmarNode {
    fn merge(left: &Self, right: &Self) -> Self {
        let len = left.len + right.len;
        let prefix_free = if left.prefix_free == left.len {
            left.len + right.prefix_free
        } else {
            left.prefix_free
        };
        let suffix_free = if right.suffix_free == right.len {
            right.len + left.suffix_free
        } else {
            right.suffix_free
        };
        let max_free = left
            .max_free
            .max(right.max_free)
            .max(left.suffix_free + right.prefix_free);
        Self {
            len,
            prefix_free,
            suffix_free,
            max_free,
            fill: None,
        }
    }

    fn apply(&mut self, lazy_value: &Self, len: usize) {
        self.len = len;
        match lazy_value.fill {
            Some(true) => {
                self.prefix_free = len;
                self.suffix_free = len;
                self.max_free = len;
                self.fill = Some(true);
            }
            Some(false) => {
                self.prefix_free = 0;
                self.suffix_free = 0;
                self.max_free = 0;
                self.fill = Some(false);
            }
            None => {}
        }
    }
}

struct VmarState {
    page_count: usize,
    tree: SegmentTree<VmarNode>,
    entries: BTreeMap<usize, Arc<VmarEntry>>,
}

impl core::fmt::Debug for VmarState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VmarState")
            .field("page_count", &self.page_count)
            .field("entry_count", &self.entries.len())
            .finish()
    }
}

impl VmarState {
    fn new(page_count: usize) -> Self {
        let mut tree = SegmentTree::new(0..page_count.max(1));
        if page_count != 0 {
            tree.update(0..page_count, VmarNode::free())
                .expect("VMAR free-space tree initialization must succeed");
        }
        Self {
            page_count,
            tree,
            entries: BTreeMap::new(),
        }
    }
}

/// One address-space region manager node.
struct VmarShared {
    range: VmRange,
    inner: SpinRwLock<VmarState, ScopedGuard<NoOp>>,
}

/// One address-space region manager node.
#[derive(Clone)]
pub struct Vmar {
    shared: Arc<VmarShared>,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VmarMapSlowArgs {
    base: usize,
    size: usize,
    vmo_offset: usize,
    flags: u32,
    purpose: usize,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VmarProtectSlowArgs {
    base: usize,
    size: usize,
    flags: u32,
}

fn read_syscall_pod<T: Copy>(
    caller: &libakarin_object::ObjectSyscallContext,
    addr: usize,
) -> Result<T, VmInvokeError> {
    let mut value = core::mem::MaybeUninit::<T>::uninit();
    let bytes = unsafe {
        core::slice::from_raw_parts_mut(value.as_mut_ptr().cast::<u8>(), core::mem::size_of::<T>())
    };
    caller
        .copy_from_user(addr, bytes)
        .map_err(|_| VmInvokeError::Fault)?;
    Ok(unsafe { value.assume_init() })
}

impl core::fmt::Debug for Vmar {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Vmar")
            .field("range", &self.shared.range)
            .field("entries", &self.child_count())
            .finish()
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum VmarError {
    #[error("invalid range")]
    InvalidRange,
    #[error("range overlaps an existing region")]
    AlreadyMapped,
    #[error("mapping violates layout constraints")]
    PermissionDenied,
    #[error("mapping not found")]
    NotMapped,
}

impl VmarError {
    fn vm_error(self) -> VmInvokeError {
        match self {
            Self::InvalidRange => VmInvokeError::InvalidRange,
            Self::AlreadyMapped => VmInvokeError::AlreadyMapped,
            Self::PermissionDenied => VmInvokeError::PermissionDenied,
            Self::NotMapped => VmInvokeError::NotMapped,
        }
    }
}

impl From<VmarError> for VmInvokeError {
    fn from(value: VmarError) -> Self {
        value.vm_error()
    }
}

impl Vmar {
    /// Create a new VMAR covering `range`.
    pub fn new(range: VmRange) -> Self {
        let page_count = range.len() / PAGE_SIZE;
        Self {
            shared: Arc::new(VmarShared {
                range,
                inner: SpinRwLock::new(VmarState::new(page_count)),
            }),
        }
    }

    /// Return the VMAR-owned range.
    pub fn range(&self) -> VmRange {
        self.shared.range
    }

    /// Create and insert one child VMAR.
    pub fn allocate_child(&self, range: VmRange) -> Result<Arc<Vmar>, VmarError> {
        self.validate_child_range(range)?;
        let child = Arc::new(Vmar::new(range));
        self.insert_entry(VmarEntry::Region {
            range,
            child: child.as_ref().clone(),
        })?;
        Ok(child)
    }

    /// Create and insert one child VMAR using the first page-aligned gap large
    /// enough to satisfy `size`.
    pub fn allocate_child_any(&self, size: usize) -> Result<Arc<Vmar>, VmarError> {
        if size == 0 || !size.is_multiple_of(PAGE_SIZE) {
            return Err(VmarError::InvalidRange);
        }

        let candidate = {
            let mut state = self.shared.inner.write();
            let needed_pages = size / PAGE_SIZE;
            let page_count = state.page_count;
            let overall = state
                .tree
                .query(0..page_count)
                .expect("VMAR tree query must stay within range");
            if overall.max_free < needed_pages {
                return Err(VmarError::NotMapped);
            }
            let start_page = Self::find_first_fit(&mut state, 0, page_count, needed_pages)
                .ok_or(VmarError::NotMapped)?;
            let start = self.shared.range.start().as_usize() + start_page * PAGE_SIZE;
            let end = start.checked_add(size).ok_or(VmarError::InvalidRange)?;
            VmRange::new(VirtAddr::new(start), VirtAddr::new(end)).ok_or(VmarError::InvalidRange)?
        };

        self.allocate_child(candidate)
    }

    /// Insert one VMO mapping into this VMAR.
    pub fn map_vmo(
        &self,
        range: VmRange,
        vmo: Arc<Vmo>,
        vmo_offset: usize,
        flags: VmFlags,
        purpose: RegionPurpose,
    ) -> Result<(), VmarError> {
        if !range.is_page_aligned() || !vmo_offset.is_multiple_of(vmo.page_size()) {
            return Err(VmarError::InvalidRange);
        }

        let mapping = VmarMapping {
            range,
            flags,
            purpose,
            vmo,
            vmo_offset,
        };
        if !mapping.validate_layout() {
            return Err(VmarError::PermissionDenied);
        }
        self.insert_entry(VmarEntry::Mapping(mapping))
    }

    /// Reserve one guard page range inside this VMAR.
    pub fn map_guard(&self, range: VmRange) -> Result<(), VmarError> {
        if !range.is_page_aligned() {
            return Err(VmarError::InvalidRange);
        }
        let Some(segment) = VmLayoutSegment::classify_range(range) else {
            return Err(VmarError::InvalidRange);
        };
        if !segment.is_guard() {
            return Err(VmarError::PermissionDenied);
        }
        self.insert_entry(VmarEntry::Guard(range))
    }

    /// Update the protection flags of one immediate mapping entry.
    ///
    /// The current implementation only supports exact-range updates and does
    /// not split mappings yet.
    pub fn protect(&self, range: VmRange, flags: VmFlags) -> Result<(), VmarError> {
        if !range.is_page_aligned() {
            return Err(VmarError::InvalidRange);
        }

        let mut state = self.shared.inner.write();
        let Some((start_page, _end_page)) = self.page_span(range) else {
            return Err(VmarError::InvalidRange);
        };
        let Some((_, entry)) = self.find_entry(&state, start_page) else {
            return Err(VmarError::NotMapped);
        };
        let VmarEntry::Mapping(mapping) = entry.as_ref() else {
            return Err(VmarError::NotMapped);
        };
        if mapping.range != range {
            return Err(VmarError::NotMapped);
        }

        let updated = VmarMapping {
            range: mapping.range,
            flags,
            purpose: mapping.purpose,
            vmo: Arc::clone(&mapping.vmo),
            vmo_offset: mapping.vmo_offset,
        };
        if !updated.validate_layout() {
            return Err(VmarError::PermissionDenied);
        }
        state
            .entries
            .insert(start_page, Arc::new(VmarEntry::Mapping(updated)));
        Ok(())
    }

    /// Remove one immediate child entry identified by its start address.
    pub fn unmap(&self, start: VirtAddr) -> Result<VmarEntry, VmarError> {
        let mut state = self.shared.inner.write();
        let page = self.page_index(start).ok_or(VmarError::InvalidRange)?;
        let entry = state.entries.remove(&page).ok_or(VmarError::NotMapped)?;
        if entry.range().start() != start {
            state.entries.insert(page, Arc::clone(&entry));
            return Err(VmarError::NotMapped);
        }
        let range = entry.range();
        let (start_page, end_page) = self.page_span(range).ok_or(VmarError::InvalidRange)?;
        state
            .tree
            .update(start_page..end_page, VmarNode::free())
            .expect("VMAR free-space tree update must succeed");
        Ok((*entry).clone())
    }

    /// Return the resolved mapping at `addr`, descending into child VMARs.
    pub fn mapping_at(&self, addr: VirtAddr) -> Option<VmarMapping> {
        match self.locate(addr) {
            VmPointerRegion::Mapping(mapping) => Some(mapping),
            _ => None,
        }
    }

    /// Classify one virtual address within this VMAR subtree.
    pub fn locate(&self, addr: VirtAddr) -> VmPointerRegion {
        if !self.shared.range.contains(addr) {
            return match VmLayoutSegment::classify(addr) {
                Some(segment) if segment.is_guard() => VmPointerRegion::GuardPage,
                Some(segment) => VmPointerRegion::Static(segment),
                None => VmPointerRegion::Unmapped,
            };
        }

        let state = self.shared.inner.read();
        let Some(page) = self.page_index(addr) else {
            return VmPointerRegion::Reserved(self.shared.range);
        };
        if let Some((_, entry)) = self.find_entry(&state, page) {
            let range = entry.range();
            if range.contains(addr) {
                return match entry.as_ref() {
                    VmarEntry::Guard(_) => VmPointerRegion::GuardPage,
                    VmarEntry::Mapping(mapping) => VmPointerRegion::Mapping(mapping.clone()),
                    VmarEntry::Region { child, .. } => child.locate(addr),
                };
            }
        }

        VmPointerRegion::Reserved(self.shared.range)
    }

    /// Return the number of immediate child entries.
    pub fn child_count(&self) -> usize {
        let state = self.shared.inner.read();
        Self::snapshot_entries(&state).len()
    }

    /// Return a snapshot of immediate child entries for diagnostics.
    pub fn entries(&self) -> Vec<VmarEntry> {
        let state = self.shared.inner.read();
        Self::snapshot_entries(&state)
    }

    fn insert_entry(&self, entry: VmarEntry) -> Result<(), VmarError> {
        let range = entry.range();
        self.validate_child_range(range)?;

        let mut state = self.shared.inner.write();
        let (start_page, end_page) = self.page_span(range).ok_or(VmarError::InvalidRange)?;
        let status = state
            .tree
            .query(start_page..end_page)
            .expect("VMAR tree query must stay within range");
        if !status.fully_free() {
            return Err(VmarError::AlreadyMapped);
        }
        let entry = Arc::new(entry);
        state.entries.insert(start_page, entry);
        state
            .tree
            .update(start_page..end_page, VmarNode::used())
            .expect("VMAR tree occupancy update must succeed");
        Ok(())
    }

    fn validate_child_range(&self, range: VmRange) -> Result<(), VmarError> {
        if !range.is_page_aligned() || !self.shared.range.contains_range(range) {
            return Err(VmarError::InvalidRange);
        }
        Ok(())
    }

    fn page_index(&self, addr: VirtAddr) -> Option<usize> {
        self.shared
            .range
            .contains(addr)
            .then(|| (addr.as_usize() - self.shared.range.start().as_usize()) / PAGE_SIZE)
    }

    fn page_span(&self, range: VmRange) -> Option<(usize, usize)> {
        let start = self.page_index(range.start())?;
        let end = (range.end().as_usize() - self.shared.range.start().as_usize()) / PAGE_SIZE;
        let page_count = self.shared.range.len() / PAGE_SIZE;
        (start < end && end <= page_count).then_some((start, end))
    }

    fn snapshot_entries(state: &VmarState) -> Vec<VmarEntry> {
        state
            .entries
            .values()
            .map(|entry| (**entry).clone())
            .collect()
    }

    fn find_entry(&self, state: &VmarState, page: usize) -> Option<(usize, Arc<VmarEntry>)> {
        let (start_page, entry) = state.entries.range(..=page).next_back()?;
        let (entry_start, entry_end) = self.page_span_for_range(state.page_count, entry.range())?;
        (entry_start <= page && page < entry_end).then_some((*start_page, Arc::clone(entry)))
    }

    fn page_span_for_range(&self, page_count: usize, range: VmRange) -> Option<(usize, usize)> {
        let base = self.shared.range.start().as_usize();
        let start = range.start().as_usize().checked_sub(base)? / PAGE_SIZE;
        let end = range.end().as_usize().checked_sub(base)? / PAGE_SIZE;
        (start < end && end <= page_count).then_some((start, end))
    }

    fn find_first_fit(
        state: &mut VmarState,
        start: usize,
        end: usize,
        needed_pages: usize,
    ) -> Option<usize> {
        if end.saturating_sub(start) < needed_pages {
            return None;
        }

        let summary = state.tree.query(start..end).ok()?;
        if summary.max_free < needed_pages {
            return None;
        }
        if end - start == needed_pages {
            return Some(start);
        }

        let mid = start + (end - start) / 2;
        if mid > start
            && let Some(found) = Self::find_first_fit(state, start, mid, needed_pages)
        {
            return Some(found);
        }

        let left = state.tree.query(start..mid).ok()?;
        let right = state.tree.query(mid..end).ok()?;
        if left.suffix_free + right.prefix_free >= needed_pages {
            return Some(mid - left.suffix_free);
        }

        (mid < end).then_some(())?;
        Self::find_first_fit(state, mid, end, needed_pages)
    }
}

impl ControlPlane for Vmar {
    type ReadGuard<'a>
        = VmarReadGuard<'a>
    where
        Self: 'a;
    type WriteGuard<'a>
        = VmarWriteGuard<'a>
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = VmarDeniedGuard<'a>
    where
        Self: 'a;
    type AgentGuard<'a>
        = VmarDeniedGuard<'a>
    where
        Self: 'a;
    type AdminGuard<'a>
        = VmarAdminGuard<'a>
    where
        Self: 'a;

    fn read(&self, interface_caps: u32) -> Self::ReadGuard<'_> {
        VmarReadGuard::new(self, interface_caps)
    }

    fn write(&self, interface_caps: u32) -> Self::WriteGuard<'_> {
        VmarWriteGuard::new(self, interface_caps)
    }

    fn execute(&self, _interface_caps: u32) -> Self::ExecuteGuard<'_> {
        VmarDeniedGuard::new(self)
    }

    fn agent(&self, _interface_caps: u32) -> Self::AgentGuard<'_> {
        VmarDeniedGuard::new(self)
    }

    fn admin(&self, interface_caps: u32) -> Self::AdminGuard<'_> {
        VmarAdminGuard::new(self, interface_caps)
    }
}

#[async_trait]
impl SyscallDispatch<libakarin_object::ObjectSyscallContext> for VmarReadGuard<'_> {
    async fn dispatch(
        &self,
        _caller: &libakarin_object::ObjectSyscallContext,
        method_id: usize,
        _arg1: usize,
        _arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let Ok(method) = VmarMethod::try_from(method_id) else {
            return Ok(VmInvokeFrame::vm_error(VmInvokeError::InvalidArgument));
        };
        match method {
            VmarMethod::OpRange => Ok(VmInvokeFrame::vm_error(VmInvokeError::InvalidArgument)),
            _ => Err(ObjectError::InsufficientCapabilities),
        }
    }
}

#[async_trait]
impl SyscallDispatch<libakarin_object::ObjectSyscallContext> for VmarWriteGuard<'_> {
    async fn dispatch(
        &self,
        caller: &libakarin_object::ObjectSyscallContext,
        method_id: usize,
        arg1: usize,
        arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let Ok(method) = VmarMethod::try_from(method_id) else {
            return Ok(VmInvokeFrame::vm_error(VmInvokeError::InvalidArgument));
        };
        match method {
            VmarMethod::Allocate => {
                if arg2 == 0 {
                    return Ok(VmInvokeFrame::vm_error(VmInvokeError::InvalidArgument));
                }
                allow_interface(self.interface_caps, VM_ALLOCATE)?;
                let child = if arg1 == 0 {
                    match self.vmar.allocate_child_any(arg2) {
                        Ok(child) => child,
                        Err(error) => return Ok(VmInvokeFrame::vm_error(error.vm_error())),
                    }
                } else {
                    let Some(end) = arg1.checked_add(arg2) else {
                        return Ok(VmInvokeFrame::vm_error(VmInvokeError::InvalidArgument));
                    };
                    let range = VmRange::new(VirtAddr::new(arg1), VirtAddr::new(end))
                        .ok_or(VmInvokeError::InvalidArgument);
                    let range: VmRange = match range {
                        Ok(range) => range,
                        Err(error) => return Ok(VmInvokeFrame::vm_error(error)),
                    };
                    match self.vmar.allocate_child(range) {
                        Ok(child) => child,
                        Err(error) => return Ok(VmInvokeFrame::vm_error(error.vm_error())),
                    }
                };
                let range = child.range();
                let slot = caller.create_anonymous_object(
                    libakarin_object::Payload::new(child.as_ref().clone()),
                    libakarin_object::Capability::READ
                        | libakarin_object::Capability::WRITE
                        | libakarin_object::Capability::EXECUTE,
                    VMAR_DEFAULT_INTERFACE_CAPS,
                )?;
                Ok(VmInvokeFrame::ok([
                    slot as usize,
                    range.start().as_usize(),
                    range.len(),
                    0,
                    0,
                ]))
            }
            VmarMethod::Map => {
                allow_interface(self.interface_caps, VM_MAP)?;
                let args = match read_syscall_pod::<VmarMapSlowArgs>(caller, arg1) {
                    Ok(args) => args,
                    Err(error) => return Ok(VmInvokeFrame::vm_error(error)),
                };
                let Some(flags) = VmFlags::from_bits(args.flags) else {
                    return Ok(VmInvokeFrame::vm_error(VmInvokeError::InvalidArgument));
                };
                let purpose = match RegionPurpose::try_from(args.purpose) {
                    Ok(purpose) => purpose,
                    Err(_) => return Ok(VmInvokeFrame::vm_error(VmInvokeError::InvalidArgument)),
                };
                let end = args
                    .base
                    .checked_add(args.size)
                    .ok_or(VmInvokeError::InvalidArgument);
                let end = match end {
                    Ok(end) => end,
                    Err(error) => return Ok(VmInvokeFrame::vm_error(error)),
                };
                let range = VmRange::new(VirtAddr::new(args.base), VirtAddr::new(end))
                    .ok_or(VmInvokeError::InvalidArgument);
                let range: VmRange = match range {
                    Ok(range) => range,
                    Err(error) => return Ok(VmInvokeFrame::vm_error(error)),
                };
                let vmo_handle = caller.acquire_handle(arg2 as u32)?;
                let vmo = vmo_handle.read_cp_with::<Vmo, _, _>(|vmo| vmo.share())??;
                if let Err(error) = self
                    .vmar
                    .map_vmo(range, vmo, args.vmo_offset, flags, purpose)
                {
                    return Ok(VmInvokeFrame::vm_error(error.vm_error()));
                }
                Ok(VmInvokeFrame::ok([
                    range.start().as_usize(),
                    range.len(),
                    0,
                    0,
                    0,
                ]))
            }
            VmarMethod::Protect => {
                allow_interface(self.interface_caps, VM_PROTECT)?;
                let args = match read_syscall_pod::<VmarProtectSlowArgs>(caller, arg1) {
                    Ok(args) => args,
                    Err(error) => return Ok(VmInvokeFrame::vm_error(error)),
                };
                let Some(flags) = VmFlags::from_bits(args.flags) else {
                    return Ok(VmInvokeFrame::vm_error(VmInvokeError::InvalidArgument));
                };
                let end = args
                    .base
                    .checked_add(args.size)
                    .ok_or(VmInvokeError::InvalidArgument);
                let end = match end {
                    Ok(end) => end,
                    Err(error) => return Ok(VmInvokeFrame::vm_error(error)),
                };
                let range = VmRange::new(VirtAddr::new(args.base), VirtAddr::new(end))
                    .ok_or(VmInvokeError::InvalidArgument);
                let range: VmRange = match range {
                    Ok(range) => range,
                    Err(error) => return Ok(VmInvokeFrame::vm_error(error)),
                };
                let Some(old) = self.vmar.mapping_at(range.start()) else {
                    return Ok(VmInvokeFrame::vm_error(VmInvokeError::NotMapped));
                };
                if old.range != range {
                    return Ok(VmInvokeFrame::vm_error(VmInvokeError::NotMapped));
                }
                if let Err(error) = self.vmar.protect(range, flags) {
                    return Ok(VmInvokeFrame::vm_error(error.vm_error()));
                }
                Ok(VmInvokeFrame::empty_ok())
            }
            VmarMethod::Unmap => {
                allow_interface(self.interface_caps, VM_UNMAP)?;
                let removed = match self.vmar.unmap(VirtAddr::new(arg1)) {
                    Ok(entry) => entry,
                    Err(error) => return Ok(VmInvokeFrame::vm_error(error.vm_error())),
                };
                Ok(VmInvokeFrame::ok([removed.range().len(), 0, 0, 0, 0]))
            }
            _ => Err(ObjectError::InsufficientCapabilities),
        }
    }
}

#[async_trait]
impl SyscallDispatch<libakarin_object::ObjectSyscallContext> for VmarAdminGuard<'_> {
    async fn dispatch(
        &self,
        caller: &libakarin_object::ObjectSyscallContext,
        method_id: usize,
        arg1: usize,
        _arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let Ok(method) = VmarMethod::try_from(method_id) else {
            return Ok(VmInvokeFrame::vm_error(VmInvokeError::InvalidArgument));
        };
        match method {
            VmarMethod::Destroy => {
                self.destroy_allowed()?;
                caller.destroy_anonymous_object(arg1 as u32)?;
                Ok(VmInvokeFrame::empty_ok())
            }
            _ => Err(ObjectError::InsufficientCapabilities),
        }
    }
}

#[async_trait]
impl SyscallDispatch<libakarin_object::ObjectSyscallContext> for VmarDeniedGuard<'_> {
    async fn dispatch(
        &self,
        _caller: &libakarin_object::ObjectSyscallContext,
        _method_id: usize,
        _arg1: usize,
        _arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        Err(ObjectError::InsufficientCapabilities)
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;

    use super::*;
    use crate::memory::VmFlags;

    #[test]
    fn mapping_layout_accepts_cache_bits_when_access_matches() {
        let range = VmRange::new(
            VirtAddr::new(0x0000_0000_0040_0000),
            VirtAddr::new(0x0000_0000_0040_1000),
        )
        .unwrap();
        let vmo = Arc::new(Vmo::new(
            "test",
            PAGE_SIZE,
            PAGE_SIZE,
            VmFlags::READ | VmFlags::WRITE | VmFlags::MAP,
        ));
        let mapping = VmarMapping {
            range,
            flags: (VmFlags::READ | VmFlags::WRITE | VmFlags::USER)
                .with_cache_policy(libakarin_machine_core::memory::paging::CachePolicy::Cached),
            purpose: RegionPurpose::User,
            vmo,
            vmo_offset: 0,
        };
        assert!(mapping.validate_layout());
    }

    #[test]
    fn allocate_child_any_uses_first_free_gap() {
        let root = Vmar::new(
            VmRange::new(
                VirtAddr::new(0x0000_0000_0040_0000),
                VirtAddr::new(0x0000_0000_0040_8000),
            )
            .unwrap(),
        );
        let existing = Arc::new(Vmo::new(
            "mapped",
            PAGE_SIZE,
            PAGE_SIZE,
            VmFlags::READ | VmFlags::WRITE | VmFlags::MAP,
        ));
        root.map_vmo(
            VmRange::new(
                VirtAddr::new(0x0000_0000_0040_1000),
                VirtAddr::new(0x0000_0000_0040_2000),
            )
            .unwrap(),
            existing,
            0,
            VmFlags::READ | VmFlags::WRITE | VmFlags::USER,
            RegionPurpose::User,
        )
        .unwrap();

        let child = root.allocate_child_any(PAGE_SIZE).unwrap();
        assert_eq!(
            child.range(),
            VmRange::new(
                VirtAddr::new(0x0000_0000_0040_0000),
                VirtAddr::new(0x0000_0000_0040_1000),
            )
            .unwrap()
        );
    }

    #[test]
    fn protect_updates_exact_mapping_flags() {
        let root = Vmar::new(
            VmRange::new(
                VirtAddr::new(0x0000_1000_0000_0000),
                VirtAddr::new(0x0000_1000_0000_3000),
            )
            .unwrap(),
        );
        let range = VmRange::new(
            VirtAddr::new(0x0000_1000_0000_0000),
            VirtAddr::new(0x0000_1000_0000_1000),
        )
        .unwrap();
        let vmo = Arc::new(Vmo::new(
            "mapped",
            PAGE_SIZE,
            PAGE_SIZE,
            VmFlags::READ | VmFlags::WRITE | VmFlags::MAP,
        ));
        root.map_vmo(
            range,
            vmo,
            0,
            VmFlags::READ | VmFlags::WRITE | VmFlags::USER,
            RegionPurpose::User,
        )
        .unwrap();

        root.protect(range, VmFlags::READ | VmFlags::USER).unwrap();
        let mapping = root
            .mapping_at(VirtAddr::new(0x0000_1000_0000_0000))
            .expect("mapping should remain present");
        assert_eq!(mapping.flags, VmFlags::READ | VmFlags::USER);
    }

    #[test]
    fn mapping_resolve_fault_reports_permission_denied_before_vmo_probe() {
        let range = VmRange::new(
            VirtAddr::new(0x0000_1000_0000_0000),
            VirtAddr::new(0x0000_1000_0000_1000),
        )
        .unwrap();
        let mapping = VmarMapping {
            range,
            flags: VmFlags::READ | VmFlags::USER,
            purpose: RegionPurpose::User,
            vmo: Arc::new(Vmo::new(
                "mapped",
                PAGE_SIZE,
                PAGE_SIZE,
                VmFlags::READ | VmFlags::WRITE | VmFlags::MAP,
            )),
            vmo_offset: 0,
        };
        assert_eq!(
            mapping.resolve_fault(
                VirtAddr::new(0x0000_1000_0000_0080),
                MMUFlags::WRITE | MMUFlags::USER
            ),
            VmFaultResolution::ProtectionDenied
        );
    }

    #[test]
    fn mapping_resolve_fault_reports_unresolved_when_backing_is_missing() {
        let range = VmRange::new(
            VirtAddr::new(0x0000_1000_0000_0000),
            VirtAddr::new(0x0000_1000_0000_1000),
        )
        .unwrap();
        let mapping = VmarMapping {
            range,
            flags: VmFlags::READ | VmFlags::USER,
            purpose: RegionPurpose::User,
            vmo: Arc::new(Vmo::new(
                "mapped",
                PAGE_SIZE,
                PAGE_SIZE,
                VmFlags::READ | VmFlags::MAP,
            )),
            vmo_offset: 0,
        };
        assert_eq!(
            mapping.resolve_fault(
                VirtAddr::new(0x0000_1000_0000_0040),
                MMUFlags::READ | MMUFlags::USER
            ),
            VmFaultResolution::Unresolved
        );
    }
}
