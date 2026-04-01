use alloc::{boxed::Box, format, string::String, sync::Arc, vec, vec::Vec};
use core::{
    convert::TryFrom,
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;
use libakarin_machine_core::{
    memory::{
        AddressSpaceTrait, FrameAllocatorTrait, FrameZone, PhysAddr, PhysRun,
        paging::{MMUFlags, PhysFrameTrait},
    },
    sync::{NoOp, ScopedGuard},
};
use libakarin_object::{ControlPlane, ObjectError, SyscallDispatch};
use libakarin_sync::spin::SpinRwLock;
use libakarin_syscall::SyscallResult;

use crate::memory::{VmControlError, VmFlags, VmInvokeError, VmInvokeFrame};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmoPagePurpose {
    Anonymous,
    PageTable,
    Stack,
    Heap,
    Dma,
    FileBacked,
    KernelMeta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmoPageMetadata {
    pub phys: PhysAddr,
    pub purpose: VmoPagePurpose,
    pub committed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmoBacking {
    Paged,
    Physical { base: PhysAddr },
}

/// Child VMO flavor describing how one derived view relates to its parent.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmoChildKind {
    /// The child is one fixed-size shared subview into the parent range.
    SharedView = 1,
    /// The child starts as one shared view and may later diverge on writes.
    PrivateCow = 2,
}

/// Fault-servicing strategy currently associated with one VMO.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmoFaultPolicy {
    /// The VMO expects pages to already be available to the current mapping.
    Immediate = 1,
    /// The VMO will eventually satisfy writes by creating one private copy.
    PrivateCow = 2,
    /// The VMO will eventually fetch backing pages from one pager endpoint.
    PagerBacked = 3,
}

/// Classification returned by one VMO fault probe.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmFaultResolution {
    /// The VMO already has backing for the requested page and only needs one
    /// future page-table install step.
    RetryAfterMap = 1,
    /// The requested access violates the VMO-level access policy.
    ProtectionDenied = 2,
    /// The VMO has no backing for the requested page and no recovery policy.
    Unresolved = 3,
    /// The VMO would need one future private-copy fault path.
    PrivateCow = 4,
    /// The VMO would need one future pager round-trip.
    PagerBacked = 5,
    /// The fault does not address one valid byte inside this VMO.
    InvalidRange = 6,
}

/// Stable metadata describing one child VMO and the parent range it covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmoChildInfo {
    /// Child flavor currently active for this derived VMO.
    pub kind: VmoChildKind,
    /// Stable identifier of the parent VMO that still owns the backing range.
    pub parent_id: u64,
    /// Parent-relative start offset of the child window.
    pub parent_offset: usize,
    /// Fixed logical size of the child window in bytes.
    pub size: usize,
    /// Current logical stream size visible through the child window.
    pub stream_size: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmoRangeOpError {
    InvalidArgument,
    InvalidRange,
}

impl From<VmoRangeOpError> for VmInvokeError {
    fn from(value: VmoRangeOpError) -> Self {
        match value {
            VmoRangeOpError::InvalidArgument => VmInvokeError::InvalidArgument,
            VmoRangeOpError::InvalidRange => VmInvokeError::InvalidRange,
        }
    }
}

#[derive(Debug)]
struct VmoExtent {
    offset: usize,
    frame: PhysRun,
    purpose: VmoPagePurpose,
    committed: bool,
}

impl VmoExtent {
    fn end(&self) -> usize {
        self.offset + self.frame.len_bytes()
    }

    fn contains(&self, offset: usize) -> bool {
        self.offset <= offset && offset < self.end()
    }

    fn metadata_at(&self, offset: usize) -> Option<VmoPageMetadata> {
        self.contains(offset).then_some(VmoPageMetadata {
            phys: PhysAddr::new(self.frame.phys_addr().as_usize() + (offset - self.offset)),
            purpose: self.purpose,
            committed: self.committed,
        })
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct RangeOwner(Option<usize>);

#[derive(Default)]
struct VmoRangeIndex {
    coords: Vec<usize>,
    owners: Vec<RangeOwner>,
}

impl core::fmt::Debug for VmoRangeIndex {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VmoRangeIndex")
            .field("coords", &self.coords)
            .field("owners", &self.owners.len())
            .finish()
    }
}

impl VmoRangeIndex {
    fn rebuild(&mut self, extents: &[(usize, usize, usize)]) {
        self.coords.clear();
        self.owners.clear();
        for &(_, start, end) in extents {
            self.coords.push(start);
            self.coords.push(end);
        }
        self.coords.sort_unstable();
        self.coords.dedup();

        if self.coords.len() < 2 {
            return;
        }

        self.owners = vec![RangeOwner(None); self.coords.len() - 1];
        for &(owner, start_offset, end_offset) in extents {
            let Some(start) = self.coord_index(start_offset) else {
                continue;
            };
            let Some(end) = self.coord_index(end_offset) else {
                continue;
            };
            for leaf in &mut self.owners[start..end] {
                *leaf = RangeOwner(Some(owner));
            }
        }
    }

    fn owner_at(&self, offset: usize) -> Option<usize> {
        if self.coords.len() < 2 {
            return None;
        }

        let leaf = match self.coords.binary_search(&offset) {
            Ok(index) if index + 1 < self.coords.len() => index,
            Ok(_) => return None,
            Err(0) => return None,
            Err(index) if index >= self.coords.len() => return None,
            Err(index) => index - 1,
        };
        if !(self.coords[leaf] <= offset && offset < self.coords[leaf + 1]) {
            return None;
        }
        self.owners.get(leaf).and_then(|owner| owner.0)
    }

    fn coord_index(&self, value: usize) -> Option<usize> {
        self.coords.binary_search(&value).ok()
    }
}

#[derive(Debug)]
struct VmoInner {
    size: usize,
    stream_size: usize,
    flags: VmFlags,
    pager_cookie: Option<usize>,
    extents: Vec<VmoExtent>,
    index: VmoRangeIndex,
}

impl Default for VmoInner {
    fn default() -> Self {
        Self {
            size: 0,
            stream_size: 0,
            flags: VmFlags::empty(),
            pager_cookie: None,
            extents: Vec::new(),
            index: VmoRangeIndex::default(),
        }
    }
}

static NEXT_VMO_ID: AtomicU64 = AtomicU64::new(1);

/// Object-specific VMO slow-path method identifiers.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmoMethod {
    CreateChild = 0,
    GetSize = 1,
    GetStreamSize = 2,
    OpRange = 3,
    Read = 4,
    ReplaceAsExecutable = 5,
    SetCachePolicy = 6,
    SetSize = 7,
    SetStreamSize = 8,
    TransferData = 9,
    Write = 10,
    /// Return the parent-window metadata of one child VMO.
    QueryChild = 11,
}

pub const V_READ_DATA: u32 = 1 << 0;
pub const V_WRITE_DATA: u32 = 1 << 1;
pub const V_GET_SIZE: u32 = 1 << 2;
pub const V_SET_SIZE: u32 = 1 << 3;
pub const V_SET_POLICY: u32 = 1 << 4;
pub const V_TRANSFER: u32 = 1 << 5;
pub const V_CREATE_CHILD: u32 = 1 << 6;

pub const VMO_DEFAULT_INTERFACE_CAPS: u32 =
    V_READ_DATA | V_WRITE_DATA | V_GET_SIZE | V_SET_SIZE | V_SET_POLICY | V_CREATE_CHILD;
pub const VMO_ADMIN_INTERFACE_CAPS: u32 = V_TRANSFER;

impl TryFrom<usize> for VmoMethod {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            x if x == Self::CreateChild as usize => Ok(Self::CreateChild),
            x if x == Self::GetSize as usize => Ok(Self::GetSize),
            x if x == Self::GetStreamSize as usize => Ok(Self::GetStreamSize),
            x if x == Self::OpRange as usize => Ok(Self::OpRange),
            x if x == Self::Read as usize => Ok(Self::Read),
            x if x == Self::ReplaceAsExecutable as usize => Ok(Self::ReplaceAsExecutable),
            x if x == Self::SetCachePolicy as usize => Ok(Self::SetCachePolicy),
            x if x == Self::SetSize as usize => Ok(Self::SetSize),
            x if x == Self::SetStreamSize as usize => Ok(Self::SetStreamSize),
            x if x == Self::TransferData as usize => Ok(Self::TransferData),
            x if x == Self::Write as usize => Ok(Self::Write),
            x if x == Self::QueryChild as usize => Ok(Self::QueryChild),
            _ => Err(()),
        }
    }
}

struct VmoShared {
    id: u64,
    name: String,
    page_size: usize,
    backing: VmoBacking,
    child: Option<VmoChildState>,
    inner: SpinRwLock<VmoInner, ScopedGuard<NoOp>>,
}

#[derive(Clone)]
struct VmoChildState {
    parent: Vmo,
    parent_offset: usize,
    kind: VmoChildKind,
}

impl fmt::Debug for VmoShared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VmoShared")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("page_size", &self.page_size)
            .field("backing", &self.backing)
            .finish_non_exhaustive()
    }
}

