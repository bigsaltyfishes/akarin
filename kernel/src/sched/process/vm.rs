use libakarin_core::memory::{VMO_DEFAULT_INTERFACE_CAPS, VSpaceError, VmoFaultPolicy};
use libakarin_syscall::{VmError, VmoChildMode, VmoOpRangeOperation};

use super::*;
use crate::error::ObjectOrUnderlyingError;

pub type ProcessVmError = ObjectOrUnderlyingError<VmError>;

impl From<VmControlError> for ProcessVmError {
    fn from(value: VmControlError) -> Self {
        match value.into_object_or_vm() {
            Ok(error) => Self::Object(error),
            Err(error) => Self::Underlying(error),
        }
    }
}

impl From<VSpaceError> for ProcessVmError {
    fn from(value: VSpaceError) -> Self {
        let error = match value {
            VSpaceError::InvalidRange => VmError::InvalidRange,
            VSpaceError::AlreadyMapped => VmError::AlreadyMapped,
            VSpaceError::NotMapped => VmError::NotMapped,
            VSpaceError::PermissionDenied => VmError::PermissionDenied,
        };
        Self::Underlying(error)
    }
}

impl Process {
    fn process_vm_guard_error(
        error: ObjectError,
        invalid: VmError,
        missing: VmError,
    ) -> ProcessVmError {
        match error {
            ObjectError::InvalidArgument => ProcessVmError::Underlying(invalid),
            ObjectError::ObjectNotFound => ProcessVmError::Underlying(missing),
            other => ProcessVmError::Object(other),
        }
    }

    fn vspace_info_inner(&self) -> Result<VSpaceInfo, ProcessVmError> {
        let guard = self.vspace.lock();
        let vspace = guard
            .as_ref()
            .ok_or(ProcessVmError::Underlying(VmError::NotMapped))?;
        Ok(vspace.info())
    }

    fn locate_ptr_inner(&self, addr: VirtAddr) -> Result<VmPointerRegion, ProcessVmError> {
        let guard = self.vspace.lock();
        let vspace = guard
            .as_ref()
            .ok_or(ProcessVmError::Underlying(VmError::NotMapped))?;
        Ok(vspace.locate_ptr(addr))
    }

    fn mapping_at_inner(&self, addr: VirtAddr) -> Result<Option<VmarMapping>, ProcessVmError> {
        let guard = self.vspace.lock();
        let vspace = guard
            .as_ref()
            .ok_or(ProcessVmError::Underlying(VmError::NotMapped))?;
        Ok(vspace.mapping_at(addr))
    }

    fn vmo_page_purpose(purpose: RegionPurpose) -> VmoPagePurpose {
        match purpose {
            RegionPurpose::General | RegionPurpose::User => VmoPagePurpose::Anonymous,
            RegionPurpose::KernelText | RegionPurpose::KernelData => VmoPagePurpose::KernelMeta,
            RegionPurpose::KernelHeap => VmoPagePurpose::Heap,
            RegionPurpose::KernelStack => VmoPagePurpose::Stack,
            RegionPurpose::PageTable => VmoPagePurpose::PageTable,
            RegionPurpose::DeviceMmio => VmoPagePurpose::Dma,
        }
    }

    fn base_page_count(range: VmRange) -> usize {
        range.len() / libakarin_core::memory::PAGE_SIZE
    }

    fn translate_paging_error(err: PagingError) -> ObjectError {
        match err {
            PagingError::NotMapped => ObjectError::ObjectNotFound,
            PagingError::AlreadyMapped => ObjectError::InvalidArgument,
            PagingError::NoMemory
            | PagingError::ParentIsHugePage
            | PagingError::InvalidFrameAddress
            | PagingError::UnsupportedPageSize => ObjectError::InvalidArgument,
        }
    }

    fn with_page_table<R>(
        &self,
        f: impl FnOnce(&mut PageTable) -> Result<R, ObjectError>,
    ) -> Result<R, ObjectError> {
        let root = self.address_space_root()?;
        let root_virt = Machine::phys_to_virt(root).ok_or(ObjectError::InvalidArgument)?;
        let mut page_table = PageTable::from_raw(
            crate::RuntimeServices::global().frame_allocator(),
            root_virt.as_mut_ptr(),
        );
        f(&mut page_table)
    }

