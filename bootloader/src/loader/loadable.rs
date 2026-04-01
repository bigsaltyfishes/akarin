use alloc::{
    collections::binary_heap::BinaryHeap,
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::{arch::asm, cmp::Reverse, mem};

use aligned_vec::{AVec, ConstAlign};
use goblin::mach::{
    constants::{VM_PROT_EXECUTE, VM_PROT_READ, VM_PROT_WRITE},
    load_command::CommandVariant,
    segment::Segment,
};
use libakarin_core::memory::{RegionPurpose, VmFlags, VmRange, Vmar, VmarEntry, Vmo};
use libakarin_dyld::{
    DyldLinker, DyldRebaseTarget, ImageParserError, LinkerError, MachOImage, Nlist,
};
use libakarin_machine_core::memory::{
    AddressSpaceTrait, PhysAddr as MachinePhysAddr, UaccessError, VirtAddr as MachineVirtAddr,
    paging::{
        CachePolicy, MMUFlags, PageSizeTrait, PageTableEntryTrait, PageTableTrait, PageTrait,
        PagingError, PagingResult, TlbInvalidator,
    },
};
use log::{debug, error, info};
use thiserror::Error;
use uefi::boot::PAGE_SIZE;
use x86_64::{
    PhysAddr, VirtAddr,
    structures::paging::{
        FrameAllocator, Mapper, OffsetPageTable, Page, PageSize, PageTable, PageTableFlags,
        PhysFrame, Size1GiB, Size2MiB, Size4KiB, Translate,
    },
};

use crate::{
    loader::KERNEL_SPACE_START,
    memory::{MemoryMapBuilder, OrderedArena, memory_map},
    misc::align_up,
    protocol::{
        BootInfo, KernelImageInfo, KernelSymtab, Requirements,
        memory::{Arena, ArenaKind, MemoryMap},
    },
    resources::{UefiResource, acpi::Acpi, framebuffer::Framebuffer, gdt::Gdt},
};

const PAGE_BYTES: usize = 0x1000;
const KERNEL_STACK_SEGMENT_START: usize = 0xFFFF_FF00_0000_0000;
const KERNEL_STACK_SEGMENT_END: usize = 0xFFFF_FF80_0000_0000;

#[derive(Debug, Clone, Copy)]
struct BootPageSize;

impl PageSizeTrait for BootPageSize {
    const UNIT_PAGE_SIZE: usize = PAGE_SIZE;

    fn validate(size: usize) -> bool {
        size == PAGE_SIZE
    }

    fn size(&self) -> usize {
        PAGE_SIZE
    }
}

#[derive(Debug, Clone, Copy)]
struct BootPage {
    virt: MachineVirtAddr,
}

impl PageTrait<BootPageSize> for BootPage {
    fn containing(virt_addr: MachineVirtAddr, _size: BootPageSize) -> Self {
        Self { virt: virt_addr }
    }

    fn size(&self) -> BootPageSize {
        BootPageSize
    }

    fn virt_addr(&self) -> MachineVirtAddr {
        self.virt
    }
}

#[derive(Debug, Clone, Copy)]
struct BootPageTableEntry;

impl PageTableEntryTrait for BootPageTableEntry {
    fn phys_addr(&self) -> MachinePhysAddr {
        MachinePhysAddr::new(0)
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

    fn set_phys_addr(&mut self, _phys_addr: MachinePhysAddr, _flags: MMUFlags) {}

    fn clear(&mut self) {}
}

#[derive(Debug, Clone, Copy)]
struct BootPageTable;

impl PageTableTrait<BootAddressSpace, BootPageSize, BootPage, BootPageTableEntry>
    for BootPageTable
{
    fn empty(_allocator: &'static dyn libakarin_machine_core::memory::FrameAllocatorTrait) -> Self {
        Self
    }

    fn from_raw(
        _allocator: &'static dyn libakarin_machine_core::memory::FrameAllocatorTrait,
        _root_ptr: *mut u8,
    ) -> Self {
        Self
    }

    fn phys_addr(&self) -> MachinePhysAddr {
        MachinePhysAddr::new(0)
    }

    unsafe fn map(
        &mut self,
        _page: BootPage,
        _frame: <BootAddressSpace as AddressSpaceTrait>::PhysFrame,
        _flags: MMUFlags,
        _cache: CachePolicy,
    ) -> PagingResult<TlbInvalidator<BootAddressSpace>> {
        Err(PagingError::UnsupportedPageSize)
    }

    unsafe fn unmap(
        &mut self,
        _page: BootPage,
    ) -> PagingResult<(
        <BootAddressSpace as AddressSpaceTrait>::PhysFrame,
        TlbInvalidator<BootAddressSpace>,
    )> {
        Err(PagingError::NotMapped)
    }

    fn entry(&self, _page: BootPage) -> PagingResult<(&BootPageTableEntry, BootPageSize)> {
        Err(PagingError::NotMapped)
    }

    fn entry_mut(
        &mut self,
        _page: BootPage,
    ) -> PagingResult<(&mut BootPageTableEntry, BootPageSize)> {
        Err(PagingError::NotMapped)
    }
}

#[derive(Debug, Clone, Copy)]
struct BootAddressSpace;

impl AddressSpaceTrait for BootAddressSpace {
    type Page = BootPage;
    type PageSize = BootPageSize;
    type PageTable = BootPageTable;
    type PageTableEntry = BootPageTableEntry;

    fn virt_to_phys(virt_addr: MachineVirtAddr) -> Option<MachinePhysAddr> {
        Some(MachinePhysAddr::new(virt_addr.as_usize()))
    }

    fn phys_to_virt(phys_addr: MachinePhysAddr) -> Option<MachineVirtAddr> {
        Some(MachineVirtAddr::new(phys_addr.as_usize()))
    }

    unsafe fn read_phys(phys_addr: MachinePhysAddr, buffer: &mut [u8]) {
        unsafe {
            core::ptr::copy_nonoverlapping(
                phys_addr.as_usize() as *const u8,
                buffer.as_mut_ptr(),
                buffer.len(),
            );
        }
    }

    unsafe fn write_phys(phys_addr: MachinePhysAddr, data: &[u8]) {
        unsafe {
            core::ptr::copy_nonoverlapping(
                data.as_ptr(),
                phys_addr.as_usize() as *mut u8,
                data.len(),
            );
        }
    }

    unsafe fn zero_phys(phys_addr: MachinePhysAddr, size: usize) {
        unsafe {
            core::ptr::write_bytes(phys_addr.as_usize() as *mut u8, 0, size);
        }
    }

    unsafe fn copy_phys(
        src_phys_addr: MachinePhysAddr,
        dest_phys_addr: MachinePhysAddr,
        size: usize,
    ) {
        unsafe {
            core::ptr::copy_nonoverlapping(
                src_phys_addr.as_usize() as *const u8,
                dest_phys_addr.as_usize() as *mut u8,
                size,
            );
        }
    }

    fn current_base() -> MachinePhysAddr {
        let cr3: usize;
        unsafe {
            asm!("mov {}, cr3", out(reg) cr3);
        }
        MachinePhysAddr::new(cr3)
    }

    unsafe fn switch_base(new_base: MachinePhysAddr) {
        unsafe {
            asm!("mov cr3, {}", in(reg) new_base.as_usize());
        }
    }

    fn invalidate_tlb(virt_addr: MachineVirtAddr) {
        unsafe {
            asm!("invlpg [{}]", in(reg) virt_addr.as_usize(), options(nostack, preserves_flags));
        }
    }

    fn flush_tlb() {
        let cr3: usize;
        unsafe {
            asm!("mov {}, cr3", out(reg) cr3);
            asm!("mov cr3, {}", in(reg) cr3);
        }
    }

    fn map_kernel_space(_page_table: &mut Self::PageTable) {}

    fn copy_from_user(_src: MachineVirtAddr, _buffer: &mut [u8]) -> Result<(), UaccessError> {
        Err(UaccessError::Fault)
    }

    fn copy_to_user(_dst: MachineVirtAddr, _data: &[u8]) -> Result<(), UaccessError> {
        Err(UaccessError::Fault)
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
        let mapping = self
            .vmar
            .mapping_at(MachineVirtAddr::new(vm_addr as usize))
            .ok_or("rebase address is not mapped in kernel VMAR")?;
        let offset = mapping
            .vmo_offset_for_addr(MachineVirtAddr::new(vm_addr as usize))
            .ok_or("failed to translate VMAR mapping offset")?;
        mapping
            .vmo
            .read(offset, buffer)
            .then_some(())
            .ok_or("failed to read rebase source bytes")
    }

    fn write_at_vmaddr(&self, vm_addr: u64, data: &[u8]) -> Result<(), &'static str> {
        let mapping = self
            .vmar
            .mapping_at(MachineVirtAddr::new(vm_addr as usize))
            .ok_or("rebase address is not mapped in kernel VMAR")?;
        let offset = mapping
            .vmo_offset_for_addr(MachineVirtAddr::new(vm_addr as usize))
            .ok_or("failed to translate VMAR mapping offset")?;
        mapping
            .vmo
            .write(offset, data)
            .then_some(())
            .ok_or("failed to write rebase bytes")
    }
}

pub struct PageFrameAllocator<'a> {
    builder: &'a mut MemoryMapBuilder,
}