/// Virtual-memory object backing one logical collection of pages.
#[derive(Clone)]
pub struct Vmo {
    shared: Arc<VmoShared>,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VmoIoArgs {
    offset: usize,
    buffer: usize,
    len: usize,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VmoTransferArgs {
    src_slot: u32,
    _reserved: u32,
    src_offset: usize,
    dst_offset: usize,
    len: usize,
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

fn allow_interface(interface_caps: u32, required: u32) -> Result<(), ObjectError> {
    if interface_caps == u32::MAX || (interface_caps & required) == required {
        Ok(())
    } else {
        Err(ObjectError::InsufficientCapabilities)
    }
}

pub struct VmoReadGuard<'a> {
    vmo: &'a Vmo,
    interface_caps: u32,
}

impl<'a> VmoReadGuard<'a> {
    fn new(vmo: &'a Vmo, interface_caps: u32) -> Self {
        Self {
            vmo,
            interface_caps,
        }
    }

    pub fn size(&self) -> Result<usize, ObjectError> {
        allow_interface(self.interface_caps, V_GET_SIZE)?;
        Ok(self.vmo.size())
    }

    pub fn stream_size(&self) -> Result<usize, ObjectError> {
        allow_interface(self.interface_caps, V_GET_SIZE)?;
        Ok(self.vmo.stream_size())
    }

    pub fn read(&self, offset: usize, buffer: &mut [u8]) -> Result<usize, ObjectError> {
        allow_interface(self.interface_caps, V_READ_DATA)?;
        if self.vmo.read(offset, buffer) {
            Ok(buffer.len())
        } else {
            Err(ObjectError::InvalidArgument)
        }
    }

    /// Read one byte range while preserving VM subsystem errors.
    pub fn read_vm(&self, offset: usize, buffer: &mut [u8]) -> Result<usize, VmControlError> {
        allow_interface(self.interface_caps, V_READ_DATA).map_err(VmControlError::Object)?;
        if self.vmo.read(offset, buffer) {
            Ok(buffer.len())
        } else {
            Err(VmControlError::Vm(VmInvokeError::InvalidArgument))
        }
    }

    /// Return the parent-range metadata when this VMO is one derived child.
    pub fn child_info(&self) -> Result<Option<VmoChildInfo>, ObjectError> {
        allow_interface(self.interface_caps, V_GET_SIZE)?;
        let child = match self.vmo.child_info() {
            Some(child) => child,
            None => return Ok(None),
        };
        Ok(Some(child))
    }

    /// Report the currently committed byte count across one page-aligned VMO
    /// range.
    pub fn query_committed_bytes(&self, offset: usize, len: usize) -> Result<usize, ObjectError> {
        allow_interface(self.interface_caps, V_GET_SIZE)?;
        self.vmo
            .query_committed_bytes(offset, len)
            .map_err(|error| match error {
                VmoRangeOpError::InvalidArgument => ObjectError::InvalidArgument,
                VmoRangeOpError::InvalidRange => ObjectError::ObjectNotFound,
            })
    }

    /// Report committed bytes without re-encoding VM range errors as object
    /// errors.
    pub fn query_committed_bytes_vm(
        &self,
        offset: usize,
        len: usize,
    ) -> Result<usize, VmControlError> {
        allow_interface(self.interface_caps, V_GET_SIZE).map_err(VmControlError::Object)?;
        self.vmo
            .query_committed_bytes(offset, len)
            .map_err(|error| VmControlError::Vm(error.into()))
    }

    /// Share one VMO reference for composed VM operations such as `VMAR.map`.
    pub fn share(&self) -> Result<Arc<Vmo>, ObjectError> {
        Ok(Arc::new(self.vmo.clone()))
    }

    /// Share one VMO reference for composed VM operations while preserving the
    /// mixed VM/object error model.
    pub fn share_vm(&self) -> Result<Arc<Vmo>, VmControlError> {
        Ok(Arc::new(self.vmo.clone()))
    }
}

pub struct VmoWriteGuard<'a> {
    vmo: &'a Vmo,
    interface_caps: u32,
}

impl<'a> VmoWriteGuard<'a> {
    fn new(vmo: &'a Vmo, interface_caps: u32) -> Self {
        Self {
            vmo,
            interface_caps,
        }
    }

    pub fn write(&self, offset: usize, data: &[u8]) -> Result<usize, ObjectError> {
        allow_interface(self.interface_caps, V_WRITE_DATA)?;
        if self.vmo.write(offset, data) {
            Ok(data.len())
        } else {
            Err(ObjectError::InvalidArgument)
        }
    }

    /// Write one byte range while preserving VM subsystem errors.
    pub fn write_vm(&self, offset: usize, data: &[u8]) -> Result<usize, VmControlError> {
        allow_interface(self.interface_caps, V_WRITE_DATA).map_err(VmControlError::Object)?;
        if self.vmo.write(offset, data) {
            Ok(data.len())
        } else {
            Err(VmControlError::Vm(VmInvokeError::InvalidArgument))
        }
    }

    pub fn set_size(&self, size: usize) -> Result<(usize, usize), ObjectError> {
        allow_interface(self.interface_caps, V_SET_SIZE)?;
        if self.vmo.set_size(size) {
            Ok((self.vmo.size(), self.vmo.stream_size()))
        } else {
            Err(ObjectError::InvalidArgument)
        }
    }

    /// Resize one VMO while preserving the VM subsystem error code.
    pub fn set_size_vm(&self, size: usize) -> Result<(usize, usize), VmControlError> {
        allow_interface(self.interface_caps, V_SET_SIZE).map_err(VmControlError::Object)?;
        if self.vmo.set_size(size) {
            Ok((self.vmo.size(), self.vmo.stream_size()))
        } else {
            Err(VmControlError::Vm(VmInvokeError::InvalidArgument))
        }
    }

    pub fn set_stream_size(&self, size: usize) -> Result<usize, ObjectError> {
        allow_interface(self.interface_caps, V_SET_SIZE)?;
        if self.vmo.set_stream_size(size) {
            Ok(self.vmo.stream_size())
        } else {
            Err(ObjectError::InvalidArgument)
        }
    }

    /// Update one VMO stream size without routing VM errors through
    /// `ObjectError`.
    pub fn set_stream_size_vm(&self, size: usize) -> Result<usize, VmControlError> {
        allow_interface(self.interface_caps, V_SET_SIZE).map_err(VmControlError::Object)?;
        if self.vmo.set_stream_size(size) {
            Ok(self.vmo.stream_size())
        } else {
            Err(VmControlError::Vm(VmInvokeError::InvalidArgument))
        }
    }

    pub fn set_cache_policy(
        &self,
        policy: libakarin_machine_core::memory::paging::CachePolicy,
    ) -> Result<VmFlags, ObjectError> {
        allow_interface(self.interface_caps, V_SET_POLICY)?;
        self.vmo.set_cache_policy(policy);
        Ok(self.vmo.flags())
    }

    /// Update the stored cache policy while preserving the VM error channel.
    pub fn set_cache_policy_vm(
        &self,
        policy: libakarin_machine_core::memory::paging::CachePolicy,
    ) -> Result<VmFlags, VmControlError> {
        allow_interface(self.interface_caps, V_SET_POLICY).map_err(VmControlError::Object)?;
        self.vmo.set_cache_policy(policy);
        Ok(self.vmo.flags())
    }

    pub fn create_child(&self, offset: usize, size: usize) -> Result<Vmo, ObjectError> {
        allow_interface(self.interface_caps, V_CREATE_CHILD)?;
        match self
            .vmo
            .create_child(format!("{}-child", self.vmo.name()), offset, size)
        {
            Some(child) => Ok(child),
            None => Err(ObjectError::InvalidArgument),
        }
    }

    /// Create one shared child view while preserving VM subsystem errors.
    pub fn create_child_vm(&self, offset: usize, size: usize) -> Result<Vmo, VmControlError> {
        allow_interface(self.interface_caps, V_CREATE_CHILD).map_err(VmControlError::Object)?;
        match self
            .vmo
            .create_child(format!("{}-child", self.vmo.name()), offset, size)
        {
            Some(child) => Ok(child),
            None => Err(VmControlError::Vm(VmInvokeError::InvalidRange)),
        }
    }

    /// Create one private COW child VMO over the selected parent window.
    pub fn create_private_child(&self, offset: usize, size: usize) -> Result<Vmo, ObjectError> {
        allow_interface(self.interface_caps, V_CREATE_CHILD)?;
        match self
            .vmo
            .create_private_child(format!("{}-cow", self.vmo.name()), offset, size)
        {
            Some(child) => Ok(child),
            None => Err(ObjectError::InvalidArgument),
        }
    }

    /// Create one private COW child while preserving VM subsystem errors.
    pub fn create_private_child_vm(
        &self,
        offset: usize,
        size: usize,
    ) -> Result<Vmo, VmControlError> {
        allow_interface(self.interface_caps, V_CREATE_CHILD).map_err(VmControlError::Object)?;
        match self
            .vmo
            .create_private_child(format!("{}-cow", self.vmo.name()), offset, size)
        {
            Some(child) => Ok(child),
            None => Err(VmControlError::Vm(VmInvokeError::InvalidRange)),
        }
    }

    /// Materialize committed backing pages across one page-aligned range.
    pub fn commit_range<A>(
        &self,
        offset: usize,
        len: usize,
        purpose: VmoPagePurpose,
        allocator: &'static dyn FrameAllocatorTrait,
    ) -> Result<usize, ObjectError>
    where
        A: AddressSpaceTrait,
    {
        allow_interface(self.interface_caps, V_WRITE_DATA)?;
        self.vmo
            .commit_range::<A>(offset, len, purpose, allocator)
            .map_err(|error| match error {
                VmoRangeOpError::InvalidArgument => ObjectError::InvalidArgument,
                VmoRangeOpError::InvalidRange => ObjectError::ObjectNotFound,
            })
    }

    /// Materialize backing pages without folding VM range failures into object
    /// errors.
    pub fn commit_range_vm<A>(
        &self,
        offset: usize,
        len: usize,
        purpose: VmoPagePurpose,
        allocator: &'static dyn FrameAllocatorTrait,
    ) -> Result<usize, VmControlError>
    where
        A: AddressSpaceTrait,
    {
        allow_interface(self.interface_caps, V_WRITE_DATA).map_err(VmControlError::Object)?;
        self.vmo
            .commit_range::<A>(offset, len, purpose, allocator)
            .map_err(|error| VmControlError::Vm(error.into()))
    }

    /// Remove committed backing pages across one page-aligned range.
    pub fn decommit_range(&self, offset: usize, len: usize) -> Result<usize, ObjectError> {
        allow_interface(self.interface_caps, V_WRITE_DATA)?;
        self.vmo
            .decommit_range(offset, len)
            .map_err(|error| match error {
                VmoRangeOpError::InvalidArgument => ObjectError::InvalidArgument,
                VmoRangeOpError::InvalidRange => ObjectError::ObjectNotFound,
            })
    }

    /// Remove committed backing pages without object-error re-encoding.
    pub fn decommit_range_vm(&self, offset: usize, len: usize) -> Result<usize, VmControlError> {
        allow_interface(self.interface_caps, V_WRITE_DATA).map_err(VmControlError::Object)?;
        self.vmo
            .decommit_range(offset, len)
            .map_err(|error| VmControlError::Vm(error.into()))
    }

    /// Zero every committed page already backing one page-aligned range.
    pub fn zero_range<A>(&self, offset: usize, len: usize) -> Result<usize, ObjectError>
    where
        A: AddressSpaceTrait,
    {
        allow_interface(self.interface_caps, V_WRITE_DATA)?;
        self.vmo
            .zero_range::<A>(offset, len)
            .map_err(|error| match error {
                VmoRangeOpError::InvalidArgument => ObjectError::InvalidArgument,
                VmoRangeOpError::InvalidRange => ObjectError::ObjectNotFound,
            })
    }

    /// Zero committed pages without folding VM range failures into object
    /// errors.
    pub fn zero_range_vm<A>(&self, offset: usize, len: usize) -> Result<usize, VmControlError>
    where
        A: AddressSpaceTrait,
    {
        allow_interface(self.interface_caps, V_WRITE_DATA).map_err(VmControlError::Object)?;
        self.vmo
            .zero_range::<A>(offset, len)
            .map_err(|error| VmControlError::Vm(error.into()))
    }
}

pub struct VmoAdminGuard<'a> {
    vmo: &'a Vmo,
    interface_caps: u32,
}