    fn ensure_mapping_page_backed(
        &self,
        mapping: &VmarMapping,
        vmo_offset: usize,
    ) -> Result<VmoPageMetadata, ObjectError> {
        let page_offset =
            vmo_offset / libakarin_core::memory::PAGE_SIZE * libakarin_core::memory::PAGE_SIZE;
        if let Some(meta) = mapping.vmo.page_at(page_offset) {
            return Ok(meta);
        }

        if mapping.vmo.fault_policy() == VmoFaultPolicy::PagerBacked {
            return Err(ObjectError::ObjectNotFound);
        }

        if mapping.vmo.child_info().is_some() {
            return Err(ObjectError::ObjectNotFound);
        }

        if !matches!(mapping.vmo.backing(), VmoBacking::Paged) {
            return Err(ObjectError::ObjectNotFound);
        }

        let frame_allocator = crate::RuntimeServices::global().frame_allocator();
        let virt = frame_allocator
            .alloc(None, FrameZone::default(), 1)
            .map_err(|_| ObjectError::InvalidArgument)?;
        let phys = Machine::virt_to_phys(virt).ok_or(ObjectError::InvalidArgument)?;
        let frame = unsafe { PhysFrame::from_addr(Some(frame_allocator), phys, 1) };
        let run = PhysRun::from_frame(frame);
        if !mapping.vmo.track_frame(
            page_offset,
            run,
            Self::vmo_page_purpose(mapping.purpose),
            true,
        ) {
            return Err(ObjectError::InvalidArgument);
        }

        mapping
            .vmo
            .page_at(page_offset)
            .ok_or(ObjectError::ObjectNotFound)
    }

    fn install_mapping_page_in_page_table(
        &self,
        mapping: &VmarMapping,
        addr: VirtAddr,
    ) -> Result<(), ObjectError> {
        let page_base =
            addr.as_usize() / libakarin_core::memory::PAGE_SIZE * libakarin_core::memory::PAGE_SIZE;
        let page_virt = VirtAddr::new(page_base);
        let vmo_offset = mapping
            .vmo_offset_for_addr(page_virt)
            .ok_or(ObjectError::InvalidArgument)?;
        let meta = self.ensure_mapping_page_backed(mapping, vmo_offset)?;
        let mut mmu_flags = mapping
            .vmo
            .mapping_mmu_flags_for_page(vmo_offset, mapping.flags.mmu_flags());
        let cache = mapping.flags.cache_policy();
        mmu_flags.set_cache_policy(cache);
        mmu_flags.remove(MMUFlags::HUGE_PAGE);

        self.with_page_table(|page_table| {
            match page_table.entry_mut(Page::new(page_virt, PageSize::Size4K)) {
                Ok((entry, size)) => {
                    if size != PageSize::Size4K {
                        return Err(ObjectError::InvalidArgument);
                    }

                    entry.set_phys_addr(meta.phys, mmu_flags);
                    Machine::invalidate_tlb(page_virt);
                    Ok(())
                }
                Err(PagingError::NotMapped) => {
                    let frame = unsafe { PhysFrame::from_addr(None, meta.phys, 1) };
                    let invalidator = unsafe {
                        page_table.map(
                            Page::new(page_virt, PageSize::Size4K),
                            frame,
                            mmu_flags,
                            cache,
                        )
                    }
                    .map_err(Self::translate_paging_error)?;
                    invalidator.invalidate();
                    Ok(())
                }
                Err(err) => Err(Self::translate_paging_error(err)),
            }
        })
    }

