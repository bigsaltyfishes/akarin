//! Internal PCI interrupt-capability parsing and programming support.

use libakarin_core::io::MmioRegion;
use libakarin_machine_core::{
    interrupt::{IrqError, IrqResult, MessageIrqDescriptor},
    memory::{AddressSpaceTrait, PhysAddr, VirtAddr},
};

use super::{PciBdf, PciConfigAccess};
use crate::arch::Machine;

const PCI_VENDOR_ID: u16 = 0x00;
const PCI_COMMAND: u16 = 0x04;
const PCI_STATUS: u16 = 0x06;
const PCI_HEADER_TYPE: u16 = 0x0e;
const PCI_CAP_PTR: u16 = 0x34;
const PCI_BAR0: u16 = 0x10;

const PCI_STATUS_CAP_LIST: u16 = 1 << 4;
const PCI_COMMAND_INTX_DISABLE: u16 = 1 << 10;

const PCI_CAP_ID_MSI: u8 = 0x05;
const PCI_CAP_ID_MSIX: u8 = 0x11;

const MSI_CONTROL_ENABLE: u16 = 1 << 0;
const MSI_CONTROL_MMC_MASK: u16 = 0b111 << 1;
const MSI_CONTROL_MME_MASK: u16 = 0b111 << 4;
const MSI_CONTROL_64BIT: u16 = 1 << 7;
const MSI_CONTROL_MASKING: u16 = 1 << 8;

const MSIX_CONTROL_MASK_ALL: u16 = 1 << 14;
const MSIX_CONTROL_ENABLE: u16 = 1 << 15;
const PCI_BAR_MEM_PREFETCHABLE: u32 = 1 << 3;

/// Compact BAR layout summary for one PCI function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PciBarSummary {
    pub bar_count: usize,
    pub present_mask: usize,
    pub io_mask: usize,
    pub mmio64_mask: usize,
    pub prefetchable_mask: usize,
}

/// Compact capability summary for one PCI function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PciCapabilitySummary {
    pub capability_bits: usize,
    pub max_msi_vectors: usize,
    pub max_msix_vectors: usize,
}

/// Interrupt mode supported by one PCI function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PciInterruptMode {
    Intx,
    Msi,
    Msix,
}

/// Summary of interrupt capability coverage across discovered PCI functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PciInterruptSummary {
    pub present_functions: usize,
    pub msi_functions: usize,
    pub msix_functions: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PciBar {
    phys_base: usize,
}

impl PciBar {
    fn table_region(self, offset: usize, entries: usize) -> IrqResult<MmioRegion> {
        let size = offset
            .checked_add(entries.checked_mul(16).ok_or(IrqError::InvalidParameter)?)
            .ok_or(IrqError::InvalidParameter)?;
        let virt = Machine::phys_to_virt(PhysAddr::new(self.phys_base))
            .unwrap_or(VirtAddr::new(self.phys_base));
        Ok(MmioRegion {
            phys_base: PhysAddr::new(self.phys_base),
            virt_base: virt,
            size,
        })
    }
}

struct PciCapabilityWalker<'a, A: PciConfigAccess> {
    access: &'a A,
    bdf: PciBdf,
    next: Option<u16>,
    remaining: usize,
}

impl<'a, A: PciConfigAccess> PciCapabilityWalker<'a, A> {
    fn new(access: &'a A, bdf: PciBdf) -> Self {
        let status = access.read_u16(bdf, PCI_STATUS);
        let next = if status & PCI_STATUS_CAP_LIST != 0 {
            Some(u16::from(access.read_u8(bdf, PCI_CAP_PTR) & !0x3))
        } else {
            None
        };
        Self {
            access,
            bdf,
            next,
            remaining: 48,
        }
    }
}

impl<A: PciConfigAccess> Iterator for PciCapabilityWalker<'_, A> {
    type Item = (u8, u16);

    fn next(&mut self) -> Option<Self::Item> {
        let offset = self.next?;
        if offset < 0x40 || offset >= 0x100 || self.remaining == 0 {
            self.next = None;
            return None;
        }

        self.remaining -= 1;
        let id = self.access.read_u8(self.bdf, offset);
        let next = u16::from(self.access.read_u8(self.bdf, offset + 1) & !0x3);
        self.next = (next != 0).then_some(next);
        Some((id, offset))
    }
}

