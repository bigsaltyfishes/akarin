use alloc::{format, vec};

use libakarin_core::memory::{
    PAGE_SIZE, RegionPurpose, VmFlags, VmLayoutSegment, VmRange, Vmar, VmarEntry, VmarMapping, Vmo,
};
use libakarin_dyld::{
    DyldLinker, DyldRebaseTarget, ImageParserError, LinkerError, MachOExecutable, MachOImage,
    MachOSegmentProtections,
};
use libakarin_machine_core::memory::VirtAddr;
use libakarin_object::{Capability, ObjectError};
use libakarin_syscall::{
    AT_AK_HEAP_BASE, AT_AK_HEAP_LIMIT, AT_AK_HEAP_VMAR, AT_AK_MAPPED_VMAR, AT_AK_PAGE_SIZE,
    AT_AK_PROCESS_SELF, AT_AK_ROOT_NS, AT_AK_ROOT_VMAR, AT_AK_SYSCALL_TABLE, AT_ENTRY, AT_NULL,
    AT_PAGESZ, PROCESS_QUERY, ProcessLoadInfo,
};

use super::*;

const BOOTSTRAP_USER_STACK_PAGES: usize = 16;

pub(crate) struct BootstrapUserAbi {
    entry_point: usize,
    process_self_slot: u32,
    root_ns_slot: u32,
    root_vmar_slot: u32,
    heap_vmar_slot: u32,
    mapped_vmar_slot: u32,
    syscall_table_slot: u32,
    page_size: usize,
    heap_base: usize,
    heap_limit: usize,
}

/// Errors returned while loading the first userspace bootstrap image.
#[derive(Debug)]
pub enum BootstrapLoadError {
    Object(ObjectError),
    Vm(ProcessVmError),
    Image(ImageParserError),
    Link(LinkerError),
    InvalidExecutable(&'static str),
}

impl From<ObjectError> for BootstrapLoadError {
    fn from(value: ObjectError) -> Self {
        Self::Object(value)
    }
}

impl From<ProcessVmError> for BootstrapLoadError {
    fn from(value: ProcessVmError) -> Self {
        Self::Vm(value)
    }
}

impl From<ImageParserError> for BootstrapLoadError {
    fn from(value: ImageParserError) -> Self {
        Self::Image(value)
    }
}

impl From<LinkerError> for BootstrapLoadError {
    fn from(value: LinkerError) -> Self {
        Self::Link(value)
    }
}

struct VmarRebaseTarget {
    vmar: Arc<Vmar>,
}

impl VmarRebaseTarget {
    fn new(vmar: Arc<Vmar>) -> Self {
        Self { vmar }
    }
}

impl DyldRebaseTarget for VmarRebaseTarget {
    fn read_at_vmaddr(&self, vm_addr: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        let addr = VirtAddr::new(vm_addr as usize);
        let mapping = self
            .vmar
            .mapping_at(addr)
            .ok_or("rebase address is not mapped in VMAR")?;
        let offset = mapping
            .vmo_offset_for_addr(addr)
            .ok_or("rebase address translation failed")?;
        mapping
            .vmo
            .read(offset, buffer)
            .then_some(())
            .ok_or("failed to read rebase bytes from VMO")
    }

    fn write_at_vmaddr(&self, vm_addr: u64, data: &[u8]) -> Result<(), &'static str> {
        let addr = VirtAddr::new(vm_addr as usize);
        let mapping = self
            .vmar
            .mapping_at(addr)
            .ok_or("rebase address is not mapped in VMAR")?;
        let offset = mapping
            .vmo_offset_for_addr(addr)
            .ok_or("rebase address translation failed")?;
        mapping
            .vmo
            .write(offset, data)
            .then_some(())
            .ok_or("failed to write rebase bytes into VMO")
    }
}

impl Process {
    /// Parse, rebase, and map one userspace image from the supplied `VMO`.
    pub fn load_image_from_vmo(
        &self,
        image_vmo: &Arc<Vmo>,
    ) -> Result<ProcessLoadInfo, BootstrapLoadError> {
        let stream_size = image_vmo.stream_size();
        if stream_size == 0 {
            return Err(BootstrapLoadError::InvalidExecutable(
                "process image VMO is empty",
            ));
        }

        let mut bytes = vec![0u8; stream_size];
        if !image_vmo.read(0, &mut bytes) {
            return Err(BootstrapLoadError::InvalidExecutable(
                "process image VMO contents are unreadable",
            ));
        }
        self.load_image_bytes(bytes)
    }

