use core::mem::size_of;

use goblin::mach::load_command::CommandVariant;
use log::{info, trace};
use thiserror::Error;

use crate::image::{ImageParserError, MachOImage};

const REBASE_OPCODE_MASK: u8 = 0xF0;
const REBASE_IMMEDIATE_MASK: u8 = 0x0F;

const REBASE_OPCODE_DONE: u8 = 0x00;
const REBASE_OPCODE_SET_TYPE_IMM: u8 = 0x10;
const REBASE_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB: u8 = 0x20;
const REBASE_OPCODE_ADD_ADDR_ULEB: u8 = 0x30;
const REBASE_OPCODE_ADD_ADDR_IMM_SCALED: u8 = 0x40;
const REBASE_OPCODE_DO_REBASE_IMM_TIMES: u8 = 0x50;
const REBASE_OPCODE_DO_REBASE_ULEB_TIMES: u8 = 0x60;
const REBASE_OPCODE_DO_REBASE_ADD_ADDR_ULEB: u8 = 0x70;
const REBASE_OPCODE_DO_REBASE_ULEB_TIMES_SKIPPING_ULEB: u8 = 0x80;

const REBASE_TYPE_POINTER: u8 = 1;
const REBASE_TYPE_TEXT_ABSOLUTE32: u8 = 2;
const REBASE_TYPE_TEXT_PCREL32: u8 = 3;

/// One abstract rebasing target addressed by final virtual addresses.
pub trait DyldRebaseTarget {
    /// Read bytes from the target virtual address space.
    fn read_at_vmaddr(&self, vm_addr: u64, buffer: &mut [u8]) -> Result<(), &'static str>;

    /// Write bytes into the target virtual address space.
    fn write_at_vmaddr(&self, vm_addr: u64, data: &[u8]) -> Result<(), &'static str>;
}