/// Interrupt-capability view over one PCI function.
pub struct PciInterruptCapabilities<'a, A: PciConfigAccess> {
    access: &'a A,
    bdf: PciBdf,
}

impl<'a, A: PciConfigAccess> PciInterruptCapabilities<'a, A> {
    /// Build one capability view over the supplied PCI function.
    pub const fn new(access: &'a A, bdf: PciBdf) -> Self {
        Self { access, bdf }
    }

    /// Return whether this function exists in config space.
    pub fn present(&self) -> bool {
        self.access.contains(self.bdf) && self.access.read_u16(self.bdf, PCI_VENDOR_ID) != u16::MAX
    }

    /// Return the most capable interrupt mode currently discoverable.
    pub fn interrupt_mode(&self) -> PciInterruptMode {
        if self.msix().is_some() {
            PciInterruptMode::Msix
        } else if self.msi().is_some() {
            PciInterruptMode::Msi
        } else {
            PciInterruptMode::Intx
        }
    }

    /// Return the parsed MSI capability, if present.
    pub fn msi(&self) -> Option<PciMsiCapability> {
        self.walk()
            .find_map(|(id, offset)| {
                (id == PCI_CAP_ID_MSI)
                    .then(|| PciMsiCapability::parse(self.access, self.bdf, offset))
            })
            .flatten()
    }

    /// Return the parsed MSI-X capability, if present.
    pub fn msix(&self) -> Option<PciMsixCapability> {
        self.walk()
            .find_map(|(id, offset)| {
                (id == PCI_CAP_ID_MSIX)
                    .then(|| PciMsixCapability::parse(self.access, self.bdf, offset))
            })
            .flatten()
    }

    /// Disable every currently programmable interrupt delivery mode and mask
    /// legacy INTx delivery.
    pub fn disable_all_interrupts(&self) {
        if let Some(msi) = self.msi() {
            msi.disable(self.access, self.bdf);
        }
        if let Some(msix) = self.msix() {
            msix.disable(self);
        }
        self.set_intx_enabled(false);
    }

    /// Return one compact summary of this function's BAR layout.
    pub fn bar_summary(&self) -> PciBarSummary {
        let Some(bar_count) = self.bar_count() else {
            return PciBarSummary::default();
        };

        let mut summary = PciBarSummary {
            bar_count,
            ..PciBarSummary::default()
        };
        let mut index = 0usize;
        while index < bar_count {
            let offset = PCI_BAR0 + (index as u16) * 4;
            let low = self.access.read_u32(self.bdf, offset);
            if low == 0 {
                index += 1;
                continue;
            }

            summary.present_mask |= 1usize << index;
            if low & 0x1 != 0 {
                summary.io_mask |= 1usize << index;
                index += 1;
                continue;
            }

            let bar_type = (low >> 1) & 0x3;
            if bar_type == 0b10 {
                summary.mmio64_mask |= 1usize << index;
                index += 2;
            } else {
                index += 1;
            }
            if low & PCI_BAR_MEM_PREFETCHABLE != 0 {
                summary.prefetchable_mask |= 1usize << (index.saturating_sub(1));
            }
        }
        summary
    }

    /// Return one compact summary of this function's interrupt capability set.
    pub fn capability_summary(&self) -> PciCapabilitySummary {
        let mut summary = PciCapabilitySummary::default();
        let status = self.access.read_u16(self.bdf, PCI_STATUS);
        if status & PCI_STATUS_CAP_LIST != 0 {
            summary.capability_bits |= libakarin_syscall::PCI_CAPABILITY_LIST;
        }
        if let Some(msi) = self.msi() {
            summary.capability_bits |= libakarin_syscall::PCI_CAPABILITY_MSI;
            summary.max_msi_vectors = msi.max_messages;
        }
        if let Some(msix) = self.msix() {
            summary.capability_bits |= libakarin_syscall::PCI_CAPABILITY_MSIX;
            summary.max_msix_vectors = msix.table_size;
        }
        summary
    }