    fn load_image_bytes(
        &self,
        image_bytes: alloc::vec::Vec<u8>,
    ) -> Result<ProcessLoadInfo, BootstrapLoadError> {
        let mut image = MachOImage::from_bytes(image_bytes)?;
        let executable = image.executable()?;
        let slide = Self::bootstrap_slide(&executable)?;
        let image_vmar = self.segment_vmar(VmLayoutSegment::UserImage)?;

        for segment in executable.segments() {
            self.map_bootstrap_segment(&image_vmar, &image, *segment, slide)?;
        }

        DyldLinker::new(&mut image, slide).link(&VmarRebaseTarget::new(Arc::clone(&image_vmar)))?;

        let entry_point = executable.entry_point().checked_add(slide).ok_or(
            BootstrapLoadError::InvalidExecutable("bootstrap entry address overflowed"),
        )?;
        let image_base = executable.image_base().checked_add(slide).ok_or(
            BootstrapLoadError::InvalidExecutable("bootstrap image base overflowed"),
        )?;
        let image_end = executable.image_end().checked_add(slide).ok_or(
            BootstrapLoadError::InvalidExecutable("bootstrap image end overflowed"),
        )?;

        Ok(ProcessLoadInfo {
            entry_ip: entry_point,
            image_base,
            image_end,
            slide,
        })
    }

    fn bootstrap_slide(executable: &MachOExecutable) -> Result<usize, BootstrapLoadError> {
        let base = executable.image_base();
        let user_base = VmLayoutSegment::UserImage.start();
        let slide = user_base.saturating_sub(base);
        let image_end = executable.image_end().checked_add(slide).ok_or(
            BootstrapLoadError::InvalidExecutable("bootstrap image end overflowed"),
        )?;
        let image_limit = VmLayoutSegment::UserImage.end_exclusive().ok_or(
            BootstrapLoadError::InvalidExecutable("user image segment must be bounded"),
        )?;
        if image_end > image_limit {
            return Err(BootstrapLoadError::InvalidExecutable(
                "bootstrap image does not fit inside the user image region",
            ));
        }
        Ok(slide)
    }

    fn page_align_up(value: usize) -> Result<usize, BootstrapLoadError> {
        let addend = PAGE_SIZE
            .checked_sub(1)
            .ok_or(BootstrapLoadError::InvalidExecutable("invalid page size"))?;
        value
            .checked_add(addend)
            .map(|aligned| aligned & !addend)
            .ok_or(BootstrapLoadError::InvalidExecutable(
                "page alignment overflowed",
            ))
    }

    fn map_bootstrap_segment(
        &self,
        image_vmar: &Arc<Vmar>,
        image: &MachOImage,
        segment: libakarin_dyld::MachOLoadSegment,
        slide: usize,
    ) -> Result<(), BootstrapLoadError> {
        let vm_start =
            segment
                .vm_addr()
                .checked_add(slide)
                .ok_or(BootstrapLoadError::InvalidExecutable(
                    "segment virtual address overflowed",
                ))?;
        let vm_size = Self::page_align_up(segment.vm_size())?;
        if !vm_start.is_multiple_of(PAGE_SIZE) || vm_size == 0 {
            return Err(BootstrapLoadError::InvalidExecutable(
                "bootstrap segments must be page aligned and non-empty",
            ));
        }
        let vm_end = vm_start
            .checked_add(vm_size)
            .ok_or(BootstrapLoadError::InvalidExecutable(
                "segment virtual range overflowed",
            ))?;
        let range = VmRange::new(VirtAddr::new(vm_start), VirtAddr::new(vm_end)).ok_or(
            BootstrapLoadError::InvalidExecutable("segment virtual range is invalid"),
        )?;
        let mut bytes = vec![0u8; vm_size];
        let file_bytes = image.segment_bytes(&segment)?;
        bytes[..file_bytes.len()].copy_from_slice(file_bytes);

        let mapping_flags = Self::mapping_flags(segment.init_prot());
        let vmo = Arc::new(Vmo::new(
            format!("proc{}-bootstrap-seg{}", self.pid(), segment.seg_index()),
            vm_size,
            PAGE_SIZE,
            VmFlags::READ | VmFlags::WRITE | VmFlags::EXECUTE | VmFlags::USER | VmFlags::MAP,
        ));
        let mapping = VmarMapping {
            range,
            flags: mapping_flags,
            purpose: RegionPurpose::User,
            vmo: Arc::clone(&vmo),
            vmo_offset: 0,
        };

        image_vmar
            .map_vmo(
                range,
                Arc::clone(&vmo),
                0,
                mapping_flags,
                RegionPurpose::User,
            )
            .map_err(|_| BootstrapLoadError::InvalidExecutable("segment mapping failed"))?;
        if let Err(error) = self.map_mapping_in_page_table(&mapping) {
            let _ = image_vmar.unmap(range.start());
            return Err(BootstrapLoadError::Object(error));
        }
        if !vmo.write(0, &bytes) {
            let _ = self.unmap_entry_from_page_table(&VmarEntry::Mapping(mapping.clone()));
            let _ = image_vmar.unmap(range.start());
            return Err(BootstrapLoadError::InvalidExecutable(
                "failed to populate bootstrap segment contents",
            ));
        }

        Ok(())
    }

    fn mapping_flags(protections: MachOSegmentProtections) -> VmFlags {
        let mut flags = VmFlags::USER;
        if protections.read {
            flags |= VmFlags::READ;
        }
        if protections.write {
            flags |= VmFlags::WRITE;
        }
        if protections.execute {
            flags |= VmFlags::EXECUTE;
        }
        flags
    }