impl<'a> VmoAdminGuard<'a> {
    fn new(vmo: &'a Vmo, interface_caps: u32) -> Self {
        Self {
            vmo,
            interface_caps,
        }
    }

    pub fn transfer_from(
        &self,
        dst_offset: usize,
        src: &Vmo,
        src_offset: usize,
        len: usize,
    ) -> Result<usize, ObjectError> {
        allow_interface(self.interface_caps, V_TRANSFER)?;
        if self.vmo.transfer_from(dst_offset, src, src_offset, len) {
            Ok(len)
        } else {
            Err(ObjectError::InvalidArgument)
        }
    }

    /// Transfer bytes between VMOs while preserving VM subsystem errors.
    pub fn transfer_from_vm(
        &self,
        dst_offset: usize,
        src: &Vmo,
        src_offset: usize,
        len: usize,
    ) -> Result<usize, VmControlError> {
        allow_interface(self.interface_caps, V_TRANSFER).map_err(VmControlError::Object)?;
        if self.vmo.transfer_from(dst_offset, src, src_offset, len) {
            Ok(len)
        } else {
            Err(VmControlError::Vm(VmInvokeError::InvalidArgument))
        }
    }
}

pub struct VmoDeniedGuard<'a> {
    _vmo: &'a Vmo,
}

impl<'a> VmoDeniedGuard<'a> {
    fn new(vmo: &'a Vmo) -> Self {
        Self { _vmo: vmo }
    }
}

impl fmt::Debug for Vmo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vmo")
            .field("id", &self.shared.id)
            .field("name", &self.shared.name)
            .field("size", &self.size())
            .field("page_size", &self.shared.page_size)
            .field("flags", &self.flags())
            .field("backing", &self.shared.backing)
            .field("child", &self.child_info())
            .field("tracked_pages", &self.tracked_pages())
            .finish()
    }
}

impl Vmo {
    /// Create one paged VMO backed by tracked physical frames.
    pub fn new(name: impl Into<String>, size: usize, page_size: usize, flags: VmFlags) -> Self {
        Self {
            shared: Arc::new(VmoShared {
                id: NEXT_VMO_ID.fetch_add(1, Ordering::Relaxed),
                name: name.into(),
                page_size,
                backing: VmoBacking::Paged,
                child: None,
                inner: SpinRwLock::new(VmoInner {
                    size,
                    stream_size: size,
                    flags,
                    ..VmoInner::default()
                }),
            }),
        }
    }

    /// Create one VMO describing a contiguous physical range.
    pub fn new_physical<A>(
        name: impl Into<String>,
        size: usize,
        page_size: usize,
        flags: VmFlags,
        base: PhysAddr,
    ) -> Self
    where
        A: AddressSpaceTrait,
    {
        let run = PhysRun::borrowed::<A>(base, size.div_ceil(page_size));
        let mut inner = VmoInner::default();
        inner.extents.push(VmoExtent {
            offset: 0,
            frame: run,
            purpose: VmoPagePurpose::Dma,
            committed: true,
        });
        let snapshots = inner
            .extents
            .iter()
            .enumerate()
            .map(|(owner, extent)| (owner, extent.offset, extent.end()))
            .collect::<Vec<_>>();
        inner.index.rebuild(&snapshots);

        Self {
            shared: Arc::new(VmoShared {
                id: NEXT_VMO_ID.fetch_add(1, Ordering::Relaxed),
                name: name.into(),
                page_size,
                backing: VmoBacking::Physical { base },
                child: None,
                inner: SpinRwLock::new(VmoInner {
                    size,
                    stream_size: size,
                    flags,
                    pager_cookie: None,
                    extents: inner.extents,
                    index: inner.index,
                }),
            }),
        }
    }

    /// Return the stable VMO identifier.
    pub fn id(&self) -> u64 {
        self.shared.id
    }

    /// Return the debug name.
    pub fn name(&self) -> &str {
        &self.shared.name
    }

    /// Return the logical byte size.
    pub fn size(&self) -> usize {
        self.shared.inner.read().size
    }

    /// Return the page size tracked by this VMO.
    pub fn page_size(&self) -> usize {
        self.shared.page_size
    }

    /// Return the VMO flags.
    pub fn flags(&self) -> VmFlags {
        self.shared.inner.read().flags
    }

    /// Return the backing model.
    pub fn backing(&self) -> VmoBacking {
        self.shared.backing
    }

    /// Return the fault policy currently associated with this VMO.
    pub fn fault_policy(&self) -> VmoFaultPolicy {
        match self.shared.child.as_ref().map(|child| child.kind) {
            Some(VmoChildKind::PrivateCow) => VmoFaultPolicy::PrivateCow,
            _ if self.shared.inner.read().pager_cookie.is_some() => VmoFaultPolicy::PagerBacked,
            _ => VmoFaultPolicy::Immediate,
        }
    }

    /// Bind one abstract pager cookie to this VMO.
    ///
    /// This does not store any kernel-private pager object. It only marks the
    /// VMO as pager-backed and records the stable cookie forwarded to future
    /// pager fault requests.
    pub fn bind_pager(&self, cookie: usize) -> bool {
        if self.shared.child.is_some() || self.backing() != VmoBacking::Paged {
            return false;
        }

        let mut inner = self.shared.inner.write();
        if inner.pager_cookie.is_some() {
            return false;
        }
        inner.pager_cookie = Some(cookie);
        true
    }

    /// Return the abstract pager cookie recorded on this VMO, if one exists.
    pub fn pager_cookie(&self) -> Option<usize> {
        self.shared.inner.read().pager_cookie
    }

    /// Return stable metadata about the parent-backed window of this child VMO.
    pub fn child_info(&self) -> Option<VmoChildInfo> {
        let child = match &self.shared.child {
            Some(child) => child,
            None => return None,
        };
        Some(VmoChildInfo {
            kind: child.kind,
            parent_id: child.parent.id(),
            parent_offset: child.parent_offset,
            size: self.size(),
            stream_size: self.stream_size(),
        })
    }

    /// Return whether this VMO already owns one private page at `offset`.
    pub fn has_private_page_at(&self, offset: usize) -> bool {
        if !self.offset_is_page_aligned(offset) {
            return false;
        }

        match &self.shared.child {
            Some(child) if child.kind == VmoChildKind::PrivateCow => {}
            _ => return false,
        }

        let inner = self.shared.inner.read();
        inner
            .index
            .owner_at(offset)
            .and_then(|owner| inner.extents.get(owner))
            .is_some()
    }

    /// Return MMU flags that should be installed for one mapped page.
    ///
    /// Private COW children keep parent-backed pages read-only until the first
    /// write fault materializes one child-owned copy.
    pub fn mapping_mmu_flags_for_page(&self, offset: usize, requested: MMUFlags) -> MMUFlags {
        let mut flags = requested;
        let child = match &self.shared.child {
            Some(child) => child,
            None => return flags,
        };
        if child.kind != VmoChildKind::PrivateCow || !flags.contains(MMUFlags::WRITE) {
            return flags;
        }

        let page_size = self.page_size();
        let page_offset = offset / page_size * page_size;
        let inner = self.shared.inner.read();
        if inner.index.owner_at(page_offset).is_none() {
            flags.remove(MMUFlags::WRITE);
        }
        flags
    }

    /// Materialize one child-owned private page for a `PrivateCow` child VMO.
    ///
    /// The new page is copied from the visible parent page when present, or
    /// zero-filled when the parent has no backing for that offset yet.
    pub fn materialize_private_cow_page<A>(
        &self,
        offset: usize,
        fallback_purpose: VmoPagePurpose,
        allocator: &'static dyn FrameAllocatorTrait,
    ) -> Option<VmoPageMetadata>
    where
        A: AddressSpaceTrait,
    {
        let page_size = self.page_size();
        let page_offset = offset / page_size * page_size;
        if !self.offset_is_page_aligned(page_offset) {
            return None;
        }

        let child = match &self.shared.child {
            Some(child) if child.kind == VmoChildKind::PrivateCow => child.clone(),
            _ => return None,
        };

        let source_offset = child.parent_offset.checked_add(page_offset)?;
        let source_page = child.parent.page_at(source_offset);
        let frame_count = page_size / allocator.unit_page_size();
        if frame_count == 0 || frame_count * allocator.unit_page_size() != page_size {
            return None;
        }

        let mut inner = self.shared.inner.write();
        if let Some(owner) = inner.index.owner_at(page_offset) {
            return inner.extents.get(owner)?.metadata_at(page_offset);
        }

        let virt = allocator
            .alloc(None, FrameZone::default(), frame_count)
            .ok()?;
        let phys = A::virt_to_phys(virt)?;
        match source_page {
            Some(meta) => unsafe { A::copy_phys(meta.phys, phys, page_size) },
            None => unsafe { A::zero_phys(phys, page_size) },
        }

        let frame = unsafe {
            libakarin_machine_core::memory::paging::PhysFrame::<A>::from_addr(
                Some(allocator),
                phys,
                frame_count,
            )
        };
        inner.extents.push(VmoExtent {
            offset: page_offset,
            frame: PhysRun::from_frame(frame),
            purpose: source_page
                .map(|meta| meta.purpose)
                .unwrap_or(fallback_purpose),
            committed: true,
        });
        let snapshots = inner
            .extents
            .iter()
            .enumerate()
            .map(|(owner, extent)| (owner, extent.offset, extent.end()))
            .collect::<Vec<_>>();
        inner.index.rebuild(&snapshots);
        inner
            .index
            .owner_at(page_offset)
            .and_then(|owner| inner.extents.get(owner))
            .and_then(|extent| extent.metadata_at(page_offset))
    }

    /// Classify one page fault against this VMO without mutating any state.
    ///
    /// This stage intentionally only describes what kind of servicing would be
    /// required. The current kernel still terminates every user page fault, but
    /// later phases will use this result to decide between page-table install,
    /// private-copy, and pager round-trips.
    pub fn resolve_fault(&self, offset: usize, access: MMUFlags) -> VmFaultResolution {
        if !self.contains_offset(offset) {
            return VmFaultResolution::InvalidRange;
        }

        let mut required = VmFlags::from_mmu_flags(access);
        required.remove(VmFlags::USER | VmFlags::DEVICE | VmFlags::GLOBAL | VmFlags::HUGE_PAGE);
        if !self.can_map(required) {
            return VmFaultResolution::ProtectionDenied;
        }

        let page_size = self.page_size();
        let page_offset = offset & !(page_size - 1);
        if matches!(
            self.shared.child.as_ref().map(|child| child.kind),
            Some(VmoChildKind::PrivateCow)
        ) && access.contains(MMUFlags::WRITE)
            && !self
                .shared
                .inner
                .read()
                .index
                .owner_at(page_offset)
                .is_some()
        {
            return VmFaultResolution::PrivateCow;
        }
        if self.page_at(page_offset).is_some() {
            return VmFaultResolution::RetryAfterMap;
        }

        match self.fault_policy() {
            VmoFaultPolicy::Immediate => VmFaultResolution::Unresolved,
            VmoFaultPolicy::PrivateCow => VmFaultResolution::PrivateCow,
            VmoFaultPolicy::PagerBacked => VmFaultResolution::PagerBacked,
        }
    }