    fn walk(&self) -> PciCapabilityWalker<'_, A> {
        PciCapabilityWalker::new(self.access, self.bdf)
    }

    fn set_intx_enabled(&self, enabled: bool) {
        let mut command = self.access.read_u16(self.bdf, PCI_COMMAND);
        if enabled {
            command &= !PCI_COMMAND_INTX_DISABLE;
        } else {
            command |= PCI_COMMAND_INTX_DISABLE;
        }
        self.access.write_u16(self.bdf, PCI_COMMAND, command);
    }

    fn bar_count(&self) -> Option<usize> {
        let header_type = self.access.read_u8(self.bdf, PCI_HEADER_TYPE) & 0x7f;
        match header_type {
            0x00 => Some(6),
            0x01 => Some(2),
            _ => None,
        }
    }

    fn bar(&self, index: u8) -> IrqResult<PciBar> {
        if index >= 6 {
            return Err(IrqError::InvalidParameter);
        }

        let offset = PCI_BAR0 + u16::from(index) * 4;
        let low = self.access.read_u32(self.bdf, offset);
        if low & 0x1 != 0 {
            return Err(IrqError::NotSupported);
        }

        let bar_type = (low >> 1) & 0x3;
        let phys_base = match bar_type {
            0b00 => usize::try_from(low & !0xf).map_err(|_| IrqError::InvalidParameter)?,
            0b10 => {
                if index >= 5 {
                    return Err(IrqError::InvalidParameter);
                }
                let high = self.access.read_u32(self.bdf, offset + 4);
                let base = (u64::from(high) << 32) | u64::from(low & !0xf);
                usize::try_from(base).map_err(|_| IrqError::InvalidParameter)?
            }
            _ => return Err(IrqError::NotSupported),
        };

        Ok(PciBar { phys_base })
    }
}

impl super::PciEcamConfig {
    /// Summarize interrupt-capability coverage for every discovered PCI
    /// function.
    pub fn interrupt_summary(&self) -> PciInterruptSummary {
        let mut summary = PciInterruptSummary::default();
        self.for_each_present_function(|bdf| {
            summary.present_functions += 1;
            let caps = PciInterruptCapabilities::new(self, bdf);
            if caps.msix().is_some() {
                summary.msix_functions += 1;
            } else if caps.msi().is_some() {
                summary.msi_functions += 1;
            }
        });
        summary
    }
}

/// Parsed MSI capability metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciMsiCapability {
    pub offset: u16,
    pub supports_64bit: bool,
    pub supports_masking: bool,
    pub max_messages: usize,
}

impl PciMsiCapability {
    fn parse<A: PciConfigAccess>(access: &A, bdf: PciBdf, offset: u16) -> Option<Self> {
        let control = access.read_u16(bdf, offset + 2);
        let max_messages = 1usize << usize::from((control & MSI_CONTROL_MMC_MASK) >> 1);
        Some(Self {
            offset,
            supports_64bit: control & MSI_CONTROL_64BIT != 0,
            supports_masking: control & MSI_CONTROL_MASKING != 0,
            max_messages,
        })
    }

    /// Return whether this capability can encode the supplied message count.
    pub fn supports_count(self, count: usize) -> bool {
        count.is_power_of_two() && count <= self.max_messages
    }

    /// Program this function into MSI mode using the supplied message
    /// descriptors.
    pub fn enable<A: PciConfigAccess>(
        self,
        caps: &PciInterruptCapabilities<'_, A>,
        descriptors: &[MessageIrqDescriptor],
    ) -> IrqResult {
        self.validate_descriptors(descriptors)?;
        let access = caps.access;
        let bdf = caps.bdf;
        let mme = descriptors.len().trailing_zeros() as u16;
        let base = descriptors[0];
        let mut control = access.read_u16(bdf, self.offset + 2);
        control &= !MSI_CONTROL_MME_MASK;
        control |= mme << 4;
        control |= MSI_CONTROL_ENABLE;

        access.write_u32(bdf, self.offset + 4, base.address_lo);
        let data_offset = if self.supports_64bit {
            access.write_u32(bdf, self.offset + 8, base.address_hi);
            self.offset + 12
        } else {
            self.offset + 8
        };
        access.write_u16(bdf, data_offset, base.data as u16);

        if self.supports_masking {
            let mask_offset = if self.supports_64bit {
                self.offset + 16
            } else {
                self.offset + 12
            };
            access.write_u32(bdf, mask_offset, 0);
        }

        access.write_u16(bdf, self.offset + 2, control);
        caps.set_intx_enabled(false);
        Ok(())
    }