    pub(crate) fn build_bootstrap_stack(
        &self,
        user_stack: &UserStackAllocation,
        bootstrap_abi: &BootstrapUserAbi,
    ) -> Result<usize, BootstrapLoadError> {
        let stack_range = user_stack.mapped_range();
        let stack_base = stack_range.start().as_usize();
        let stack_size = stack_range.len();
        let mut bytes = vec![0u8; stack_size];
        let mut cursor = stack_size;
        let program_name = b"bootstrap\0";
        cursor =
            cursor
                .checked_sub(program_name.len())
                .ok_or(BootstrapLoadError::InvalidExecutable(
                    "bootstrap user stack is unexpectedly small",
                ))?;
        bytes[cursor..cursor + program_name.len()].copy_from_slice(program_name);
        let argv0 = stack_base
            .checked_add(cursor)
            .ok_or(BootstrapLoadError::InvalidExecutable(
                "bootstrap argv pointer overflowed",
            ))?;

        let words = [
            1usize,
            argv0,
            0,
            0,
            AT_PAGESZ,
            bootstrap_abi.page_size,
            AT_ENTRY,
            bootstrap_abi.entry_point,
            AT_AK_PROCESS_SELF,
            bootstrap_abi.process_self_slot as usize,
            AT_AK_ROOT_NS,
            bootstrap_abi.root_ns_slot as usize,
            AT_AK_ROOT_VMAR,
            bootstrap_abi.root_vmar_slot as usize,
            AT_AK_SYSCALL_TABLE,
            bootstrap_abi.syscall_table_slot as usize,
            AT_AK_PAGE_SIZE,
            bootstrap_abi.page_size,
            AT_AK_HEAP_VMAR,
            bootstrap_abi.heap_vmar_slot as usize,
            AT_AK_MAPPED_VMAR,
            bootstrap_abi.mapped_vmar_slot as usize,
            AT_AK_HEAP_BASE,
            bootstrap_abi.heap_base,
            AT_AK_HEAP_LIMIT,
            bootstrap_abi.heap_limit,
            AT_NULL,
            0,
        ];
        let words_bytes = words
            .len()
            .checked_mul(core::mem::size_of::<usize>())
            .ok_or(BootstrapLoadError::InvalidExecutable(
                "bootstrap argument vector overflowed",
            ))?;
        cursor = cursor
            .checked_sub(words_bytes)
            .ok_or(BootstrapLoadError::InvalidExecutable(
                "bootstrap argument vector does not fit on the initial stack",
            ))?;
        cursor &= !0xF;
        if cursor + words_bytes > bytes.len() {
            return Err(BootstrapLoadError::InvalidExecutable(
                "bootstrap stack alignment exceeded the mapped stack range",
            ));
        }

        let mut word_offset = cursor;
        for word in words {
            let next_word = word_offset
                .checked_add(core::mem::size_of::<usize>())
                .ok_or(BootstrapLoadError::InvalidExecutable(
                    "bootstrap stack cursor overflowed",
                ))?;
            bytes[word_offset..next_word].copy_from_slice(&word.to_le_bytes());
            word_offset = next_word;
        }

        if !user_stack.vmo().write(0, &bytes) {
            return Err(BootstrapLoadError::InvalidExecutable(
                "failed to populate bootstrap initial stack image",
            ));
        }
        stack_base
            .checked_add(cursor)
            .ok_or(BootstrapLoadError::InvalidExecutable(
                "bootstrap stack pointer overflowed",
            ))
    }

    pub(crate) fn install_bootstrap_handles(
        &self,
        entry_point: usize,
    ) -> Result<BootstrapUserAbi, BootstrapLoadError> {
        let runtime = RuntimeServices::global();
        let heap_range = self.segment_vmar(VmLayoutSegment::UserHeap)?.range();
        let mut process_self_handle = self.task_process_handle()?;
        process_self_handle.downgrade(
            Capability::WRITE | Capability::EXECUTE,
            u32::MAX & !PROCESS_QUERY,
        );
        let root_ns_handle = runtime
            .namespaces()
            .user_super()
            .derive_handle(Capability::READ, 0)?;
        let root_vmar_handle = self.derive_root_vmar_handle()?;
        let heap_vmar_handle = self.derive_segment_vmar_handle(VmLayoutSegment::UserHeap)?;
        let mapped_vmar_handle = self.derive_segment_vmar_handle(VmLayoutSegment::UserMapped)?;
        let syscall_table_handle = crate::syscall::request_handle(Capability::READ, 0)?;

        Ok(BootstrapUserAbi {
            entry_point,
            process_self_slot: self.install_handle_auto(process_self_handle),
            root_ns_slot: self.install_handle_auto(root_ns_handle),
            root_vmar_slot: self.install_handle_auto(root_vmar_handle),
            heap_vmar_slot: self.install_handle_auto(heap_vmar_handle),
            mapped_vmar_slot: self.install_handle_auto(mapped_vmar_handle),
            syscall_table_slot: self.install_handle_auto(syscall_table_handle),
            page_size: PAGE_SIZE,
            heap_base: heap_range.start().as_usize(),
            heap_limit: heap_range.end().as_usize(),
        })
    }
}