    /// Return whether the supplied mapping flags are compatible with this VMO.
    pub fn can_map(&self, flags: VmFlags) -> bool {
        let current = self.flags();
        if !current.contains(VmFlags::MAP) {
            return false;
        }
        if flags.contains(VmFlags::READ) && !current.contains(VmFlags::READ) {
            return false;
        }
        if flags.contains(VmFlags::WRITE) && !current.contains(VmFlags::WRITE) {
            return false;
        }
        if flags.contains(VmFlags::EXECUTE) && !current.contains(VmFlags::EXECUTE) {
            return false;
        }
        true
    }

    /// Track one frame extent backing a page-aligned VMO offset.
    pub fn track_frame(
        &self,
        offset: usize,
        frame: PhysRun,
        purpose: VmoPagePurpose,
        committed: bool,
    ) -> bool {
        // Child-local COW pages are created by `materialize_private_cow_page`
        // so external callers never mutate child extent ownership directly.
        if self.shared.child.is_some() {
            return false;
        }
        if !self.offset_is_page_aligned(offset) || frame.unit_page_size() != self.shared.page_size {
            return false;
        }
        let Some(end) = offset.checked_add(frame.len_bytes()) else {
            return false;
        };
        if end > self.size() {
            return false;
        }

        let mut inner = self.shared.inner.write();
        if inner
            .extents
            .iter()
            .any(|extent| offset < extent.end() && extent.offset < end)
        {
            return false;
        }

        inner.extents.push(VmoExtent {
            offset,
            frame,
            purpose,
            committed,
        });
        let snapshots = inner
            .extents
            .iter()
            .enumerate()
            .map(|(owner, extent)| (owner, extent.offset, extent.end()))
            .collect::<Vec<_>>();
        inner.index.rebuild(&snapshots);
        true
    }

    /// Remove one tracked frame extent from a paged VMO.
    pub fn untrack_frame(&self, offset: usize) -> Option<VmoPageMetadata> {
        // Child-local pages remain internal to the private COW path, so only
        // parent VMOs support external tracked-page removal.
        if self.shared.child.is_some() {
            return None;
        }
        if !self.offset_is_page_aligned(offset) {
            return None;
        }
        if !matches!(self.shared.backing, VmoBacking::Paged) {
            return None;
        }

        let mut inner = self.shared.inner.write();
        let position = inner
            .extents
            .iter()
            .position(|extent| extent.offset == offset)?;
        let removed = inner.extents.remove(position);
        let snapshots = inner
            .extents
            .iter()
            .enumerate()
            .map(|(owner, extent)| (owner, extent.offset, extent.end()))
            .collect::<Vec<_>>();
        inner.index.rebuild(&snapshots);
        Some(VmoPageMetadata {
            phys: removed.frame.phys_addr(),
            purpose: removed.purpose,
            committed: removed.committed,
        })
    }

    /// Return the tracked page metadata at one page-aligned offset.
    pub fn page_at(&self, offset: usize) -> Option<VmoPageMetadata> {
        if !self.offset_is_page_aligned(offset) {
            return None;
        }

        if let Some(child) = &self.shared.child {
            if child.kind == VmoChildKind::PrivateCow {
                let inner = self.shared.inner.read();
                if let Some(owner) = inner.index.owner_at(offset) {
                    return inner.extents.get(owner)?.metadata_at(offset);
                }
            }

            // Shared child views, and COW children before one split happens,
            // resolve through the parent window.
            let translated = child.parent_offset.checked_add(offset)?;
            return child.parent.page_at(translated);
        }

        let inner = self.shared.inner.read();
        let owner = inner.index.owner_at(offset)?;
        inner.extents.get(owner)?.metadata_at(offset)
    }

    /// Return whether one byte offset is covered by this VMO.
    pub fn contains_offset(&self, offset: usize) -> bool {
        offset < self.size()
    }

    /// Return the number of tracked pages.
    pub fn tracked_pages(&self) -> usize {
        if let Some(child) = &self.shared.child {
            let page_size = self.shared.page_size;
            let mut tracked = 0usize;
            let mut offset = 0usize;
            while offset < self.size() {
                let child_local = if child.kind == VmoChildKind::PrivateCow {
                    self.shared.inner.read().index.owner_at(offset).is_some()
                } else {
                    false
                };
                if child_local {
                    tracked = tracked.saturating_add(1);
                } else if child.parent.page_at(child.parent_offset + offset).is_some() {
                    tracked = tracked.saturating_add(1);
                }
                offset = offset.saturating_add(page_size);
            }
            return tracked;
        }

        self.shared
            .inner
            .read()
            .extents
            .iter()
            .map(|extent| extent.frame.number())
            .sum()
    }

    /// Read bytes from the tracked physical backing.
    pub fn read(&self, offset: usize, buf: &mut [u8]) -> bool {
        if let Some(child) = &self.shared.child {
            let Some(end) = offset.checked_add(buf.len()) else {
                return false;
            };
            if end > self.size() {
                return false;
            }

            if child.kind == VmoChildKind::PrivateCow {
                let page_size = self.page_size();
                let mut cursor = offset;
                let mut copied = 0usize;
                while cursor < end {
                    let page_offset = cursor / page_size * page_size;
                    let within_page = cursor - page_offset;
                    let chunk = (page_size - within_page).min(end - cursor);
                    let child_local = self
                        .shared
                        .inner
                        .read()
                        .index
                        .owner_at(page_offset)
                        .is_some();
                    if child_local {
                        if !self.read_local(cursor, &mut buf[copied..copied + chunk]) {
                            return false;
                        }
                    } else {
                        let Some(parent_offset) = child.parent_offset.checked_add(cursor) else {
                            return false;
                        };
                        if !child
                            .parent
                            .read(parent_offset, &mut buf[copied..copied + chunk])
                        {
                            return false;
                        }
                    }
                    cursor += chunk;
                    copied += chunk;
                }
                return true;
            }

            let Some(parent_offset) = child.parent_offset.checked_add(offset) else {
                return false;
            };
            return child.parent.read(parent_offset, buf);
        }

        self.read_local(offset, buf)
    }

    fn read_local(&self, offset: usize, buf: &mut [u8]) -> bool {
        let Some(end) = offset.checked_add(buf.len()) else {
            return false;
        };
        if end > self.size() {
            return false;
        }

        let inner = self.shared.inner.read();
        let mut cursor = offset;
        let mut copied = 0usize;
        while cursor < end {
            let owner = match inner.index.owner_at(cursor) {
                Some(owner) => owner,
                None => return false,
            };
            let Some(extent) = inner.extents.get(owner) else {
                return false;
            };
            if !extent.committed {
                return false;
            }

            let within = cursor - extent.offset;
            let available = extent.end() - cursor;
            let chunk = available.min(end - cursor);
            if !unsafe { extent.frame.read(within, &mut buf[copied..copied + chunk]) } {
                return false;
            }
            cursor += chunk;
            copied += chunk;
        }
        true
    }

    /// Write bytes into the tracked physical backing.
    pub fn write(&self, offset: usize, buf: &[u8]) -> bool {
        if let Some(child) = &self.shared.child {
            let Some(end) = offset.checked_add(buf.len()) else {
                return false;
            };
            if end > self.size() {
                return false;
            }

            if child.kind == VmoChildKind::PrivateCow {
                let page_size = self.page_size();
                let mut cursor = offset;
                let mut copied = 0usize;
                while cursor < end {
                    let page_offset = cursor / page_size * page_size;
                    let within_page = cursor - page_offset;
                    let chunk = (page_size - within_page).min(end - cursor);
                    if self
                        .shared
                        .inner
                        .read()
                        .index
                        .owner_at(page_offset)
                        .is_none()
                    {
                        return false;
                    }
                    if !self.write_local(cursor, &buf[copied..copied + chunk]) {
                        return false;
                    }
                    cursor += chunk;
                    copied += chunk;
                }
                return true;
            }

            let Some(parent_offset) = child.parent_offset.checked_add(offset) else {
                return false;
            };
            return child.parent.write(parent_offset, buf);
        }

        self.write_local(offset, buf)
    }

    fn write_local(&self, offset: usize, buf: &[u8]) -> bool {
        let Some(end) = offset.checked_add(buf.len()) else {
            return false;
        };
        if end > self.size() {
            return false;
        }

        let inner = self.shared.inner.read();
        let mut cursor = offset;
        let mut copied = 0usize;
        while cursor < end {
            let owner = match inner.index.owner_at(cursor) {
                Some(owner) => owner,
                None => return false,
            };
            let Some(extent) = inner.extents.get(owner) else {
                return false;
            };
            if !extent.committed {
                return false;
            }

            let within = cursor - extent.offset;
            let available = extent.end() - cursor;
            let chunk = available.min(end - cursor);
            if !unsafe { extent.frame.write(within, &buf[copied..copied + chunk]) } {
                return false;
            }
            cursor += chunk;
            copied += chunk;
        }
        true
    }

    /// Return the logical stream size.
    pub fn stream_size(&self) -> usize {
        self.shared.inner.read().stream_size
    }

    /// Update the logical byte size when the VMO is resizable.
    pub fn set_size(&self, new_size: usize) -> bool {
        if self.shared.child.is_some() {
            return false;
        }
        if !self.flags().contains(VmFlags::RESIZABLE)
            || !new_size.is_multiple_of(self.shared.page_size)
        {
            return false;
        }

        let mut inner = self.shared.inner.write();
        if new_size < inner.size
            && inner
                .extents
                .iter()
                .any(|extent| extent.offset < new_size && new_size < extent.end())
        {
            return false;
        }

        if new_size < inner.size {
            inner.extents.retain(|extent| extent.offset < new_size);
            let snapshots = inner
                .extents
                .iter()
                .enumerate()
                .map(|(owner, extent)| (owner, extent.offset, extent.end()))
                .collect::<Vec<_>>();
            inner.index.rebuild(&snapshots);
        }

        inner.size = new_size;
        inner.stream_size = inner.stream_size.min(new_size);
        true
    }