impl<'a> PageFrameAllocator<'a> {
    pub fn new(builder: &'a mut MemoryMapBuilder) -> Self {
        Self { builder }
    }
}

unsafe impl FrameAllocator<Size4KiB> for PageFrameAllocator<'_> {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        let frame_addr = unsafe {
            self.builder
                .allocate_and_mark(1, ArenaKind::KernelPageTable)
        };

        error!("Allocating PAGE frame at phys addr {:#x}", frame_addr);

        Some(PhysFrame::containing_address(PhysAddr::new(
            frame_addr as u64,
        )))
    }
}

#[derive(Debug, Error)]
pub enum LoadableError {
    #[error("Kernel image parsing error: {0}")]
    ImageParseError(#[from] ImageParserError),
    #[error("Image not loaded!")]
    ImageNotLoaded,
    #[error("Failed to link the kernel image: {0}")]
    LinkerError(#[from] LinkerError),
    #[error("Interface Version Mismatch: expected {0}, found {1}")]
    InterfaceVersionMismatch(u32, u32),
    #[error("Interface Magic Mismatch: expected {0}, found {1}")]
    InterfaceMagicMismatch(u64, u64),
    #[error("Missing Interface Segment or Section, is this a valid or a compatible kernel?")]
    MissingInterfaceSection,
    #[error("Out of system resources")]
    OutOfSystemResource,
}

#[derive(Debug, Clone, Copy)]
pub struct ByteRef {
    pub offset: usize,
    pub size: usize,
}

impl ByteRef {
    /// Get a slice from the given data based on the ByteRef
    pub fn slice<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        &data[self.offset..self.offset + self.size]
    }
}

#[derive(Debug, Clone)]
struct LoadParam {
    seg_idx: u8,
    base_addr: usize,
    size: usize,
    protection: PageTableFlags,
    sections: Option<Vec<(String, usize, usize)>>, // (name, offset, size)
    data_ref: Option<ByteRef>,                     // (file offset in kernel data, filesize)
}

impl Eq for LoadParam {}

impl PartialEq for LoadParam {
    fn eq(&self, other: &Self) -> bool {
        self.base_addr == other.base_addr
    }
}

impl PartialOrd for LoadParam {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.base_addr.cmp(&other.base_addr))
    }
}