    fn service_private_cow_fault(&self, mapping: &VmarMapping, addr: VirtAddr) -> bool {
        let page_base =
            addr.as_usize() / libakarin_core::memory::PAGE_SIZE * libakarin_core::memory::PAGE_SIZE;
        let page_addr = VirtAddr::new(page_base);
        let vmo_offset = match mapping.vmo_offset_for_addr(page_addr) {
            Some(offset) => offset,
            None => return false,
        };
        let frame_allocator = crate::RuntimeServices::global().frame_allocator();
        if mapping
            .vmo
            .materialize_private_cow_page::<Machine>(
                vmo_offset,
                Self::vmo_page_purpose(mapping.purpose),
                frame_allocator,
            )
            .is_none()
        {
            return false;
        }
        self.install_mapping_page_in_page_table(mapping, page_addr)
            .is_ok()
    }

    pub(crate) async fn service_pager_fault(&self, detail: &ProcessPageFault) -> bool {
        let Some(mapping) = detail.mapping.as_ref() else {
            return false;
        };

        let purpose = Self::vmo_page_purpose(mapping.purpose);
        let pager_result = crate::service::pager_runtime()
            .resolve_mapping_fault(mapping, detail.addr, detail.access, purpose)
            .await;
        if pager_result.is_err() {
            return false;
        }

        self.install_mapping_page_in_page_table(mapping, detail.addr)
            .is_ok()
    }