    /// Update the logical stream size.
    pub fn set_stream_size(&self, new_stream_size: usize) -> bool {
        let mut inner = self.shared.inner.write();
        if new_stream_size > inner.size {
            return false;
        }
        inner.stream_size = new_stream_size;
        true
    }

    /// Update the default cache policy stored in the VMO flags.
    pub fn set_cache_policy(&self, policy: libakarin_machine_core::memory::paging::CachePolicy) {
        let mut inner = self.shared.inner.write();
        inner.flags = inner.flags.with_cache_policy(policy);
    }

    /// Materialize committed backing pages across one page-aligned range.
    ///
    /// This first implementation is intentionally narrow:
    /// - only non-child paged VMOs may allocate new backing;
    /// - the requested range must stay page aligned and fully inside the VMO;
    /// - existing tracked extents are left in place and only gain the committed
    ///   bit when they were previously retained but marked pending.
    pub fn commit_range<A>(
        &self,
        offset: usize,
        len: usize,
        purpose: VmoPagePurpose,
        allocator: &'static dyn FrameAllocatorTrait,
    ) -> Result<usize, VmoRangeOpError>
    where
        A: AddressSpaceTrait,
    {
        let page_size = self.page_size();
        if self.shared.child.is_some()
            || self.backing() != VmoBacking::Paged
            || len == 0
            || !offset.is_multiple_of(page_size)
            || !len.is_multiple_of(page_size)
        {
            return Err(VmoRangeOpError::InvalidArgument);
        }

        let end = offset
            .checked_add(len)
            .ok_or(VmoRangeOpError::InvalidRange)?;
        if end > self.size() {
            return Err(VmoRangeOpError::InvalidRange);
        }

        let frame_count = page_size / allocator.unit_page_size();
        if frame_count == 0 || frame_count * allocator.unit_page_size() != page_size {
            return Err(VmoRangeOpError::InvalidArgument);
        }

        let mut inner = self.shared.inner.write();
        let mut page_offset = offset;
        let mut index_dirty = false;
        while page_offset < end {
            if let Some(owner) = inner.index.owner_at(page_offset) {
                let Some(extent) = inner.extents.get_mut(owner) else {
                    return Err(VmoRangeOpError::InvalidArgument);
                };
                extent.committed = true;
                page_offset += page_size;
                continue;
            }

            let virt = allocator
                .alloc(None, FrameZone::default(), frame_count)
                .map_err(|_| VmoRangeOpError::InvalidArgument)?;
            let phys = A::virt_to_phys(virt).ok_or(VmoRangeOpError::InvalidArgument)?;
            let frame = unsafe {
                libakarin_machine_core::memory::paging::PhysFrame::<A>::from_addr(
                    Some(allocator),
                    phys,
                    frame_count,
                )
            };
            inner.extents.push(VmoExtent {
                offset: page_offset,
                frame: PhysRun::from_frame(frame),
                purpose,
                committed: true,
            });
            index_dirty = true;
            page_offset += page_size;
        }

        if index_dirty {
            let snapshots = inner
                .extents
                .iter()
                .enumerate()
                .map(|(owner, extent)| (owner, extent.offset, extent.end()))
                .collect::<Vec<_>>();
            inner.index.rebuild(&snapshots);
        }
        Ok(len)
    }

    /// Remove committed backing pages across one page-aligned range.
    ///
    /// The current implementation only supports ranges that cover whole tracked
    /// extents. This matches the page-sized extents produced by the existing
    /// pager-less anonymous VMO paths and avoids splitting one owned `PhysRun`
    /// into smaller RAII fragments without allocator provenance.
    pub fn decommit_range(&self, offset: usize, len: usize) -> Result<usize, VmoRangeOpError> {
        let page_size = self.page_size();
        if self.shared.child.is_some()
            || self.backing() != VmoBacking::Paged
            || len == 0
            || !offset.is_multiple_of(page_size)
            || !len.is_multiple_of(page_size)
        {
            return Err(VmoRangeOpError::InvalidArgument);
        }

        let end = offset
            .checked_add(len)
            .ok_or(VmoRangeOpError::InvalidRange)?;
        if end > self.size() {
            return Err(VmoRangeOpError::InvalidRange);
        }

        let inner = self.shared.inner.read();
        for extent in &inner.extents {
            if extent.end() <= offset || end <= extent.offset {
                continue;
            }
            if extent.offset < offset || end < extent.end() {
                return Err(VmoRangeOpError::InvalidArgument);
            }
        }
        drop(inner);

        let mut inner = self.shared.inner.write();
        let original_len = inner.extents.len();
        inner.extents.retain(|extent| {
            extent.end() <= offset
                || end <= extent.offset
                || extent.offset < offset
                || end < extent.end()
        });
        if inner.extents.len() != original_len {
            let snapshots = inner
                .extents
                .iter()
                .enumerate()
                .map(|(owner, extent)| (owner, extent.offset, extent.end()))
                .collect::<Vec<_>>();
            inner.index.rebuild(&snapshots);
        }
        Ok(len)
    }

    /// Zero every committed page already visible across one page-aligned range.
    ///
    /// Missing backing pages are left absent so this operation can be used
    /// without implicitly materializing anonymous memory.
    pub fn zero_range<A>(&self, offset: usize, len: usize) -> Result<usize, VmoRangeOpError>
    where
        A: AddressSpaceTrait,
    {
        let page_size = self.page_size();
        if self.shared.child.is_some()
            || !self.flags().contains(VmFlags::WRITE)
            || len == 0
            || !offset.is_multiple_of(page_size)
            || !len.is_multiple_of(page_size)
        {
            return Err(VmoRangeOpError::InvalidArgument);
        }

        let end = offset
            .checked_add(len)
            .ok_or(VmoRangeOpError::InvalidRange)?;
        if end > self.size() {
            return Err(VmoRangeOpError::InvalidRange);
        }

        let mut page_offset = offset;
        while page_offset < end {
            if let Some(meta) = self.page_at(page_offset)
                && meta.committed
            {
                unsafe { A::zero_phys(meta.phys, page_size) };
            }
            page_offset += page_size;
        }
        Ok(len)
    }

    /// Count the committed bytes currently visible across one page-aligned
    /// range.
    pub fn query_committed_bytes(
        &self,
        offset: usize,
        len: usize,
    ) -> Result<usize, VmoRangeOpError> {
        let page_size = self.page_size();
        if len == 0 || !offset.is_multiple_of(page_size) || !len.is_multiple_of(page_size) {
            return Err(VmoRangeOpError::InvalidArgument);
        }

        let end = offset
            .checked_add(len)
            .ok_or(VmoRangeOpError::InvalidRange)?;
        if end > self.size() {
            return Err(VmoRangeOpError::InvalidRange);
        }

        let mut committed = 0usize;
        let mut page_offset = offset;
        while page_offset < end {
            if self.page_at(page_offset).is_some_and(|meta| meta.committed) {
                committed += page_size;
            }
            page_offset += page_size;
        }
        Ok(committed)
    }

    /// Create one fixed-size child view into this VMO.
    ///
    /// The first child implementation is intentionally narrow:
    /// - the parent must not be resizable;
    /// - `offset` and `size` must be page aligned;
    /// - the child forwards reads and writes into the parent range instead of
    ///   creating private COW state.
    pub fn create_child(
        &self,
        name: impl Into<String>,
        offset: usize,
        size: usize,
    ) -> Option<Self> {
        if self.flags().contains(VmFlags::RESIZABLE)
            || size == 0
            || !offset.is_multiple_of(self.shared.page_size)
            || !size.is_multiple_of(self.shared.page_size)
        {
            return None;
        }

        let end = offset.checked_add(size)?;
        if end > self.size() {
            return None;
        }

        let mut flags = self.flags();
        flags.remove(VmFlags::RESIZABLE);
        let stream_size = self.stream_size().saturating_sub(offset).min(size);
        Some(Self {
            shared: Arc::new(VmoShared {
                id: NEXT_VMO_ID.fetch_add(1, Ordering::Relaxed),
                name: name.into(),
                page_size: self.shared.page_size,
                backing: self.shared.backing,
                child: Some(VmoChildState {
                    parent: self.clone(),
                    parent_offset: offset,
                    kind: VmoChildKind::SharedView,
                }),
                inner: SpinRwLock::new(VmoInner {
                    size,
                    stream_size,
                    flags,
                    ..VmoInner::default()
                }),
            }),
        })
    }

    /// Create one fixed-size child that starts shared and splits on writes.
    ///
    /// The first `PrivateCow` implementation only supports paged, non-resizable
    /// parents. Child-local private pages are allocated lazily on the first
    /// servicing fault and remain anchored in the child afterwards.
    pub fn create_private_child(
        &self,
        name: impl Into<String>,
        offset: usize,
        size: usize,
    ) -> Option<Self> {
        if self.backing() != VmoBacking::Paged
            || self.flags().contains(VmFlags::RESIZABLE)
            || size == 0
            || !offset.is_multiple_of(self.shared.page_size)
            || !size.is_multiple_of(self.shared.page_size)
        {
            return None;
        }

        let end = offset.checked_add(size)?;
        if end > self.size() {
            return None;
        }

        let mut flags = self.flags();
        flags.remove(VmFlags::RESIZABLE);
        let stream_size = self.stream_size().saturating_sub(offset).min(size);
        Some(Self {
            shared: Arc::new(VmoShared {
                id: NEXT_VMO_ID.fetch_add(1, Ordering::Relaxed),
                name: name.into(),
                page_size: self.shared.page_size,
                backing: VmoBacking::Paged,
                child: Some(VmoChildState {
                    parent: self.clone(),
                    parent_offset: offset,
                    kind: VmoChildKind::PrivateCow,
                }),
                inner: SpinRwLock::new(VmoInner {
                    size,
                    stream_size,
                    flags,
                    ..VmoInner::default()
                }),
            }),
        })
    }

    /// Copy one byte range from `src` into this VMO.
    pub fn transfer_from(
        &self,
        dst_offset: usize,
        src: &Self,
        src_offset: usize,
        len: usize,
    ) -> bool {
        let mut buffer = vec![0u8; len];
        if !src.read(src_offset, &mut buffer) {
            return false;
        }
        self.write(dst_offset, &buffer)
    }

    fn offset_is_page_aligned(&self, offset: usize) -> bool {
        offset < self.size() && offset.is_multiple_of(self.shared.page_size)
    }
}

impl ControlPlane for Vmo {
    type ReadGuard<'a>
        = VmoReadGuard<'a>
    where
        Self: 'a;
    type WriteGuard<'a>
        = VmoWriteGuard<'a>
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = VmoDeniedGuard<'a>
    where
        Self: 'a;
    type AgentGuard<'a>
        = VmoDeniedGuard<'a>
    where
        Self: 'a;
    type AdminGuard<'a>
        = VmoAdminGuard<'a>
    where
        Self: 'a;