impl Ord for LoadParam {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.base_addr.cmp(&other.base_addr)
    }
}

struct Addresses {
    entry: usize,
    stack_top: usize,
    page_table: &'static mut PageTable,
    info_ptr: usize,
}

/// Represents a loadable kernel image with segments to be loaded into memory
/// and mapped into the kernel's address space.
///
/// # Fields
/// - `image`: The `KernelImage` representing the parsed kernel binary.
/// - `entry_point`: The entry point address of the kernel after loading.
/// - `map_builder`: The `MemoryMapBuilder` used to allocate physical memory for
///   the kernel.
/// - `load_params`: A binary heap of `LoadParam` structs representing segments
///   to load.
/// - `requirements`: The `Requirements` specified by the kernel for loading.
/// - `stack_top`: The top address of the kernel stack, the address is in the
///   kernel's virtual space.
/// - `reserve_pages`: Optional reserved memory pages for the kernel.
/// - `symtab`: Optional symbol and string tables required by the kernel.
/// - `loaded_sections`: A vector of tuples containing the names and memory
///   arenas of loaded sections.
/// - `page_table`: The page table used for the kernel's address space.
#[derive(Debug)]
pub struct Loadable {
    image: MachOImage,
    entry_point: Option<usize>,
    slide: Option<usize>,
    map_builder: Option<MemoryMapBuilder>,
    load_params: BinaryHeap<Reverse<LoadParam>>,
    requirements: Requirements,
    stack_top: Option<usize>,
    symtab: Option<KernelSymtab>,
    loaded_sections: Option<Vec<(String, Arena)>>,
    page_table: Option<&'static mut PageTable>,
}

impl Loadable {
    pub fn new(image: MachOImage) -> Result<Self, LoadableError> {
        let bin = image.binary()?;
        let mut map_builder = MemoryMapBuilder::new();

        let req = bin
            .segments
            .iter()
            .find(|seg| seg.name().ok() == Some("__REQ"))
            .map(|seg| {
                if let Ok(sections) = seg.sections() {
                    for sect in sections {
                        if sect.0.name().ok() == Some("__requirements") {
                            let offset = sect.0.offset as usize;
                            let size = sect.0.size as usize;
                            if size < core::mem::size_of::<Requirements>() {
                                return Err(LoadableError::InterfaceVersionMismatch(
                                    crate::protocol::INTERFACE_VERSION,
                                    0,
                                ));
                            }

                            let data =
                                &image[offset..offset + core::mem::size_of::<Requirements>()];
                            let req: Requirements = unsafe {
                                core::ptr::read_unaligned(data.as_ptr() as *const Requirements)
                            };

                            if req.magic != crate::protocol::INTERFACE_MAGIC {
                                return Err(LoadableError::InterfaceMagicMismatch(
                                    crate::protocol::INTERFACE_MAGIC,
                                    req.magic,
                                ));
                            }

                            if req.version != crate::protocol::INTERFACE_VERSION {
                                return Err(LoadableError::InterfaceVersionMismatch(
                                    crate::protocol::INTERFACE_VERSION,
                                    req.version,
                                ));
                            }

                            return Ok(req);
                        }
                    }
                }
                Err(LoadableError::MissingInterfaceSection)
            })
            .ok_or(LoadableError::MissingInterfaceSection)??;

        let mut symtab_and_strtab = None;
        let mut load_params = BinaryHeap::new();
        bin.load_commands.iter().enumerate().for_each(|(id, cmd)| {
            match cmd.command {
                CommandVariant::Segment64(seg) => {
                    let name = seg.name().unwrap_or("");
                    if name == "__PAGEZERO" || name == "__LINKEDIT" || name == "__REQ" {
                        return;
                    }

                    let mut protection = PageTableFlags::PRESENT;
                    if seg.initprot & VM_PROT_WRITE != 0 {
                        protection |= PageTableFlags::WRITABLE;
                    }
                    if seg.initprot & VM_PROT_EXECUTE == 0 {
                        protection |= PageTableFlags::NO_EXECUTE;
                    }

                    // seg.vmaddr / seg.vmsize represent the virtual mapping range
                    let seg_vmaddr = seg.vmaddr as usize;
                    let seg_vmsize = seg.vmsize as usize;
                    let seg_filesize = seg.filesize as usize;
                    let seg_fileoff = seg.fileoff as usize;

                    let seg_sections = Segment::from_64(&image, &seg, cmd.offset, image.ctx())
                        .map(|seg| {
                            if let Ok(sects) = seg.sections() {
                                let mut seg_sections = Vec::new();
                                for sect in sects {
                                    let sect_name = sect.0.name().unwrap_or("");
                                    debug!(
                                        "Found section '{}' in segment '{}', offset: '{:#x}', \
                                         fileoff: '{:#x}'",
                                        sect_name, name, sect.0.offset, seg.fileoff
                                    );
                                    seg_sections.push((
                                        sect_name.to_string(),
                                        (sect.0.offset as usize)
                                            .checked_sub(seg.fileoff as usize)
                                            .unwrap_or(sect.0.offset as usize),
                                        sect.0.size as usize,
                                    ));
                                }

                                Some(seg_sections)
                            } else {
                                None
                            }
                        })
                        .unwrap_or(None);

                    // If filesize==0, we still map vmsize (BSS); data_ref None
                    let data_ref = if seg_filesize > 0 {
                        Some(ByteRef {
                            offset: seg_fileoff,
                            size: seg_filesize,
                        })
                    } else {
                        None
                    };

                    info!(
                        "Prepared load param for segment '{}' at addr {:#x} vmsize {:#x} filesize \
                         {:#x}",
                        name, seg_vmaddr, seg_vmsize, seg_filesize
                    );

                    load_params.push(Reverse(LoadParam {
                        seg_idx: id as u8,
                        base_addr: seg_vmaddr,
                        size: seg_vmsize,
                        sections: seg_sections,
                        protection,
                        data_ref,
                    }));
                }
                CommandVariant::Symtab(tab) => {
                    if req.symtab_required {
                        info!(
                            "Kernel requires symtab at offset {:#x} with {} symbols",
                            tab.symoff, tab.nsyms
                        );

                        let mut symtab = AVec::<Nlist, ConstAlign<0x1000>>::new(0x1000);
                        let symtab_slice = &image[tab.symoff as usize
                            ..(tab.symoff as usize + (tab.nsyms as usize * size_of::<Nlist>()))
                                as usize];
                        for i in 0..tab.nsyms as usize {
                            let entry_offset = i * size_of::<Nlist>();
                            let entry_data =
                                &symtab_slice[entry_offset..entry_offset + size_of::<Nlist>()];
                            let nlist: Nlist = unsafe {
                                core::ptr::read_unaligned(entry_data.as_ptr() as *const Nlist)
                            };
                            symtab.push(nlist);
                        }

                        let mut strtab = AVec::<u8, ConstAlign<0x1000>>::new(0x1000);
                        strtab.extend_from_slice(
                            &image
                                [tab.stroff as usize..(tab.stroff as usize + tab.strsize as usize)],
                        );
                        symtab.sort_by(|a, b| a.n_value.cmp(&b.n_value));

                        symtab_and_strtab = Some(KernelSymtab {
                            sym_addr: symtab.as_ptr() as usize + KERNEL_SPACE_START,
                            num_syms: symtab.len(),
                            str_addr: strtab.as_ptr() as usize + KERNEL_SPACE_START,
                            str_size: strtab.len(),
                        });

                        map_builder.add_arena(Arena {
                            start: symtab.as_ptr() as usize,
                            end: align_up(
                                PAGE_SIZE,
                                symtab.as_ptr() as usize
                                    + symtab.len() * core::mem::size_of::<Nlist>(),
                            ),
                            kind: ArenaKind::KernelSymbolTable,
                        });
                        map_builder.add_arena(Arena {
                            start: strtab.as_ptr() as usize,
                            end: align_up(PAGE_SIZE, strtab.as_ptr() as usize + strtab.len()),
                            kind: ArenaKind::KernelSymbolTable,
                        });

                        // Map Builder owns the AVec now,
                        // it will deallocate it when the memory map is destroyed.
                        mem::forget(symtab);
                        mem::forget(strtab);
                    }
                }
                _ => {}
            }
        });

        Ok(Self {
            image,
            entry_point: None,
            slide: None,
            map_builder: Some(map_builder),
            load_params,
            stack_top: None,
            requirements: req,
            symtab: symtab_and_strtab,
            loaded_sections: None,
            page_table: None,
        })
    }