    /// Disable MSI delivery on this capability.
    pub fn disable<A: PciConfigAccess>(self, access: &A, bdf: PciBdf) {
        let control = access.read_u16(bdf, self.offset + 2) & !MSI_CONTROL_ENABLE;
        access.write_u16(bdf, self.offset + 2, control);
    }

    fn validate_descriptors(self, descriptors: &[MessageIrqDescriptor]) -> IrqResult {
        let Some(base) = descriptors.first().copied() else {
            return Err(IrqError::InvalidParameter);
        };
        if !self.supports_count(descriptors.len()) {
            return Err(IrqError::InvalidParameter);
        }
        if descriptors.len() > 1 && base.vector & (descriptors.len() - 1) != 0 {
            return Err(IrqError::InvalidParameter);
        }
        for (index, descriptor) in descriptors.iter().enumerate() {
            if descriptor.address_lo != base.address_lo
                || descriptor.address_hi != base.address_hi
                || descriptor.cpu_id != base.cpu_id
                || descriptor.vector != base.vector + index
                || descriptor.data != base.data + index as u32
            {
                return Err(IrqError::InvalidParameter);
            }
        }
        Ok(())
    }
}

/// Parsed MSI-X capability metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciMsixCapability {
    pub offset: u16,
    pub table_size: usize,
    pub table_bir: u8,
    pub table_offset: u32,
    pub pba_bir: u8,
    pub pba_offset: u32,
}

impl PciMsixCapability {
    fn parse<A: PciConfigAccess>(access: &A, bdf: PciBdf, offset: u16) -> Option<Self> {
        let control = access.read_u16(bdf, offset + 2);
        let table = access.read_u32(bdf, offset + 4);
        let pba = access.read_u32(bdf, offset + 8);
        Some(Self {
            offset,
            table_size: usize::from((control & 0x07ff) + 1),
            table_bir: (table & 0x7) as u8,
            table_offset: table & !0x7,
            pba_bir: (pba & 0x7) as u8,
            pba_offset: pba & !0x7,
        })
    }

    /// Program this function into MSI-X mode using the supplied message
    /// descriptors.
    pub fn enable<A: PciConfigAccess>(
        self,
        caps: &PciInterruptCapabilities<'_, A>,
        descriptors: &[MessageIrqDescriptor],
    ) -> IrqResult {
        if descriptors.is_empty() || descriptors.len() > self.table_size {
            return Err(IrqError::InvalidParameter);
        }

        let access = caps.access;
        let bdf = caps.bdf;
        let mut control = access.read_u16(bdf, self.offset + 2);
        control |= MSIX_CONTROL_MASK_ALL;
        control &= !MSIX_CONTROL_ENABLE;
        access.write_u16(bdf, self.offset + 2, control);

        let bar = caps.bar(self.table_bir)?;
        let table = bar.table_region(self.table_offset as usize, self.table_size)?;
        for (index, descriptor) in descriptors.iter().enumerate() {
            let base = self.table_offset as usize + index * 16;
            unsafe {
                let _ = table.write_u32(base, descriptor.address_lo);
                let _ = table.write_u32(base + 4, descriptor.address_hi);
                let _ = table.write_u32(base + 8, descriptor.data);
                let _ = table.write_u32(base + 12, 0);
            }
        }

        control |= MSIX_CONTROL_ENABLE;
        control &= !MSIX_CONTROL_MASK_ALL;
        access.write_u16(bdf, self.offset + 2, control);
        caps.set_intx_enabled(false);
        Ok(())
    }

    /// Disable MSI-X delivery on this capability.
    pub fn disable<A: PciConfigAccess>(self, caps: &PciInterruptCapabilities<'_, A>) {
        let access = caps.access;
        let bdf = caps.bdf;
        let mut control = access.read_u16(bdf, self.offset + 2);
        control &= !MSIX_CONTROL_ENABLE;
        control |= MSIX_CONTROL_MASK_ALL;
        access.write_u16(bdf, self.offset + 2, control);
    }
}