    fn read(&self, interface_caps: u32) -> Self::ReadGuard<'_> {
        VmoReadGuard::new(self, interface_caps)
    }

    fn write(&self, interface_caps: u32) -> Self::WriteGuard<'_> {
        VmoWriteGuard::new(self, interface_caps)
    }

    fn execute(&self, _interface_caps: u32) -> Self::ExecuteGuard<'_> {
        VmoDeniedGuard::new(self)
    }

    fn agent(&self, _interface_caps: u32) -> Self::AgentGuard<'_> {
        VmoDeniedGuard::new(self)
    }

    fn admin(&self, interface_caps: u32) -> Self::AdminGuard<'_> {
        VmoAdminGuard::new(self, interface_caps)
    }
}

#[async_trait]
impl SyscallDispatch<libakarin_object::ObjectSyscallContext> for VmoReadGuard<'_> {
    async fn dispatch(
        &self,
        caller: &libakarin_object::ObjectSyscallContext,
        method_id: usize,
        arg1: usize,
        _arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let Ok(method) = VmoMethod::try_from(method_id) else {
            return Ok(VmInvokeFrame::vm_error(VmInvokeError::InvalidArgument));
        };
        match method {
            VmoMethod::GetSize => Ok(VmInvokeFrame::ok([self.size()?, 0, 0, 0, 0])),
            VmoMethod::GetStreamSize => Ok(VmInvokeFrame::ok([self.stream_size()?, 0, 0, 0, 0])),
            VmoMethod::QueryChild => match self.child_info()? {
                Some(info) => Ok(VmInvokeFrame::ok([
                    info.kind as usize,
                    info.parent_id as usize,
                    info.parent_offset,
                    info.size,
                    info.stream_size,
                ])),
                None => Ok(VmInvokeFrame::ok([
                    0,
                    0,
                    0,
                    self.size()?,
                    self.stream_size()?,
                ])),
            },
            VmoMethod::Read => {
                let args = match read_syscall_pod::<VmoIoArgs>(caller, arg1) {
                    Ok(args) => args,
                    Err(error) => return Ok(VmInvokeFrame::vm_error(error)),
                };
                let mut buffer = vec![0u8; args.len];
                if let Err(error) = self.read(args.offset, &mut buffer) {
                    return match error {
                        ObjectError::InsufficientCapabilities => Err(error),
                        _ => Ok(VmInvokeFrame::vm_error(VmInvokeError::InvalidArgument)),
                    };
                }
                if caller.copy_to_user(args.buffer, &buffer).is_err() {
                    return Ok(VmInvokeFrame::vm_error(VmInvokeError::Fault));
                }
                Ok(VmInvokeFrame::ok([buffer.len(), 0, 0, 0, 0]))
            }
            _ => Err(ObjectError::InsufficientCapabilities),
        }
    }
}

#[async_trait]
impl SyscallDispatch<libakarin_object::ObjectSyscallContext> for VmoWriteGuard<'_> {
    async fn dispatch(
        &self,
        caller: &libakarin_object::ObjectSyscallContext,
        method_id: usize,
        arg1: usize,
        _arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let Ok(method) = VmoMethod::try_from(method_id) else {
            return Ok(VmInvokeFrame::vm_error(VmInvokeError::InvalidArgument));
        };
        match method {
            VmoMethod::Write => {
                let args = match read_syscall_pod::<VmoIoArgs>(caller, arg1) {
                    Ok(args) => args,
                    Err(error) => return Ok(VmInvokeFrame::vm_error(error)),
                };
                let mut buffer = vec![0u8; args.len];
                if caller.copy_from_user(args.buffer, &mut buffer).is_err() {
                    return Ok(VmInvokeFrame::vm_error(VmInvokeError::Fault));
                }
                let written = match self.write(args.offset, &buffer) {
                    Ok(written) => written,
                    Err(error) => {
                        return match error {
                            ObjectError::InsufficientCapabilities => Err(error),
                            _ => Ok(VmInvokeFrame::vm_error(VmInvokeError::InvalidArgument)),
                        };
                    }
                };
                Ok(VmInvokeFrame::ok([written, 0, 0, 0, 0]))
            }
            VmoMethod::SetSize => match self.set_size(arg1) {
                Ok((size, stream_size)) => Ok(VmInvokeFrame::ok([size, stream_size, 0, 0, 0])),
                Err(error) => match error {
                    ObjectError::InsufficientCapabilities => Err(error),
                    _ => Ok(VmInvokeFrame::vm_error(VmInvokeError::InvalidArgument)),
                },
            },
            VmoMethod::SetStreamSize => match self.set_stream_size(arg1) {
                Ok(stream_size) => Ok(VmInvokeFrame::ok([stream_size, 0, 0, 0, 0])),
                Err(error) => match error {
                    ObjectError::InsufficientCapabilities => Err(error),
                    _ => Ok(VmInvokeFrame::vm_error(VmInvokeError::InvalidArgument)),
                },
            },
            VmoMethod::SetCachePolicy => {
                let policy = match arg1 {
                    0 => libakarin_machine_core::memory::paging::CachePolicy::Cached,
                    1 => libakarin_machine_core::memory::paging::CachePolicy::Uncached,
                    2 => libakarin_machine_core::memory::paging::CachePolicy::UncachedDevice,
                    3 => libakarin_machine_core::memory::paging::CachePolicy::WriteCombining,
                    _ => return Ok(VmInvokeFrame::vm_error(VmInvokeError::InvalidArgument)),
                };
                match self.set_cache_policy(policy) {
                    Ok(flags) => Ok(VmInvokeFrame::ok([flags.bits() as usize, 0, 0, 0, 0])),
                    Err(error) => match error {
                        ObjectError::InsufficientCapabilities => Err(error),
                        _ => Ok(VmInvokeFrame::vm_error(VmInvokeError::InvalidArgument)),
                    },
                }
            }
            VmoMethod::CreateChild => {
                allow_interface(self.interface_caps, V_CREATE_CHILD)?;
                Ok(VmInvokeFrame::vm_error(VmInvokeError::InvalidArgument))
            }
            _ => Err(ObjectError::InsufficientCapabilities),
        }
    }
}

#[async_trait]
impl SyscallDispatch<libakarin_object::ObjectSyscallContext> for VmoAdminGuard<'_> {
    async fn dispatch(
        &self,
        caller: &libakarin_object::ObjectSyscallContext,
        method_id: usize,
        arg1: usize,
        _arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let Ok(method) = VmoMethod::try_from(method_id) else {
            return Ok(VmInvokeFrame::vm_error(VmInvokeError::InvalidArgument));
        };
        match method {
            VmoMethod::TransferData => {
                let args = match read_syscall_pod::<VmoTransferArgs>(caller, arg1) {
                    Ok(args) => args,
                    Err(error) => return Ok(VmInvokeFrame::vm_error(error)),
                };
                let src_handle = caller.acquire_handle(args.src_slot)?;
                let src = src_handle.read_cp_with::<Vmo, _, _>(|src| src.share())??;
                match self.transfer_from(args.dst_offset, src.as_ref(), args.src_offset, args.len) {
                    Ok(copied) => Ok(VmInvokeFrame::ok([copied, 0, 0, 0, 0])),
                    Err(error) => match error {
                        ObjectError::InsufficientCapabilities => Err(error),
                        _ => Ok(VmInvokeFrame::vm_error(VmInvokeError::InvalidArgument)),
                    },
                }
            }
            _ => Err(ObjectError::InsufficientCapabilities),
        }
    }
}