    /// Load the loadable into memory at the given base address
    ///
    /// # Arguments
    ///
    /// * `base` - The base virtual address to load the kernel segments into
    /// * `stack_size` - The size of the kernel stack to allocate, in pages.
    pub fn load(&mut self) -> Result<(), LoadableError> {
        let map_builder = self
            .map_builder
            .as_mut()
            .ok_or(LoadableError::ImageNotLoaded)?;
        let kernel_page_table: &'static mut PageTable = {
            let frame = unsafe { map_builder.allocate_and_mark(1, ArenaKind::KernelPageTable) };

            let table_ptr = frame as *mut PageTable;
            unsafe {
                table_ptr.write(PageTable::new());
                &mut *table_ptr
            }
        };

        let mut offset_table = unsafe { OffsetPageTable::new(kernel_page_table, VirtAddr::new(0)) };

        // compute min_load_addr from segment base addresses (page-aligned)
        let min_load_addr = self
            .load_params
            .peek()
            .map(|l| l.0.base_addr & !(0x1000 - 1))
            .unwrap_or(0);
        let slide = self
            .requirements
            .load_base
            .checked_sub(min_load_addr)
            .ok_or(LoadableError::ImageNotLoaded)?;

        let image_span_end = self
            .load_params
            .iter()
            .map(|param| {
                let load_param = &param.0;
                let map_start = load_param.base_addr & !(0x1000 - 1);
                let map_end = (load_param.base_addr + load_param.size + 0x1000 - 1) & !(0x1000 - 1);
                self.requirements.load_base - min_load_addr + map_end.max(map_start)
            })
            .max()
            .ok_or(LoadableError::ImageNotLoaded)?;
        let image_span_start = self.requirements.load_base;
        let image_vmar_range = VmRange::new(
            MachineVirtAddr::new(image_span_start),
            MachineVirtAddr::new(image_span_end),
        )
        .ok_or(LoadableError::ImageNotLoaded)?;
        let image_vmar = Arc::new(Vmar::new(image_vmar_range));