#[derive(Debug, Error)]
pub enum LinkerError {
    #[error("Failed to link Mach-O image: {0}")]
    LinkError(&'static str),
    #[error("Failed to read ULEB128 value: {0}")]
    UlebReadingError(&'static str),
    #[error("Failed to parse Mach-O image: {0}")]
    MachOParsingError(#[from] ImageParserError),
}

#[derive(Debug)]
pub struct DyldLinker<'a> {
    image: &'a mut MachOImage,
    slide: usize,
}

impl<'a> DyldLinker<'a> {
    pub fn new(image: &'a mut MachOImage, slide: usize) -> Self {
        Self { image, slide }
    }

    fn read_uleb128(&self, data: &[u8], offset: &mut usize) -> Result<u64, LinkerError> {
        let mut result = 0u64;
        let mut shift = 0;
        loop {
            if *offset >= data.len() {
                return Err(LinkerError::UlebReadingError(
                    "Unexpected end of data while reading ULEB128",
                ));
            }
            let byte = data[*offset];
            *offset += 1;
            result |= ((byte & 0x7F) as u64) << shift;
            if (byte & 0x80) == 0 {
                break;
            }
            shift += 7;
        }
        Ok(result)
    }

    fn apply_rebase<T>(
        &mut self,
        seg_idx: u8,
        seg_offset: u64,
        type_of_rebase: u8,
        target: &T,
    ) -> Result<(), LinkerError>
    where
        T: DyldRebaseTarget,
    {
        let binary = self.image.binary()?;
        let seg = binary
            .segments
            .get(seg_idx as usize)
            .ok_or(LinkerError::LinkError(
                "segment index out of range during rebase",
            ))?;
        let vm_addr = seg.vmaddr + seg_offset;
        let target_vm_addr = vm_addr
            .checked_add(self.slide as u64)
            .ok_or(LinkerError::LinkError("rebased virtual address overflowed"))?;

        match type_of_rebase {
            REBASE_TYPE_POINTER => {
                let mut buffer = [0u8; size_of::<usize>()];
                target
                    .read_at_vmaddr(target_vm_addr, &mut buffer)
                    .map_err(LinkerError::LinkError)?;
                let mut cur = usize::from_le_bytes(buffer);
                cur += self.slide;
                target
                    .write_at_vmaddr(target_vm_addr, &cur.to_le_bytes())
                    .map_err(LinkerError::LinkError)?;
                trace!(
                    "Applied pointer rebase on target: seg_idx={}, seg_offset={:#x}, \
                     vm_addr={:#x}, target_vm_addr={:#x}, new_addr={:#x}, slide={:#x}",
                    seg_idx, seg_offset, vm_addr, target_vm_addr, cur, self.slide
                );
                Ok(())
            }
            REBASE_TYPE_TEXT_ABSOLUTE32 => {
                let mut buffer = [0u8; size_of::<u32>()];
                target
                    .read_at_vmaddr(target_vm_addr, &mut buffer)
                    .map_err(LinkerError::LinkError)?;
                let mut cur = u32::from_le_bytes(buffer);
                cur = cur.wrapping_add(self.slide as u32);
                target
                    .write_at_vmaddr(target_vm_addr, &cur.to_le_bytes())
                    .map_err(LinkerError::LinkError)?;
                trace!(
                    "Applied 32-bit rebase on target: seg_idx={}, seg_offset={:#x}, \
                     vm_addr={:#x}, target_vm_addr={:#x}, new_addr={:#x}, slide={:#x}",
                    seg_idx, seg_offset, vm_addr, target_vm_addr, cur, self.slide
                );
                Ok(())
            }
            REBASE_TYPE_TEXT_PCREL32 => Ok(()),
            _ => Err(LinkerError::LinkError("Unsupported rebase type")),
        }
    }

    pub fn link<T>(mut self, target: &T) -> Result<(), LinkerError>
    where
        T: DyldRebaseTarget,
    {
        let binary = self.image.binary()?;
        let mut rebase_off = 0;
        let mut rebase_size = 0;

        for cmd in binary.load_commands.iter() {
            match cmd.command {
                CommandVariant::DyldInfo(dyld) => {
                    if dyld.bind_size > 0 || dyld.weak_bind_size > 0 || dyld.lazy_bind_size > 0 {
                        return Err(LinkerError::LinkError(
                            "Binding information is not supported",
                        ));
                    }
                    rebase_off = dyld.rebase_off as usize;
                    rebase_size = dyld.rebase_size as usize;
                }
                CommandVariant::DyldInfoOnly(dyld) => {
                    if dyld.bind_size > 0 || dyld.weak_bind_size > 0 || dyld.lazy_bind_size > 0 {
                        return Err(LinkerError::LinkError(
                            "Binding information is not supported",
                        ));
                    }
                    rebase_off = dyld.rebase_off as usize;
                    rebase_size = dyld.rebase_size as usize;
                }
                _ => {}
            }
        }

        if rebase_size == 0 {
            return Ok(());
        }

        info!(
            "Processing rebase information at offset {:#x} with size {:#x}",
            rebase_off, rebase_size
        );

        let mut current = rebase_off;
        let end = rebase_off + rebase_size;
        let mut type_of_rebase = 0u8;
        let mut seg_idx = 0u8;
        let mut seg_offset = 0u64;

        while current < end {
            let opcode = self.image[current];
            current += 1;
            let op = opcode & REBASE_OPCODE_MASK;
            let imm = opcode & REBASE_IMMEDIATE_MASK;

            let pointer_size = match type_of_rebase {
                REBASE_TYPE_POINTER => size_of::<usize>() as u64,
                REBASE_TYPE_TEXT_ABSOLUTE32 | REBASE_TYPE_TEXT_PCREL32 => size_of::<u32>() as u64,
                _ => size_of::<usize>() as u64,
            };

            match op {
                REBASE_OPCODE_DONE => {
                    if end - current > 15 {
                        return Err(LinkerError::LinkError("Rebase opcode terminated early"));
                    }
                    break;
                }
                REBASE_OPCODE_SET_TYPE_IMM => match imm {
                    REBASE_TYPE_POINTER
                    | REBASE_TYPE_TEXT_ABSOLUTE32
                    | REBASE_TYPE_TEXT_PCREL32 => {
                        type_of_rebase = imm;
                    }
                    _ => {
                        type_of_rebase = 0;
                    }
                },
                REBASE_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB => {
                    seg_idx = imm;
                    seg_offset = self.read_uleb128(&self.image, &mut current)?;
                }
                REBASE_OPCODE_ADD_ADDR_ULEB => {
                    seg_offset += self.read_uleb128(&self.image, &mut current)?;
                }
                REBASE_OPCODE_ADD_ADDR_IMM_SCALED => {
                    seg_offset += (imm as u64) * pointer_size;
                }
                REBASE_OPCODE_DO_REBASE_IMM_TIMES => {
                    trace!(
                        "Rebase imm times: count={}, seg_idx={}, seg_offset={:#x}, type={}",
                        imm, seg_idx, seg_offset, type_of_rebase
                    );
                    for _ in 0..imm {
                        self.apply_rebase(seg_idx, seg_offset, type_of_rebase, target)?;
                        seg_offset += pointer_size;
                    }
                }
                REBASE_OPCODE_DO_REBASE_ULEB_TIMES => {
                    let count = self.read_uleb128(&self.image, &mut current)?;

                    trace!(
                        "Rebase ULEB times: count={}, seg_idx={}, seg_offset={:#x}",
                        count, seg_idx, seg_offset
                    );
                    for _ in 0..count {
                        self.apply_rebase(seg_idx, seg_offset, type_of_rebase, target)?;
                        seg_offset += pointer_size;
                    }
                }
                REBASE_OPCODE_DO_REBASE_ADD_ADDR_ULEB => {
                    trace!(
                        "Rebase add addr ULEB: seg_idx={}, seg_offset={:#x}, type={}",
                        seg_idx, seg_offset, type_of_rebase
                    );
                    self.apply_rebase(seg_idx, seg_offset, type_of_rebase, target)?;
                    let add = self.read_uleb128(&self.image, &mut current)?;
                    seg_offset += add + pointer_size;
                }
                REBASE_OPCODE_DO_REBASE_ULEB_TIMES_SKIPPING_ULEB => {
                    let count = self.read_uleb128(&self.image, &mut current)?;
                    let skip = self.read_uleb128(&self.image, &mut current)?;

                    trace!(
                        "Rebase ULEB times: count={}, skip={:#x}, seg_idx={}, seg_offset={:#x}",
                        count, skip, seg_idx, seg_offset
                    );

                    let step = skip + pointer_size;
                    for _ in 0..count {
                        self.apply_rebase(seg_idx, seg_offset, type_of_rebase, target)?;
                        seg_offset += step;
                    }
                }
                _ => {
                    return Err(LinkerError::LinkError("Unknown rebase opcode"));
                }
            }
        }

        Ok(())
    }
}