#[async_trait]
impl SyscallDispatch<libakarin_object::ObjectSyscallContext> for VmoDeniedGuard<'_> {
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
    use core::sync::atomic::{AtomicUsize, Ordering};

    use libakarin_machine_core::memory::{
        AllocationError, FrameAllocatorTrait, FrameZone, PhysAddr, VirtAddr,
        paging::{CachePolicy, MMUFlags, PagingError, PhysFrameTrait, TlbInvalidator},
    };

    use super::*;

    struct DummyAllocator;

    impl FrameAllocatorTrait for DummyAllocator {
        fn unit_page_size(&self) -> usize {
            0x1000
        }

        fn alloc(
            &self,
            _addr: Option<PhysAddr>,
            _prefer_zone: FrameZone,
            _num: usize,
        ) -> Result<VirtAddr, AllocationError> {
            Err(AllocationError::OutOfMemory)
        }

        unsafe fn dealloc(&self, _addr: VirtAddr, _num: usize) {}

        fn used_frames(&self) -> usize {
            0
        }

        fn total_frames(&self) -> usize {
            0
        }
    }

    struct CowTestAllocator {
        pages: [usize; 2],
        count: usize,
        next: AtomicUsize,
    }

    impl CowTestAllocator {
        fn new(pages: [usize; 2], count: usize) -> Self {
            Self {
                pages,
                count,
                next: AtomicUsize::new(0),
            }
        }
    }

    impl FrameAllocatorTrait for CowTestAllocator {
        fn unit_page_size(&self) -> usize {
            0x1000
        }

        fn alloc(
            &self,
            _addr: Option<PhysAddr>,
            _prefer_zone: FrameZone,
            num: usize,
        ) -> Result<VirtAddr, AllocationError> {
            if num != 1 {
                return Err(AllocationError::OutOfMemory);
            }

            let index = self.next.fetch_add(1, Ordering::Relaxed);
            if index >= self.count {
                return Err(AllocationError::OutOfMemory);
            }

            let page = self.pages[index];
            unsafe { core::ptr::write_bytes(page as *mut u8, 0, 0x1000) };
            Ok(VirtAddr::new(page))
        }

        unsafe fn dealloc(&self, _addr: VirtAddr, _num: usize) {}

        fn used_frames(&self) -> usize {
            self.next.load(Ordering::Relaxed)
        }

        fn total_frames(&self) -> usize {
            self.count
        }
    }

    struct DummyAddressSpace;
    #[derive(Clone, Copy)]
    struct DummyPage;
    #[derive(Clone, Copy)]
    struct DummyEntry;
    struct DummyPageTable;

    impl AddressSpaceTrait for DummyAddressSpace {
        type Page = DummyPage;
        type PageSize = DummyPageSize;
        type PageTable = DummyPageTable;
        type PageTableEntry = DummyEntry;

        fn virt_to_phys(virt_addr: VirtAddr) -> Option<PhysAddr> {
            Some(PhysAddr::new(virt_addr.as_usize()))
        }

        fn phys_to_virt(phys_addr: PhysAddr) -> Option<VirtAddr> {
            Some(VirtAddr::new(phys_addr.as_usize()))
        }

        unsafe fn read_phys(phys_addr: PhysAddr, buffer: &mut [u8]) {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    phys_addr.as_usize() as *const u8,
                    buffer.as_mut_ptr(),
                    buffer.len(),
                )
            };
        }
        unsafe fn write_phys(phys_addr: PhysAddr, data: &[u8]) {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    data.as_ptr(),
                    phys_addr.as_usize() as *mut u8,
                    data.len(),
                )
            };
        }
        unsafe fn zero_phys(phys_addr: PhysAddr, size: usize) {
            unsafe { core::ptr::write_bytes(phys_addr.as_usize() as *mut u8, 0, size) };
        }
        unsafe fn copy_phys(src_phys_addr: PhysAddr, dest_phys_addr: PhysAddr, size: usize) {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src_phys_addr.as_usize() as *const u8,
                    dest_phys_addr.as_usize() as *mut u8,
                    size,
                )
            };
        }
        fn current_base() -> PhysAddr {
            PhysAddr::new(0)
        }
        unsafe fn switch_base(_new_base: PhysAddr) {}
        fn invalidate_tlb(_virt_addr: VirtAddr) {}
        fn flush_tlb() {}
        fn map_kernel_space(_page_table: &mut Self::PageTable) {}

        type PhysFrame = libakarin_machine_core::memory::PhysFrame<Self>;

        fn copy_from_user(
            src: VirtAddr,
            buffer: &mut [u8],
        ) -> Result<(), libakarin_machine_core::memory::UaccessError> {
            todo!()
        }

        fn copy_to_user(
            dst: VirtAddr,
            data: &[u8],
        ) -> Result<(), libakarin_machine_core::memory::UaccessError> {
            todo!()
        }
    }

    struct DummyPageSize;

    impl libakarin_machine_core::memory::PageSizeTrait for DummyPageSize {
        const UNIT_PAGE_SIZE: usize = 0x1000;

        fn validate(size: usize) -> bool {
            size == Self::UNIT_PAGE_SIZE
        }

        fn size(&self) -> usize {
            Self::UNIT_PAGE_SIZE
        }
    }

    impl libakarin_machine_core::memory::PageTrait<DummyPageSize> for DummyPage {
        fn containing(virt_addr: VirtAddr, size: DummyPageSize) -> Self {
            assert!(size.validate(size.size()));
            assert!(virt_addr.as_usize() % size.size() == 0);
            Self
        }
        fn size(&self) -> DummyPageSize {
            DummyPageSize
        }

        fn virt_addr(&self) -> VirtAddr {
            VirtAddr::new(0)
        }
    }

    impl libakarin_machine_core::memory::PageTableEntryTrait for DummyEntry {
        fn phys_addr(&self) -> PhysAddr {
            PhysAddr::new(0)
        }
        fn flags(&self) -> MMUFlags {
            MMUFlags::empty()
        }
        fn cache_policy(&self) -> CachePolicy {
            CachePolicy::Cached
        }
        fn is_unused(&self) -> bool {
            true
        }
        fn is_present(&self) -> bool {
            false
        }
        fn is_leaf(&self) -> bool {
            false
        }
        fn set_flags(&mut self, _flags: MMUFlags) {}
        fn set_cache_policy(&mut self, _policy: CachePolicy) {}
        fn set_phys_addr(&mut self, _phys_addr: PhysAddr, _flags: MMUFlags) {}
        fn clear(&mut self) {}
    }

    impl
        libakarin_machine_core::memory::PageTableTrait<
            DummyAddressSpace,
            DummyPageSize,
            DummyPage,
            DummyEntry,
        > for DummyPageTable
    {
        fn empty(_allocator: &'static dyn FrameAllocatorTrait) -> Self {
            Self
        }
        fn from_raw(_allocator: &'static dyn FrameAllocatorTrait, _root_ptr: *mut u8) -> Self {
            Self
        }
        fn phys_addr(&self) -> PhysAddr {
            PhysAddr::new(0)
        }
        unsafe fn map(
            &mut self,
            _page: DummyPage,
            _frame: <DummyAddressSpace as AddressSpaceTrait>::PhysFrame,
            _flags: MMUFlags,
            _cache: CachePolicy,
        ) -> Result<TlbInvalidator<DummyAddressSpace>, PagingError> {
            unreachable!()
        }
        unsafe fn unmap(
            &mut self,
            _page: DummyPage,
        ) -> libakarin_machine_core::memory::paging::PagingResult<(
            <DummyAddressSpace as AddressSpaceTrait>::PhysFrame,
            TlbInvalidator<DummyAddressSpace>,
        )> {
            unreachable!()
        }
        fn entry(&self, _page: DummyPage) -> Result<(&DummyEntry, DummyPageSize), PagingError> {
            unreachable!()
        }
        fn entry_mut(
            &mut self,
            _page: DummyPage,
        ) -> Result<(&mut DummyEntry, DummyPageSize), PagingError> {
            unreachable!()
        }
    }

    #[test]
    fn vmo_requires_map_flag_for_mapping() {
        let vmo = Vmo::new("test", 0x2000, 0x1000, VmFlags::READ | VmFlags::WRITE);
        assert!(!vmo.can_map(VmFlags::READ));

        let mapped = Vmo::new(
            "mapped",
            0x2000,
            0x1000,
            VmFlags::READ | VmFlags::WRITE | VmFlags::MAP,
        );
        assert!(mapped.can_map(VmFlags::READ));
        assert!(!mapped.can_map(VmFlags::EXECUTE));
    }

    #[test]
    fn tracked_frame_is_resolved_by_offset() {
        static ALLOC: DummyAllocator = DummyAllocator;
        let frame = unsafe {
            libakarin_machine_core::memory::paging::PhysFrame::<DummyAddressSpace>::from_addr(
                Some(&ALLOC),
                PhysAddr::new(0x2000),
                1,
            )
        };
        let vmo = Vmo::new(
            "test",
            0x3000,
            0x1000,
            VmFlags::READ | VmFlags::WRITE | VmFlags::MAP,
        );
        assert!(vmo.track_frame(
            0x1000,
            PhysRun::from_frame(frame),
            VmoPagePurpose::Anonymous,
            true,
        ));
        let page = vmo
            .page_at(0x1000)
            .expect("tracked page should be discoverable");
        assert_eq!(page.phys, PhysAddr::new(0x2000));
    }

    #[test]
    fn tracked_extent_supports_byte_reads_and_writes() {
        let page = Box::leak(Box::new([0u8; 0x1000]));
        let phys = PhysAddr::new(page.as_mut_ptr() as usize);
        let vmo = Vmo::new(
            "io",
            0x1000,
            0x1000,
            VmFlags::READ | VmFlags::WRITE | VmFlags::MAP,
        );
        assert!(vmo.track_frame(
            0,
            PhysRun::borrowed::<DummyAddressSpace>(phys, 1),
            VmoPagePurpose::Anonymous,
            true,
        ));

        assert!(vmo.write(8, b"akarin"));
        let mut out = [0u8; 6];
        assert!(vmo.read(8, &mut out));
        assert_eq!(&out, b"akarin");
        assert_eq!(&page[8..14], b"akarin");
    }

    #[test]
    fn resizable_vmo_updates_size_and_stream_size() {
        let vmo = Vmo::new(
            "resizable",
            0x3000,
            0x1000,
            VmFlags::READ | VmFlags::WRITE | VmFlags::MAP | VmFlags::RESIZABLE,
        );
        assert!(vmo.set_stream_size(0x2000));
        assert!(vmo.set_size(0x2000));
        assert_eq!(vmo.size(), 0x2000);
        assert_eq!(vmo.stream_size(), 0x2000);
        assert!(!vmo.set_stream_size(0x3000));
    }

    #[test]
    fn transfer_from_copies_between_vmos() {
        let src_page = Box::leak(Box::new([0u8; 0x1000]));
        let dst_page = Box::leak(Box::new([0u8; 0x1000]));
        src_page[32..38].copy_from_slice(b"kernel");

        let src = Vmo::new(
            "src",
            0x1000,
            0x1000,
            VmFlags::READ | VmFlags::WRITE | VmFlags::MAP,
        );
        let dst = Vmo::new(
            "dst",
            0x1000,
            0x1000,
            VmFlags::READ | VmFlags::WRITE | VmFlags::MAP,
        );

        assert!(src.track_frame(
            0,
            PhysRun::borrowed::<DummyAddressSpace>(
                PhysAddr::new(src_page.as_mut_ptr() as usize),
                1
            ),
            VmoPagePurpose::Anonymous,
            true,
        ));
        assert!(dst.track_frame(
            0,
            PhysRun::borrowed::<DummyAddressSpace>(
                PhysAddr::new(dst_page.as_mut_ptr() as usize),
                1
            ),
            VmoPagePurpose::Anonymous,
            true,
        ));

        assert!(dst.transfer_from(64, &src, 32, 6));
        assert_eq!(&dst_page[64..70], b"kernel");
    }

    #[test]
    fn child_vmo_shares_parent_range() {
        let pages = Box::leak(Box::new([0u8; 0x2000]));
        pages[0x1020..0x1026].copy_from_slice(b"parent");
        let parent = Vmo::new(
            "parent",
            0x2000,
            0x1000,
            VmFlags::READ | VmFlags::WRITE | VmFlags::MAP,
        );
        assert!(parent.track_frame(
            0,
            PhysRun::borrowed::<DummyAddressSpace>(PhysAddr::new(pages.as_mut_ptr() as usize), 2),
            VmoPagePurpose::Anonymous,
            true,
        ));

        let child = parent
            .create_child("child", 0x1000, 0x1000)
            .expect("child VMO should be created");
        let mut out = [0u8; 6];
        assert!(child.read(0x20, &mut out));
        assert_eq!(&out, b"parent");
        assert!(child.write(0x40, b"child!"));
        assert_eq!(&pages[0x1040..0x1046], b"child!");
        assert_eq!(child.tracked_pages(), 1);
        assert_eq!(
            child.page_at(0).expect("child page should resolve").phys,
            PhysAddr::new(pages.as_mut_ptr() as usize + 0x1000)
        );
    }

    #[test]
    fn child_vmo_rejects_invalid_parent_ranges() {
        let resizable = Vmo::new(
            "resizable",
            0x2000,
            0x1000,
            VmFlags::READ | VmFlags::WRITE | VmFlags::MAP | VmFlags::RESIZABLE,
        );
        assert!(resizable.create_child("bad", 0x1000, 0x1000).is_none());

        let parent = Vmo::new(
            "parent",
            0x2000,
            0x1000,
            VmFlags::READ | VmFlags::WRITE | VmFlags::MAP,
        );
        assert!(parent.create_child("bad", 0x100, 0x1000).is_none());
        assert!(parent.create_child("bad", 0x1000, 0x800).is_none());
        assert!(parent.create_child("bad", 0x1000, 0x2000).is_none());
        assert!(parent.create_child("bad", 0x1000, 0).is_none());
    }

    #[test]
    fn child_vmo_reports_parent_window_metadata() {
        let parent = Vmo::new(
            "parent",
            0x4000,
            0x1000,
            VmFlags::READ | VmFlags::WRITE | VmFlags::MAP,
        );
        let child = parent
            .create_child("child", 0x1000, 0x2000)
            .expect("child VMO should be created");
        let info = child.child_info().expect("child metadata should exist");
        assert_eq!(info.kind, VmoChildKind::SharedView);
        assert_eq!(info.parent_id, parent.id());
        assert_eq!(info.parent_offset, 0x1000);
        assert_eq!(info.size, 0x2000);
        assert_eq!(info.stream_size, 0x2000);
    }

    #[test]
    fn child_vmo_supports_cross_page_io() {
        let pages = Box::leak(Box::new([0u8; 0x3000]));
        let parent = Vmo::new(
            "parent",
            0x3000,
            0x1000,
            VmFlags::READ | VmFlags::WRITE | VmFlags::MAP,
        );
        assert!(parent.track_frame(
            0,
            PhysRun::borrowed::<DummyAddressSpace>(PhysAddr::new(pages.as_mut_ptr() as usize), 3),
            VmoPagePurpose::Anonymous,
            true,
        ));

        let child = parent
            .create_child("child", 0x1000, 0x2000)
            .expect("child VMO should be created");
        let payload = *b"cross-page-window";
        assert!(child.write(0xff8, &payload));
        let mut out = [0u8; 17];
        assert!(parent.read(0x1ff8, &mut out));
        assert_eq!(out, payload);
    }

    #[test]
    fn child_vmo_rejects_out_of_range_accesses() {
        let pages = Box::leak(Box::new([0u8; 0x2000]));
        let parent = Vmo::new(
            "parent",
            0x2000,
            0x1000,
            VmFlags::READ | VmFlags::WRITE | VmFlags::MAP,
        );
        assert!(parent.track_frame(
            0,
            PhysRun::borrowed::<DummyAddressSpace>(PhysAddr::new(pages.as_mut_ptr() as usize), 2),
            VmoPagePurpose::Anonymous,
            true,
        ));

        let child = parent
            .create_child("child", 0x1000, 0x1000)
            .expect("child VMO should be created");
        let mut out = [0u8; 16];
        assert!(!child.read(0xff8, &mut out));
        assert!(!child.write(0xff8, b"overflow-window"));
    }

    #[test]
    fn child_vmo_reflects_parent_mutations_until_cow_exists() {
        let pages = Box::leak(Box::new([0u8; 0x2000]));
        let parent = Vmo::new(
            "parent",
            0x2000,
            0x1000,
            VmFlags::READ | VmFlags::WRITE | VmFlags::MAP,
        );
        assert!(parent.track_frame(
            0,
            PhysRun::borrowed::<DummyAddressSpace>(PhysAddr::new(pages.as_mut_ptr() as usize), 2),
            VmoPagePurpose::Anonymous,
            true,
        ));

        let child = parent
            .create_child("child", 0x1000, 0x1000)
            .expect("child VMO should be created");
        assert!(parent.write(0x1010, b"shared"));
        let mut out = [0u8; 6];
        assert!(child.read(0x10, &mut out));
        assert_eq!(&out, b"shared");
    }

    #[test]
    fn private_cow_child_requires_copy_before_writes_become_retryable() {
        let page = Box::leak(Box::new([0u8; 0x1000]));
        page[0x20..0x26].copy_from_slice(b"parent");
        let parent = Vmo::new(
            "parent",
            0x1000,
            0x1000,
            VmFlags::READ | VmFlags::WRITE | VmFlags::MAP,
        );
        assert!(parent.track_frame(
            0,
            PhysRun::borrowed::<DummyAddressSpace>(PhysAddr::new(page.as_mut_ptr() as usize), 1),
            VmoPagePurpose::Anonymous,
            true,
        ));

        let child = parent
            .create_private_child("child", 0, 0x1000)
            .expect("private child VMO should be created");
        assert_eq!(
            child
                .child_info()
                .expect("child metadata should exist")
                .kind,
            VmoChildKind::PrivateCow
        );
        assert_eq!(
            child.resolve_fault(0x20, MMUFlags::READ),
            VmFaultResolution::RetryAfterMap
        );
        assert_eq!(
            child.resolve_fault(0x20, MMUFlags::WRITE),
            VmFaultResolution::PrivateCow
        );
        assert!(!child.write(0x20, b"child!"));
        assert!(
            !child
                .mapping_mmu_flags_for_page(0, MMUFlags::READ | MMUFlags::WRITE)
                .contains(MMUFlags::WRITE)
        );
    }

    #[test]
    fn private_cow_fault_materializes_one_private_page_and_preserves_parent_data() {
        let parent_page = Box::leak(Box::new([0u8; 0x1000]));
        let child_page = Box::leak(Box::new([0u8; 0x1000]));
        parent_page[0x20..0x26].copy_from_slice(b"parent");

        let parent = Vmo::new(
            "parent",
            0x1000,
            0x1000,
            VmFlags::READ | VmFlags::WRITE | VmFlags::MAP,
        );
        assert!(parent.track_frame(
            0,
            PhysRun::borrowed::<DummyAddressSpace>(
                PhysAddr::new(parent_page.as_mut_ptr() as usize),
                1
            ),
            VmoPagePurpose::Anonymous,
            true,
        ));

        let child = parent
            .create_private_child("child", 0, 0x1000)
            .expect("private child VMO should be created");
        let allocator = Box::leak(Box::new(CowTestAllocator::new(
            [child_page.as_mut_ptr() as usize, 0],
            1,
        )));
        let private_meta = child
            .materialize_private_cow_page::<DummyAddressSpace>(
                0x20,
                VmoPagePurpose::Anonymous,
                allocator,
            )
            .expect("private page should materialize");
        assert_ne!(
            private_meta.phys,
            parent.page_at(0).expect("parent page should exist").phys
        );
        assert!(child.write(0x20, b"child!"));

        let mut child_out = [0u8; 6];
        let mut parent_out = [0u8; 6];
        assert!(child.read(0x20, &mut child_out));
        assert!(parent.read(0x20, &mut parent_out));
        assert_eq!(&child_out, b"child!");
        assert_eq!(&parent_out, b"parent");
        assert_eq!(
            child.resolve_fault(0x20, MMUFlags::WRITE),
            VmFaultResolution::RetryAfterMap
        );
        assert!(
            child
                .mapping_mmu_flags_for_page(0, MMUFlags::READ | MMUFlags::WRITE)
                .contains(MMUFlags::WRITE)
        );
    }

    #[test]
    fn vmo_resolve_fault_distinguishes_backed_and_unresolved_ranges() {
        let pages = Box::leak(Box::new([0u8; 0x1000]));
        let vmo = Vmo::new(
            "fault-probe",
            0x2000,
            0x1000,
            VmFlags::READ | VmFlags::WRITE | VmFlags::MAP,
        );
        assert!(vmo.track_frame(
            0,
            PhysRun::borrowed::<DummyAddressSpace>(PhysAddr::new(pages.as_mut_ptr() as usize), 1),
            VmoPagePurpose::Anonymous,
            true,
        ));

        assert_eq!(
            vmo.resolve_fault(0x20, MMUFlags::READ),
            VmFaultResolution::RetryAfterMap
        );
        assert_eq!(
            vmo.resolve_fault(0x1020, MMUFlags::READ),
            VmFaultResolution::Unresolved
        );
        assert_eq!(
            vmo.resolve_fault(0x3000, MMUFlags::READ),
            VmFaultResolution::InvalidRange
        );
    }

    #[test]
    fn vmo_resolve_fault_rejects_disallowed_accesses() {
        let vmo = Vmo::new("readonly", 0x1000, 0x1000, VmFlags::READ | VmFlags::MAP);
        assert_eq!(
            vmo.resolve_fault(0x40, MMUFlags::WRITE),
            VmFaultResolution::ProtectionDenied
        );
    }

    #[test]
    fn vmo_op_range_commit_zero_query_and_decommit_roundtrip() {
        let first_page = Box::leak(Box::new([0xAAu8; 0x1000]));
        let second_page = Box::leak(Box::new([0xBBu8; 0x1000]));
        let allocator = Box::leak(Box::new(CowTestAllocator::new(
            [
                first_page.as_mut_ptr() as usize,
                second_page.as_mut_ptr() as usize,
            ],
            2,
        )));
        let vmo = Vmo::new(
            "range-ops",
            0x2000,
            0x1000,
            VmFlags::READ | VmFlags::WRITE | VmFlags::MAP,
        );

        assert_eq!(vmo.query_committed_bytes(0, 0x2000), Ok(0));
        assert_eq!(
            vmo.commit_range::<DummyAddressSpace>(0, 0x2000, VmoPagePurpose::Anonymous, allocator,),
            Ok(0x2000)
        );
        assert_eq!(vmo.query_committed_bytes(0, 0x2000), Ok(0x2000));

        assert!(vmo.write(0x40, b"first-page"));
        assert!(vmo.write(0x1040, b"second"));
        assert_eq!(vmo.zero_range::<DummyAddressSpace>(0, 0x1000), Ok(0x1000));
        let mut first = [0u8; 10];
        let mut second = [0u8; 6];
        assert!(vmo.read(0x40, &mut first));
        assert!(vmo.read(0x1040, &mut second));
        assert_eq!(first, [0u8; 10]);
        assert_eq!(&second, b"second");

        assert_eq!(vmo.decommit_range(0x1000, 0x1000), Ok(0x1000));
        assert_eq!(vmo.query_committed_bytes(0, 0x2000), Ok(0x1000));
        assert!(!vmo.read(0x1040, &mut second));
    }

    #[test]
    fn vmo_op_range_rejects_partial_decommit_of_one_multi_page_extent() {
        let pages = Box::leak(Box::new([0u8; 0x2000]));
        let vmo = Vmo::new(
            "range-ops",
            0x2000,
            0x1000,
            VmFlags::READ | VmFlags::WRITE | VmFlags::MAP,
        );
        assert!(vmo.track_frame(
            0,
            PhysRun::borrowed::<DummyAddressSpace>(PhysAddr::new(pages.as_mut_ptr() as usize), 2),
            VmoPagePurpose::Anonymous,
            true,
        ));

        assert_eq!(
            vmo.decommit_range(0x1000, 0x1000),
            Err(VmoRangeOpError::InvalidArgument)
        );
        assert_eq!(vmo.query_committed_bytes(0, 0x2000), Ok(0x2000));
    }
}