        // Iterate segment-based load params and map each whole segment (vmsize-aligned)
        let mut loaded_sections = Vec::new();
        while let Some(param) = self.load_params.pop() {
            let load_param = param.0;

            let seg_base = load_param.base_addr;
            let seg_vmsize = load_param.size;
            // page-align the mapping region
            let map_start = seg_base & !(0x1000 - 1);
            let map_end = (seg_base + seg_vmsize + 0x1000 - 1) & !(0x1000 - 1);
            let page_cnt = (map_end - map_start) / 0x1000;

            // destination virtual start in higher-half
            let dest_vstart = self.requirements.load_base - min_load_addr + map_start;

            // allocate contiguous physical pages for this whole segment mapping
            let phys_base =
                unsafe { map_builder.allocate_and_mark(page_cnt, ArenaKind::ExecutableAndModules) };

            // copy file-backed data if any
            if let Some(byte_ref) = load_param.data_ref {
                // copy the bytes from the kernel image into the physical allocation with offset
                let copy_offset = seg_base - map_start; // offset inside the mapped physical range
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        byte_ref.slice(&self.image).as_ptr(),
                        (phys_base + (seg_base - map_start)) as *mut u8,
                        byte_ref.size,
                    );
                }

                // zero the remainder between filesize and vmsize
                let zero_start = phys_base + copy_offset + byte_ref.size;
                let zero_len = seg_vmsize.saturating_sub(byte_ref.size);
                if zero_len > 0 {
                    unsafe {
                        core::ptr::write_bytes(zero_start as *mut u8, 0, zero_len);
                    }
                }
            } else {
                // pure BSS: zero entire vmsize range
                unsafe {
                    core::ptr::write_bytes(
                        (phys_base + (seg_base - map_start)) as *mut u8,
                        0,
                        seg_vmsize,
                    );
                }
            }

            let mapped_len = map_end - map_start;
            let vmo_flags = VmFlags::READ
                | VmFlags::WRITE
                | VmFlags::EXECUTE
                | VmFlags::MAP
                | VmFlags::PHYSICAL;
            let vmo = Arc::new(Vmo::new_physical::<BootAddressSpace>(
                format!("kernel-seg-{}", load_param.seg_idx),
                mapped_len,
                PAGE_SIZE,
                vmo_flags,
                MachinePhysAddr::new(phys_base),
            ));
            let vm_range = VmRange::new(
                MachineVirtAddr::new(dest_vstart),
                MachineVirtAddr::new(dest_vstart + mapped_len),
            )
            .ok_or(LoadableError::ImageNotLoaded)?;
            image_vmar
                .map_vmo(
                    vm_range,
                    Arc::clone(&vmo),
                    0,
                    Self::vm_flags_from_page_table(load_param.protection),
                    Self::region_purpose_from_page_table(load_param.protection),
                )
                .map_err(|_| LoadableError::ImageNotLoaded)?;

            loaded_sections.extend(load_param.sections.unwrap_or_default().into_iter().map(
                |(name, offset, size)| {
                    (
                        name,
                        Arena {
                            start: phys_base + (seg_base - map_start) + offset,
                            end: phys_base + (seg_base - map_start) + offset + size,
                            kind: ArenaKind::ExecutableAndModules,
                        },
                    )
                },
            ));

            info!(
                "Prepared segment at vaddr {:#x} (phys {:#x}) pages {}",
                dest_vstart, phys_base, page_cnt
            );
        }

        DyldLinker::new(&mut self.image, slide)
            .link(&VmarRebaseTarget::new(Arc::clone(&image_vmar)))?;
        Self::map_image_vmar(&mut offset_table, map_builder, &image_vmar)?;

        // set entry point, stack, page table
        self.entry_point =
            Some(self.image.binary()?.entry as usize - min_load_addr + self.requirements.load_base);
        self.slide = Some(slide);
        self.stack_top = Some(unsafe {
            map_builder.allocate_and_mark(self.requirements.stack_size, ArenaKind::KernelStack)
                + KERNEL_SPACE_START
                + self.requirements.stack_size * 0x1000
        });
        self.page_table = Some(kernel_page_table);
        self.loaded_sections = Some(loaded_sections);

        // map context switch function
        unsafe {
            self.map_function(Self::context_switch as *const ())?;
        };

        // map gdt
        self.map_gdt()?;

        // map higher-half kernel space
        self.map_phys()?;

        Ok(())
    }

    fn vm_flags_from_page_table(flags: PageTableFlags) -> VmFlags {
        let mut vm_flags = VmFlags::READ;
        if flags.contains(PageTableFlags::WRITABLE) {
            vm_flags |= VmFlags::WRITE;
        }
        if !flags.contains(PageTableFlags::NO_EXECUTE) {
            vm_flags |= VmFlags::EXECUTE;
        }
        vm_flags
    }

    fn region_purpose_from_page_table(flags: PageTableFlags) -> RegionPurpose {
        if !flags.contains(PageTableFlags::NO_EXECUTE) {
            RegionPurpose::KernelText
        } else {
            RegionPurpose::KernelData
        }
    }

    fn map_image_vmar(
        offset_table: &mut OffsetPageTable<'_>,
        map_builder: &mut MemoryMapBuilder,
        image_vmar: &Arc<Vmar>,
    ) -> Result<(), LoadableError> {
        for entry in image_vmar.entries() {
            let VmarEntry::Mapping(mapping) = entry else {
                continue;
            };

            let page_count = mapping.range.len() / PAGE_SIZE;
            for page_idx in 0..page_count {
                let virt_addr = mapping.range.start().as_usize() + page_idx * PAGE_SIZE;
                let vmo_offset = mapping
                    .vmo_offset_for_addr(MachineVirtAddr::new(virt_addr))
                    .ok_or(LoadableError::ImageNotLoaded)?;
                let meta = mapping
                    .vmo
                    .page_at(vmo_offset)
                    .ok_or(LoadableError::ImageNotLoaded)?;

                unsafe {
                    let _ = offset_table
                        .map_to_with_table_flags(
                            Page::<Size4KiB>::containing_address(VirtAddr::new(virt_addr as u64)),
                            PhysFrame::containing_address(PhysAddr::new(
                                meta.phys.as_usize() as u64
                            )),
                            Self::page_table_flags_from_vm_flags(mapping.flags),
                            PageTableFlags::PRESENT | PageTableFlags::WRITABLE,
                            &mut PageFrameAllocator::new(map_builder),
                        )
                        .expect("Failed to map kernel segment from VMAR");
                }
            }
        }

        Ok(())
    }

    fn page_table_flags_from_vm_flags(flags: VmFlags) -> PageTableFlags {
        let mut table_flags = PageTableFlags::PRESENT;
        if flags.contains(VmFlags::WRITE) {
            table_flags |= PageTableFlags::WRITABLE;
        }
        if !flags.contains(VmFlags::EXECUTE) {
            table_flags |= PageTableFlags::NO_EXECUTE;
        }
        table_flags
    }

    pub unsafe fn map_function(&mut self, f: *const ()) -> Result<(), LoadableError> {
        let mut map_builder = self
            .map_builder
            .take()
            .ok_or(LoadableError::ImageNotLoaded)?;
        let mut offset_table = self.offset_table()?;
        let func_addr = PhysAddr::new(f as u64);
        let func_start_frame: PhysFrame<Size4KiB> = PhysFrame::containing_address(func_addr);
        for frame in PhysFrame::range_inclusive(func_start_frame, func_start_frame + 1) {
            let page = Page::containing_address(VirtAddr::new(frame.start_address().as_u64()));
            match unsafe {
                // The parent table flags need to be both readable and writable to
                // support recursive page tables.
                // See https://github.com/rust-osdev/bootloader/issues/443#issuecomment-2130010621
                offset_table.map_to_with_table_flags(
                    page,
                    frame,
                    PageTableFlags::PRESENT,
                    PageTableFlags::PRESENT | PageTableFlags::WRITABLE,
                    &mut PageFrameAllocator::new(&mut map_builder),
                )
            } {
                Ok(tlb) => tlb.flush(),
                Err(err) => panic!("failed to identity map frame {:?}: {:?}", frame, err),
            }
        }

        let ret = offset_table.translate(VirtAddr::new(f as u64));
        info!(
            "Mapped function at virt addr {:p} -> phys addr {:?}",
            f, ret
        );

        self.map_builder = Some(map_builder);

        Ok(())
    }

    /// Map all physical memory described in the given memory map
    ///
    /// # Arguments
    ///
    /// * `memmap` - The UEFI memory map describing physical memory regions,
    ///   memmap must be sorted and non-overlapping.
    pub fn map_phys(&mut self) -> Result<(), LoadableError> {
        let mut map_builder = self
            .map_builder
            .take()
            .ok_or(LoadableError::ImageNotLoaded)?;
        let mut offset_table = self.offset_table()?;

        let mut min_phys = PhysAddr::new(0);
        let max_phys = PhysAddr::new(
            memory_map()
                .last()
                .map_or(0, |OrderedArena(a)| a.end as u64),
        );
        let vaddr_start = VirtAddr::new(KERNEL_SPACE_START as u64);
        while min_phys < max_phys {
            let phys_addr = min_phys;
            let virt_addr = vaddr_start + phys_addr.as_u64();

            if max_phys - min_phys >= Size1GiB::SIZE {
                // Map 1 GiB pages where possible
                unsafe {
                    let _ = offset_table
                        .map_to(
                            Page::<Size1GiB>::containing_address(virt_addr),
                            PhysFrame::containing_address(phys_addr),
                            PageTableFlags::PRESENT | PageTableFlags::WRITABLE,
                            &mut PageFrameAllocator::new(&mut map_builder),
                        )
                        .expect("Failed to map physical memory");
                }

                min_phys += Size1GiB::SIZE;
            } else if max_phys - min_phys >= Size2MiB::SIZE {
                // Map 2 MiB pages where possible
                unsafe {
                    let _ = offset_table
                        .map_to(
                            Page::<Size2MiB>::containing_address(virt_addr),
                            PhysFrame::containing_address(phys_addr),
                            PageTableFlags::PRESENT | PageTableFlags::WRITABLE,
                            &mut PageFrameAllocator::new(&mut map_builder),
                        )
                        .expect("Failed to map physical memory");
                }

                min_phys += Size2MiB::SIZE;
            } else {
                // Map 4 KiB pages for the rest
                unsafe {
                    let _ = offset_table
                        .map_to(
                            Page::<Size4KiB>::containing_address(virt_addr),
                            PhysFrame::containing_address(phys_addr),
                            PageTableFlags::PRESENT | PageTableFlags::WRITABLE,
                            &mut PageFrameAllocator::new(&mut map_builder),
                        )
                        .expect("Failed to map physical memory");
                }

                min_phys += Size4KiB::SIZE;
            }
        }

        self.map_builder = Some(map_builder);

        Ok(())
    }

    pub fn map_gdt(&mut self) -> Result<(), LoadableError> {
        let gdt = Gdt::resource().ok_or(LoadableError::OutOfSystemResource)?;
        let gdt_arena = gdt.as_arena().clone();
        let mut map_builder = self
            .map_builder
            .take()
            .ok_or(LoadableError::ImageNotLoaded)?;
        let mut page_table = self.offset_table()?;

        // Map GDT pages to same virtual address
        let gdt_end = gdt_arena.end;
        let mut curr_addr = gdt_arena.start;
        while curr_addr < gdt_end {
            let phys_addr = PhysAddr::new(curr_addr as u64);
            let virt_addr = VirtAddr::new(curr_addr as u64);

            let _ = unsafe {
                page_table
                    .map_to(
                        Page::<Size4KiB>::containing_address(virt_addr),
                        PhysFrame::containing_address(phys_addr),
                        PageTableFlags::PRESENT | PageTableFlags::WRITABLE,
                        &mut PageFrameAllocator::new(&mut map_builder),
                    )
                    .expect("Failed to map GDT")
            };
            curr_addr += PAGE_SIZE;
        }

        let addr = page_table.translate(VirtAddr::new(gdt_arena.start as u64));
        info!(
            "Mapped GDT at virt addr {:#x?} -> phys addr {:#x}",
            addr, gdt_arena.start
        );

        map_builder.add_arena(gdt_arena);

        self.map_builder = Some(map_builder);

        Ok(())
    }

    pub fn enter_kernel(
        mut self,
        args: Vec<String>,
        bootstrap_image: Vec<u8>,
    ) -> Result<(), LoadableError> {
        let gdt = Gdt::resource().ok_or(LoadableError::OutOfSystemResource)?;
        let fb = Framebuffer::resource()
            .map(|f| (f.mode().clone(), f.buffer()))
            .map(|(m, f)| {
                let fb_len = f.len();
                let fb_ptr = f.as_mut_ptr() as usize + KERNEL_SPACE_START;
                let fb: &mut [u8] =
                    unsafe { core::slice::from_raw_parts_mut(fb_ptr as *mut u8, fb_len) };
                (fb, m)
            });

        let rsdp = Acpi::resource().map(|acpi| acpi.rsdp_address as usize);

        // DEBUG
        let page_table = self.offset_table()?;
        page_table.translate(VirtAddr::new(gdt.as_arena().start as _));

        // Construct BootInfo
        let loaded_sectons = self.loaded_sections.take();
        let (
            boot_info_phys,
            args_slice_phys,
            args_len,
            section_slice_phys,
            section_slice_len,
            bootstrap_phys,
            bootstrap_len,
        ) = self.alloc_bootinfo(
            &args,
            {
                if self.requirements.mapped_sections {
                    loaded_sectons.as_ref()
                } else {
                    None
                }
            },
            &bootstrap_image,
        )?;
        let boot_info_ptr = boot_info_phys as *mut BootInfo;

        let args_slice = if args_len > 0 {
            Some(unsafe {
                core::slice::from_raw_parts(
                    (KERNEL_SPACE_START + args_slice_phys) as *const &str,
                    args_len,
                )
            })
        } else {
            None
        };

        let section_slice = if section_slice_len > 0 {
            Some(unsafe {
                core::slice::from_raw_parts(
                    (KERNEL_SPACE_START + section_slice_phys) as *const (&str, Arena),
                    section_slice_len,
                )
            })
        } else {
            None
        };

        let bootstrap_slice = if bootstrap_len > 0 {
            Some(unsafe {
                core::slice::from_raw_parts(
                    (KERNEL_SPACE_START + bootstrap_phys) as *const u8,
                    bootstrap_len,
                )
            })
        } else {
            None
        };

        let (arena_len, arena_list_ptr) = self.map_builder.unwrap().construct_memory_map();

        let boot_info = BootInfo {
            framebuffer: fb,
            memory_map: MemoryMap::from_raw(
                arena_len,
                (arena_list_ptr as usize + KERNEL_SPACE_START) as _,
            ),
            rsdp,
            physical_memory_offset: KERNEL_SPACE_START,
            image_info: Some(KernelImageInfo {
                slide: self.slide.ok_or(LoadableError::ImageNotLoaded)?,
                symtab: self.symtab,
                mapped_sections: section_slice,
            }),
            bootstrap: bootstrap_slice,
            args: args_slice,
        };

        unsafe {
            boot_info_ptr.write(boot_info);
        }

        let boot_info_virt = KERNEL_SPACE_START + boot_info_phys;

        info!(
            "Entering kernel at entry point {:#x} with stack top {:#x}",
            self.entry_point.ok_or(LoadableError::ImageNotLoaded)?,
            self.stack_top.ok_or(LoadableError::ImageNotLoaded)?
        );

        unsafe {
            let _ = uefi::boot::exit_boot_services(None);
        }

        unsafe {
            gdt.load();
            Self::context_switch(Addresses {
                entry: self.entry_point.ok_or(LoadableError::ImageNotLoaded)?,
                stack_top: self.stack_top.ok_or(LoadableError::ImageNotLoaded)?,
                page_table: self.page_table.ok_or(LoadableError::ImageNotLoaded)?,
                info_ptr: boot_info_virt,
            });
        }
    }

    unsafe fn context_switch(addresses: Addresses) -> ! {
        unsafe {
            asm!(
                "xor rbp, rbp",
                "mov cr3, {pt}",
                "mov rsp, {st}",
                "push 0",      // Fake return address
                "jmp {ent}",
                pt = in(reg) addresses.page_table as *const _ as usize,
                st = in(reg) addresses.stack_top,
                ent = in(reg) addresses.entry,
                in("rdi") addresses.info_ptr,
                options(noreturn)
            );
        }
    }

    pub fn alloc_bootinfo(
        &mut self,
        args: &Vec<String>,
        mapped_section: Option<&Vec<(String, Arena)>>,
        bootstrap_image: &[u8],
    ) -> Result<(usize, usize, usize, usize, usize, usize, usize), LoadableError> {
        let mut map_builder = self
            .map_builder
            .take()
            .ok_or(LoadableError::ImageNotLoaded)?;

        // --- 1) 统计所有字符串总长度（args + mapped_section names） ---
        let args_total_len: usize = args.iter().map(|s| s.len()).sum();
        let mapped_names_total_len: usize = mapped_section
            .as_ref()
            .map(|v| v.iter().map(|(name, _)| name.len()).sum())
            .unwrap_or(0);
        let total_len: usize = args_total_len + mapped_names_total_len;

        // 按页对齐分配字符串区域
        let strings_pages = (total_len + 0xFFF) / 0x1000;
        let strings_addr = if strings_pages > 0 {
            unsafe {
                map_builder.allocate_and_mark(strings_pages, ArenaKind::BootloaderProvideInfo)
            }
        } else {
            0usize
        };

        // 当前写入字符串的物理地址偏移（相对于 strings_addr）
        let mut current_addr = strings_addr;
        // 为 args 准备 (ptr, len) 对应的记录
        let mut args_entries: Vec<(usize, usize)> = Vec::with_capacity(args.len());
        for arg in args {
            let len = arg.len();
            if len > 0 {
                // 把字符串字节拷贝到 strings 区域
                unsafe {
                    core::ptr::copy_nonoverlapping(arg.as_ptr(), current_addr as *mut u8, len);
                }
                args_entries.push((KERNEL_SPACE_START + current_addr, len));
                current_addr = current_addr + len;
            } else {
                // 空字符串使用 (0,0)
                args_entries.push((0, 0));
            }
        }

        // 为 mapped_section 的名字拷贝与记录位置
        // 记录每个 mapped_section 元素 (name_ptr, name_len, arena)
        let mut mapped_entries_names_and_arenas: Vec<(usize, usize, Arena)> = Vec::new();
        if let Some(ref mapped_vec) = mapped_section {
            for (name, arena) in mapped_vec.iter() {
                let len = name.len();
                if len > 0 {
                    unsafe {
                        core::ptr::copy_nonoverlapping(name.as_ptr(), current_addr as *mut u8, len);
                    }
                    mapped_entries_names_and_arenas.push((
                        KERNEL_SPACE_START + current_addr,
                        len,
                        arena.clone(),
                    ));
                    current_addr = current_addr + len;
                } else {
                    // 空名字 -> (0,0)
                    mapped_entries_names_and_arenas.push((0, 0, arena.clone()));
                }
            }
        }

        let args_slice_len = args_entries.len();
        let args_element_size = core::mem::size_of::<&'static str>(); // fat pointer (ptr,len) -> 2 usize
        let args_slice_size = args_slice_len * args_element_size;
        let args_slice_pages = (args_slice_size + 0xFFF) / 0x1000;
        let args_slice_addr = if args_slice_pages > 0 {
            unsafe {
                map_builder.allocate_and_mark(args_slice_pages, ArenaKind::BootloaderProvideInfo)
            }
        } else {
            0usize
        };

        if args_slice_addr != 0 {
            let slice_ptr = args_slice_addr as *mut usize;
            for (i, (ptr, len)) in args_entries.into_iter().enumerate() {
                unsafe {
                    slice_ptr.add(i * 2).write(ptr);
                    slice_ptr.add(i * 2 + 1).write(len);
                }
            }
        }

        let mapped_slice_len = mapped_entries_names_and_arenas.len();
        let usize_sz = core::mem::size_of::<usize>();
        let arena_sz = core::mem::size_of::<Arena>();
        let mapped_element_size = usize_sz * 2 + arena_sz;
        let mapped_slice_size = mapped_slice_len * mapped_element_size;
        let mapped_slice_pages = (mapped_slice_size + 0xFFF) / 0x1000;
        let mapped_slice_addr = if mapped_slice_pages > 0 {
            unsafe {
                map_builder.allocate_and_mark(mapped_slice_pages, ArenaKind::BootloaderProvideInfo)
            }
        } else {
            0usize
        };

        if mapped_slice_addr != 0 {
            let base_ptr = mapped_slice_addr as *mut u8;
            for (i, (ptr, len, arena)) in mapped_entries_names_and_arenas.into_iter().enumerate() {
                let entry_base = unsafe { base_ptr.add(i * mapped_element_size) };

                unsafe {
                    (entry_base as *mut usize).write(ptr);
                }
                unsafe {
                    (entry_base.add(usize_sz) as *mut usize).write(len);
                }
                unsafe {
                    let arena_dest = entry_base.add(usize_sz * 2) as *mut u8;
                    let arena_src = &arena as *const Arena as *const u8;
                    core::ptr::copy_nonoverlapping(arena_src, arena_dest, arena_sz);
                }
            }
        }

        let bootstrap_len = bootstrap_image.len();
        let bootstrap_pages = (bootstrap_len + 0xFFF) / 0x1000;
        let bootstrap_addr = if bootstrap_pages > 0 {
            unsafe {
                map_builder.allocate_and_mark(bootstrap_pages, ArenaKind::BootloaderProvideInfo)
            }
        } else {
            0usize
        };

        if bootstrap_addr != 0 {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    bootstrap_image.as_ptr(),
                    bootstrap_addr as *mut u8,
                    bootstrap_len,
                );
            }
        }

        let boot_info_size = core::mem::size_of::<BootInfo>();
        let boot_info_pages = (boot_info_size + 0xFFF) / 0x1000;
        let boot_info_addr = unsafe {
            map_builder.allocate_and_mark(boot_info_pages, ArenaKind::BootloaderProvideInfo)
        };

        self.map_builder = Some(map_builder);

        Ok((
            boot_info_addr,
            args_slice_addr,
            args_slice_len,
            mapped_slice_addr,
            mapped_slice_len,
            bootstrap_addr,
            bootstrap_len,
        ))
    }

    fn offset_table(&mut self) -> Result<OffsetPageTable<'_>, LoadableError> {
        let page_table = self
            .page_table
            .as_mut()
            .ok_or(LoadableError::ImageNotLoaded)?;
        Ok(unsafe { OffsetPageTable::new(*page_table, VirtAddr::new(0)) })
    }
}
