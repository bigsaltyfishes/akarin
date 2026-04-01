use alloc::vec::Vec;
use core::{
    fmt::Debug,
    ops::{Deref, DerefMut},
};

use goblin::{
    container::Ctx,
    mach::{
        Mach, MachO,
        constants::{VM_PROT_EXECUTE, VM_PROT_READ, VM_PROT_WRITE},
        cputype::CPU_TYPE_X86_64,
        header::MH_EXECUTE,
        parse_magic_and_ctx,
    },
};
use log::trace;
use thiserror::Error;

#[repr(C)]
#[derive(Debug, Clone)]
pub struct Nlist {
    /// index into the string table
    pub n_strx: u32,
    /// type flag, see below
    pub n_type: u8,
    /// section number or NO_SECT
    pub n_sect: u8,
    /// see <mach-o/stab.h>
    pub n_desc: u16,
    /// value of this symbol (or stab offset)
    pub n_value: u64,
}

#[derive(Debug, Error)]
pub enum ImageParserError {
    #[error("Mach-O Parsing Error: {0}")]
    MachOParsingError(#[from] goblin::error::Error),
    #[error("Required architecture not found in Fat Mach-O binary")]
    ArchNotFound,
    #[error("Unsupported executable image: {0}")]
    UnsupportedExecutable(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachOSegmentProtections {
    pub read: bool,
    pub write: bool,
    pub execute: bool,
}

impl MachOSegmentProtections {
    /// Return whether this segment carries no accessible protection bits.
    fn from_mach(init_prot: u32) -> Self {
        Self {
            read: init_prot & VM_PROT_READ != 0,
            write: init_prot & VM_PROT_WRITE != 0,
            execute: init_prot & VM_PROT_EXECUTE != 0,
        }
    }

    pub fn is_empty(self) -> bool {
        !self.read && !self.write && !self.execute
    }
}

/// One loadable Mach-O segment view extracted from the image load commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachOLoadSegment {
    seg_index: u8,
    vm_addr: usize,
    vm_size: usize,
    file_offset: usize,
    file_size: usize,
    init_prot: MachOSegmentProtections,
}

impl MachOLoadSegment {
    /// Return the original segment index inside the Mach-O segment table.
    pub fn seg_index(self) -> u8 {
        self.seg_index
    }

    /// Return the preferred virtual address before applying any slide.
    pub fn vm_addr(self) -> usize {
        self.vm_addr
    }

    /// Return the in-memory byte size of this segment.
    pub fn vm_size(self) -> usize {
        self.vm_size
    }

    /// Return the file offset of the initialized bytes.
    pub fn file_offset(self) -> usize {
        self.file_offset
    }

    /// Return the number of initialized bytes present in the file image.
    pub fn file_size(self) -> usize {
        self.file_size
    }

    /// Return the initial protection bits encoded in the segment command.
    pub fn init_prot(self) -> MachOSegmentProtections {
        self.init_prot
    }

    /// Return whether the segment carries initialized bytes in the file image.
    pub fn has_file_backing(self) -> bool {
        self.file_size != 0
    }

    /// Return whether the segment should be mapped into the target address
    /// space.
    pub fn requires_mapping(self) -> bool {
        self.vm_size != 0 && (!self.init_prot.is_empty() || self.has_file_backing())
    }
}

/// Parsed executable layout extracted from one Mach-O image.
#[derive(Debug, Clone)]
pub struct MachOExecutable {
    entry: usize,
    image_base: usize,
    image_end: usize,
    segments: Vec<MachOLoadSegment>,
}

impl MachOExecutable {
    /// Return the executable entry point before applying any slide.
    pub fn entry_point(&self) -> usize {
        self.entry
    }

    /// Return the lowest loadable segment base address.
    pub fn image_base(&self) -> usize {
        self.image_base
    }

    /// Return the exclusive upper bound of the loadable image span.
    pub fn image_end(&self) -> usize {
        self.image_end
    }

    /// Return the loadable segment list in original Mach-O order.
    pub fn segments(&self) -> &[MachOLoadSegment] {
        &self.segments
    }
}

pub struct MachOImage {
    data: Vec<u8>,
    ctx: Ctx,
    offset: usize,
}

impl MachOImage {
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, ImageParserError> {
        let (f, mut offset) = match Mach::parse(&bytes)? {
            Mach::Binary(_) => (None, Some(0usize)),
            Mach::Fat(f) => {
                trace!(
                    "Detected Fat Mach-O binary with {} architectures",
                    f.narches
                );
                (Some(f), None)
            }
        };

        if let Some(ref fat) = f {
            fat.arches()?.iter().for_each(|f| {
                if f.cputype() == CPU_TYPE_X86_64 {
                    offset = Some(f.offset as _);
                }
            });
        }

        let (_magic, ctx) = parse_magic_and_ctx(&bytes, offset.unwrap_or(0))?;

        Ok(Self {
            data: bytes,
            ctx: ctx.unwrap(),
            offset: offset.ok_or(ImageParserError::ArchNotFound)?,
        })
    }

    pub fn binary(&self) -> Result<MachO<'_>, ImageParserError> {
        Ok(MachO::parse(&self.data, self.offset)?)
    }

    /// Extract the executable entry point and loadable segment layout.
    pub fn executable(&self) -> Result<MachOExecutable, ImageParserError> {
        let binary = self.binary()?;
        if binary.header.filetype != MH_EXECUTE {
            return Err(ImageParserError::UnsupportedExecutable(
                "only MH_EXECUTE images are supported",
            ));
        }
        if binary.entry == 0 {
            return Err(ImageParserError::UnsupportedExecutable(
                "image does not define one entry point",
            ));
        }

        let mut segments = Vec::new();
        let mut image_base = usize::MAX;
        let mut image_end = 0usize;
        for (seg_index, segment) in binary.segments.iter().enumerate() {
            let vm_addr = segment.vmaddr as usize;
            let vm_size = segment.vmsize as usize;
            let file_offset = segment.fileoff as usize;
            let file_size = segment.filesize as usize;
            let load = MachOLoadSegment {
                seg_index: seg_index as u8,
                vm_addr,
                vm_size,
                file_offset,
                file_size,
                init_prot: MachOSegmentProtections::from_mach(segment.initprot),
            };
            if !load.requires_mapping() {
                continue;
            }
            image_base = image_base.min(vm_addr);
            image_end = image_end.max(vm_addr.saturating_add(vm_size));
            segments.push(load);
        }

        if segments.is_empty() || image_base == usize::MAX || image_end <= image_base {
            return Err(ImageParserError::UnsupportedExecutable(
                "image does not contain any loadable segment",
            ));
        }

        Ok(MachOExecutable {
            entry: binary.entry as usize,
            image_base,
            image_end,
            segments,
        })
    }

    /// Return the initialized file bytes for one extracted load segment.
    pub fn segment_bytes<'a>(
        &'a self,
        segment: &MachOLoadSegment,
    ) -> Result<&'a [u8], ImageParserError> {
        let end = segment.file_offset.checked_add(segment.file_size).ok_or(
            ImageParserError::UnsupportedExecutable("segment file range overflowed"),
        )?;
        if end > self.len() {
            return Err(ImageParserError::UnsupportedExecutable(
                "segment file range exceeds image size",
            ));
        }
        Ok(&self[segment.file_offset..end])
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    pub fn ctx(&self) -> Ctx {
        self.ctx.clone()
    }

    /// Translates a virtual memory address to a file offset, if it exists in
    /// the Mach-O segments.
    pub fn vm_of(&self, vm_addr: u64) -> Result<Option<usize>, ImageParserError> {
        let binary = self.binary()?;
        for seg in binary.segments.iter() {
            if seg.vmaddr <= vm_addr && vm_addr < seg.vmaddr + seg.vmsize {
                let offset_in_seg = vm_addr - seg.vmaddr;
                if offset_in_seg < seg.filesize {
                    return Ok(Some((offset_in_seg + seg.fileoff) as _));
                } else {
                    return Ok(None);
                }
            }
        }

        Ok(None)
    }
}

impl Debug for MachOImage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MachOImage")
            .field("data_len", &self.data.len())
            .field("offset", &self.offset)
            .finish()
    }
}

impl Deref for MachOImage {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.data[self.offset..]
    }
}

impl DerefMut for MachOImage {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data[self.offset..]
    }
}

pub type KernelImage = MachOImage;