    pub(super) fn map_mapping_in_page_table(
        &self,
        mapping: &VmarMapping,
    ) -> Result<(), ObjectError> {
        let page_count = Self::base_page_count(mapping.range);
        for page_index in 0..page_count {
            let virt = VirtAddr::new(
                mapping.range.start().as_usize() + page_index * libakarin_core::memory::PAGE_SIZE,
            );
            match self.install_mapping_page_in_page_table(mapping, virt) {
                Ok(()) => {}
                Err(ObjectError::ObjectNotFound)
                    if mapping.vmo.fault_policy() == VmoFaultPolicy::PagerBacked => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn protect_mapping_in_page_table(&self, mapping: &VmarMapping) -> Result<(), ObjectError> {
        let cache = mapping.flags.cache_policy();
        let page_count = Self::base_page_count(mapping.range);

        self.with_page_table(|page_table| {
            for page_index in 0..page_count {
                let virt = VirtAddr::new(
                    mapping.range.start().as_usize()
                        + page_index * libakarin_core::memory::PAGE_SIZE,
                );
                let vmo_offset = mapping
                    .vmo_offset_for_addr(virt)
                    .ok_or(ObjectError::InvalidArgument)?;
                let mut mmu_flags = mapping
                    .vmo
                    .mapping_mmu_flags_for_page(vmo_offset, mapping.flags.mmu_flags());
                mmu_flags.set_cache_policy(cache);
                mmu_flags.remove(MMUFlags::HUGE_PAGE);
                let page = Page::new(virt, PageSize::Size4K);
                let (entry, _) = page_table
                    .entry_mut(page)
                    .map_err(Self::translate_paging_error)?;
                entry.set_flags(mmu_flags);
                entry.set_cache_policy(cache);
                Machine::invalidate_tlb(virt);
            }
            Ok(())
        })
    }

    fn unmap_entry_from_page_table_inner(&self, entry: &VmarEntry) -> Result<(), ObjectError> {
        match entry {
            VmarEntry::Guard(_) => Ok(()),
            VmarEntry::Mapping(mapping) => {
                let page_count = Self::base_page_count(mapping.range);
                self.with_page_table(|page_table| {
                    for page_index in 0..page_count {
                        let virt = VirtAddr::new(
                            mapping.range.start().as_usize()
                                + page_index * libakarin_core::memory::PAGE_SIZE,
                        );
                        let page = Page::new(virt, PageSize::Size4K);
                        match page_table.entry_mut(page) {
                            Ok((entry, _)) => {
                                entry.clear();
                                Machine::invalidate_tlb(virt);
                            }
                            Err(PagingError::NotMapped) => {}
                            Err(err) => return Err(Self::translate_paging_error(err)),
                        }
                    }
                    Ok(())
                })
            }
            VmarEntry::Region { child, .. } => {
                for child_entry in child.entries() {
                    self.unmap_entry_from_page_table_inner(&child_entry)?;
                }
                Ok(())
            }
        }
    }

    pub(super) fn unmap_entry_from_page_table(&self, entry: &VmarEntry) -> Result<(), ObjectError> {
        self.unmap_entry_from_page_table_inner(entry)
    }

    /// Create one anonymous paged VMO and return a process-local handle to it.
    pub fn create_vmo(
        &self,
        name: impl Into<String>,
        size: usize,
        page_size: usize,
        flags: VmFlags,
    ) -> Result<Handle, ObjectError> {
        self.create_anonymous_object(
            Payload::new(Vmo::new(name, size, page_size, flags)),
            Capability::READ | Capability::WRITE | Capability::EXECUTE,
            VMO_DEFAULT_INTERFACE_CAPS,
        )
    }

    /// Create one physical VMO backed by the supplied physical base address.
    pub fn create_physical_vmo(
        &self,
        name: impl Into<String>,
        size: usize,
        page_size: usize,
        flags: VmFlags,
        base: PhysAddr,
    ) -> Result<Handle, ObjectError> {
        self.create_anonymous_object(
            Payload::new(Vmo::new_physical::<Machine>(
                name, size, page_size, flags, base,
            )),
            Capability::READ | Capability::WRITE | Capability::EXECUTE,
            VMO_DEFAULT_INTERFACE_CAPS,
        )
    }

    /// Create one child VMO from the parent handle at `slot`.
    ///
    /// `mode` selects whether the child is a shared view or a private COW
    /// clone.
    pub fn create_child_vmo(
        &self,
        slot: u32,
        offset: usize,
        size: usize,
        mode: VmoChildMode,
    ) -> Result<Handle, ProcessVmError> {
        let child = match self.with_handle(slot, |handle| {
            handle.write_cp_with::<Vmo, _, _>(|vmo| match mode {
                VmoChildMode::SharedView => vmo.create_child_vm(offset, size),
                VmoChildMode::PrivateCow => vmo.create_private_child_vm(offset, size),
            })
        }) {
            Ok(Ok(child)) => child,
            Ok(Err(error)) => return Err(ProcessVmError::from(error)),
            Err(error) => return Err(ProcessVmError::Object(error)),
        };

        self.create_anonymous_object(
            Payload::new(child),
            Capability::READ | Capability::WRITE | Capability::EXECUTE,
            VMO_DEFAULT_INTERFACE_CAPS,
        )
        .map_err(ProcessVmError::Object)
    }

    /// Perform one range operation on the VMO stored in `slot`.
    pub fn vmo_op_range(
        &self,
        slot: u32,
        operation: VmoOpRangeOperation,
        offset: usize,
        len: usize,
    ) -> Result<usize, ProcessVmError> {
        let result = match operation {
            VmoOpRangeOperation::Commit => self.with_handle(slot, |handle| {
                handle.write_cp_with::<Vmo, _, _>(|vmo| {
                    let allocator = crate::RuntimeServices::global().frame_allocator();
                    vmo.commit_range_vm::<Machine>(
                        offset,
                        len,
                        Self::vmo_page_purpose(RegionPurpose::User),
                        allocator,
                    )
                })
            }),
            VmoOpRangeOperation::Decommit => self.with_handle(slot, |handle| {
                handle.write_cp_with::<Vmo, _, _>(|vmo| vmo.decommit_range_vm(offset, len))
            }),
            VmoOpRangeOperation::Zero => self.with_handle(slot, |handle| {
                handle.write_cp_with::<Vmo, _, _>(|vmo| vmo.zero_range_vm::<Machine>(offset, len))
            }),
            VmoOpRangeOperation::QueryCommitted => self.with_handle(slot, |handle| {
                handle.read_cp_with::<Vmo, _, _>(|vmo| vmo.query_committed_bytes_vm(offset, len))
            }),
        };

        match result {
            Ok(Ok(bytes)) => Ok(bytes),
            Ok(Err(error)) => Err(ProcessVmError::from(error)),
            Err(error) => Err(ProcessVmError::Object(error)),
        }
    }

    /// Derive one read-only handle to the process root VMAR.
    pub fn derive_root_vmar_handle(&self) -> Result<Handle, ObjectError> {
        self.root_vmar_admin
            .lock()
            .as_ref()
            .ok_or(ObjectError::ObjectNotFound)?
            .derive_handle(Capability::READ, VMAR_DEFAULT_INTERFACE_CAPS)
    }

    /// Return one snapshot of the process address-space layout state.
    pub fn vspace_info(&self) -> Result<VSpaceInfo, ProcessVmError> {
        self.vspace_info_inner()
    }

    /// Classify `addr` within the process logical address-space layout.
    pub fn locate_ptr(&self, addr: VirtAddr) -> Result<VmPointerRegion, ProcessVmError> {
        self.locate_ptr_inner(addr)
    }

    /// Return the installed VMAR mapping covering `addr`, if one exists.
    pub fn mapping_at(&self, addr: VirtAddr) -> Result<Option<VmarMapping>, ProcessVmError> {
        self.mapping_at_inner(addr)
    }

    /// Validate that `range` is mapped in user space with `required` access.
    pub fn validate_user_range(
        &self,
        range: VmRange,
        required: VmFlags,
    ) -> Result<(), VSpaceError> {
        let guard = self.vspace.lock();
        let vspace = guard.as_ref().ok_or(VSpaceError::NotMapped)?;
        vspace.validate_range(range, required)
    }

    /// Classify one user page fault against the current logical address space.
    ///
    /// This stage only produces one structured fault record. The current
    /// scheduler still terminates every user page fault, but later VM phases
    /// will let specific classifications resume after installing pages, making
    /// private copies, or talking to one pager.
    pub fn resolve_user_fault(
        &self,
        addr: VirtAddr,
        access: MMUFlags,
    ) -> Result<ProcessUserFaultResolution, ProcessVmError> {
        let region = self.locate_ptr(addr)?;
        let detail = match &region {
            VmPointerRegion::Unmapped => ProcessPageFault {
                addr,
                access,
                region: region.clone(),
                kind: ProcessPageFaultKind::NotMapped,
                mapping: None,
            },
            VmPointerRegion::GuardPage => ProcessPageFault {
                addr,
                access,
                region: region.clone(),
                kind: ProcessPageFaultKind::GuardPage,
                mapping: None,
            },
            VmPointerRegion::Static(_) => ProcessPageFault {
                addr,
                access,
                region: region.clone(),
                kind: ProcessPageFaultKind::StaticLayout,
                mapping: None,
            },
            VmPointerRegion::Reserved(_) => ProcessPageFault {
                addr,
                access,
                region: region.clone(),
                kind: ProcessPageFaultKind::ReservedRange,
                mapping: None,
            },
            VmPointerRegion::Mapping(mapping) => {
                let resolution = mapping.resolve_fault(addr, access);
                if resolution == VmFaultResolution::RetryAfterMap {
                    // The logical mapping and VMO backing already exist; only
                    // the concrete page-table entry needs to be restored.
                    self.install_mapping_page_in_page_table(mapping, addr)
                        .map_err(ProcessVmError::Object)?;
                    return Ok(ProcessUserFaultResolution::Resume);
                }
                if resolution == VmFaultResolution::PrivateCow
                    && access.contains(MMUFlags::WRITE)
                    && self.service_private_cow_fault(mapping, addr)
                {
                    return Ok(ProcessUserFaultResolution::Resume);
                }

                let kind = match resolution {
                    VmFaultResolution::ProtectionDenied => ProcessPageFaultKind::ProtectionDenied,
                    VmFaultResolution::Unresolved => ProcessPageFaultKind::UnresolvedMapping,
                    VmFaultResolution::PrivateCow => ProcessPageFaultKind::PrivateCow,
                    VmFaultResolution::PagerBacked => ProcessPageFaultKind::PagerBacked,
                    VmFaultResolution::InvalidRange => ProcessPageFaultKind::InvalidMappingRange,
                    VmFaultResolution::RetryAfterMap => ProcessPageFaultKind::RetryAfterMap,
                };
                let fault = ProcessPageFault {
                    addr,
                    access,
                    region: region.clone(),
                    kind,
                    mapping: Some(mapping.clone()),
                };
                if kind == ProcessPageFaultKind::PagerBacked {
                    return Ok(ProcessUserFaultResolution::Block(fault));
                }
                fault
            }
        };
        Ok(ProcessUserFaultResolution::Terminate(detail))
    }

    fn create_child_vmar(&self, child: Arc<Vmar>) -> Result<Handle, ObjectError> {
        self.create_anonymous_object(
            Payload::new(child.as_ref().clone()),
            Capability::READ | Capability::WRITE | Capability::EXECUTE,
            VMAR_DEFAULT_INTERFACE_CAPS,
        )
    }

    /// Allocate one child VMAR at the exact `range` inside the parent VMAR in
    /// `slot`.
    pub fn allocate_child_vmar(&self, slot: u32, range: VmRange) -> Result<Handle, ProcessVmError> {
        let child = match self.with_handle(slot, |handle| {
            handle.write_cp_with::<Vmar, _, _>(|vmar| vmar.allocate_child_vm(range))
        }) {
            Ok(Ok(child)) => child,
            Ok(Err(error)) => return Err(ProcessVmError::from(error)),
            Err(error) => return Err(ProcessVmError::Object(error)),
        };
        self.create_child_vmar(child)
            .map_err(ProcessVmError::Object)
    }

    /// Allocate one child VMAR of `size` at any suitable range inside the
    /// parent VMAR in `slot`.
    pub fn allocate_child_vmar_any(
        &self,
        slot: u32,
        size: usize,
    ) -> Result<(Handle, VmRange), ProcessVmError> {
        let child = match self.with_handle(slot, |handle| {
            handle.write_cp_with::<Vmar, _, _>(|vmar| vmar.allocate_child_any_vm(size))
        }) {
            Ok(Ok(child)) => child,
            Ok(Err(error)) => return Err(ProcessVmError::from(error)),
            Err(error) => return Err(ProcessVmError::Object(error)),
        };
        let range = child.range();
        let handle = self
            .create_child_vmar(child)
            .map_err(ProcessVmError::Object)?;
        Ok((handle, range))
    }

    /// Create one anonymous owned object and return a delegated public handle.
    ///
    /// The owner handle stays in the process anonymous-object registry so the
    /// object lifetime is tied to the process even if only delegated handles
    /// escape.
    pub fn create_anonymous_object(
        &self,
        payload: Payload,
        capability: Capability,
        interface_caps: u32,
    ) -> Result<Handle, ObjectError> {
        if capability.intersects(Capability::ADMIN | Capability::AGENT) {
            return Err(ObjectError::InvalidArgument);
        }

        let owner = Handle::new_anonymous(payload, Capability::ADMIN | Capability::AGENT);
        let public = owner.derive_handle(capability, interface_caps)?;
        self.retain_anonymous_owner(owner)?;
        Ok(public)
    }

    /// Create one anonymous owner handle that preserves `ADMIN` authority for
    /// the caller.
    pub fn create_anonymous_admin_handle(
        &self,
        payload: Payload,
        capability: Capability,
    ) -> Result<Handle, ObjectError> {
        let allowed =
            Capability::ADMIN | Capability::READ | Capability::WRITE | Capability::EXECUTE;
        if !capability.contains(Capability::ADMIN)
            || capability.contains(Capability::AGENT)
            || !allowed.contains(capability)
        {
            return Err(ObjectError::InvalidArgument);
        }

        let mut owner = Handle::new_anonymous(payload, Capability::ADMIN | Capability::AGENT);
        owner.downgrade(allowed & !capability | Capability::AGENT, 0);
        Ok(owner)
    }

    /// Retain one anonymous owner handle inside the process-local owner table.
    pub fn retain_anonymous_owner(&self, owner: Handle) -> Result<(), ObjectError> {
        let key = owner.object_key()?;
        self.anon_objects.write().insert(key, owner);
        Ok(())
    }

    /// Destroy one anonymous object referenced by the delegated handle in
    /// `slot`.
    pub fn destroy_anonymous_object(&self, slot: u32) -> Result<(), ObjectError> {
        let handle = self
            .handle_table
            .write()
            .remove(&slot)
            .ok_or(ObjectError::ObjectNotFound)?;
        let key = handle.object_key()?;
        let still_referenced = self
            .handle_table
            .read()
            .values()
            .any(|other| other.object_key().ok() == Some(key));
        if !still_referenced {
            self.anon_objects.write().remove(&key);
        }
        Ok(())
    }

    /// Unmap the VMAR entry starting at `start` from the VMAR stored in `slot`.
    pub fn vmar_unmap(&self, slot: u32, start: VirtAddr) -> Result<VmarEntry, ProcessVmError> {
        let entry = match self.with_handle(slot, |handle| {
            handle.write_cp_with::<Vmar, _, _>(|vmar| vmar.unmap_vm(start))
        }) {
            Ok(Ok(entry)) => entry,
            Ok(Err(error)) => return Err(ProcessVmError::from(error)),
            Err(error) => return Err(ProcessVmError::Object(error)),
        };
        self.unmap_entry_from_page_table(&entry).map_err(|error| {
            Self::process_vm_guard_error(error, VmError::InvalidArgument, VmError::NotMapped)
        })?;
        Ok(entry)
    }

    /// Map the VMO in `vmo_slot` into the VMAR in `vmar_slot`.
    pub fn vmar_map(
        &self,
        vmar_slot: u32,
        vmo_slot: u32,
        range: VmRange,
        vmo_offset: usize,
        flags: VmFlags,
        purpose: RegionPurpose,
    ) -> Result<(), ProcessVmError> {
        let vmo = match self.with_handle(vmo_slot, |handle| {
            handle.read_cp_with::<Vmo, _, _>(|vmo| vmo.share_vm())
        }) {
            Ok(Ok(vmo)) => vmo,
            Ok(Err(error)) => return Err(ProcessVmError::from(error)),
            Err(error) => return Err(ProcessVmError::Object(error)),
        };

        let mapping = VmarMapping {
            range,
            flags,
            purpose,
            vmo,
            vmo_offset,
        };
        match self.with_handle(vmar_slot, |handle| {
            handle.write_cp_with::<Vmar, _, _>(|vmar| {
                vmar.map_vmo_vm(range, Arc::clone(&mapping.vmo), vmo_offset, flags, purpose)
            })
        }) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(ProcessVmError::from(error)),
            Err(error) => return Err(ProcessVmError::Object(error)),
        }
        if let Err(err) = self.map_mapping_in_page_table(&mapping) {
            let _ = self.with_handle(vmar_slot, |handle| {
                handle.write_cp_with::<Vmar, _, _>(|vmar| vmar.unmap(range.start()))
            });
            return Err(Self::process_vm_guard_error(
                err,
                VmError::InvalidArgument,
                VmError::NotMapped,
            ));
        }
        Ok(())
    }

    /// Update access flags for one mapped range inside the VMAR in `slot`.
    pub fn vmar_protect(
        &self,
        slot: u32,
        range: VmRange,
        flags: VmFlags,
    ) -> Result<(), ProcessVmError> {
        let (old, new_mapping) = match self.with_handle(slot, |handle| {
            handle.write_cp_with::<Vmar, _, _>(|vmar| vmar.protect_vm(range, flags))
        }) {
            Ok(Ok(values)) => values,
            Ok(Err(error)) => return Err(ProcessVmError::from(error)),
            Err(error) => return Err(ProcessVmError::Object(error)),
        };
        if let Err(err) = self.protect_mapping_in_page_table(&new_mapping) {
            let _ = self.with_handle(slot, |handle| {
                handle.write_cp_with::<Vmar, _, _>(|vmar| vmar.protect(range, old.flags))
            });
            return Err(Self::process_vm_guard_error(
                err,
                VmError::InvalidArgument,
                VmError::NotMapped,
            ));
        }
        Ok(())
    }

    /// Destroy one anonymously owned VMAR handle from the process handle table.
    pub fn destroy_vmar(&self, slot: u32) -> Result<(), ObjectError> {
        self.with_handle(slot, |handle| {
            handle.admin_cp_with::<Vmar, _, _>(|vmar| vmar.destroy_allowed())
        })??;
        self.destroy_anonymous_object(slot)
    }
}
