//! Internal PCI configuration-space access contract.

use alloc::{format, string::String, vec::Vec};

use acpi::{AcpiTables, platform::PciConfigRegions};
use libakarin_core::io::MmioRegion;
use libakarin_machine_core::{
    interrupt::{IrqError, IrqResult},
    memory::{AddressSpaceTrait, PhysAddr},
};

use crate::{RuntimeServices, arch::Machine, device::acpi::AcpiIdentityHandler};

/// Stable PCI address tuple.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PciBdf {
    pub segment: u16,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

impl PciBdf {
    /// Create one PCI address tuple.
    pub const fn new(segment: u16, bus: u8, device: u8, function: u8) -> Self {
        Self {
            segment,
            bus,
            device,
            function,
        }
    }

    /// Return one stable namespace key for this PCI function.
    pub fn object_name(self) -> String {
        format!(
            "{:04x}:{:02x}:{:02x}.{}",
            self.segment, self.bus, self.device, self.function
        )
    }
}

#[derive(Debug, Clone, Copy)]
struct PciEcamRegion {
    segment_group: u16,
    bus_start: u8,
    bus_end: u8,
    mmio: MmioRegion,
}

impl PciEcamRegion {
    fn contains(self, bdf: PciBdf) -> bool {
        self.segment_group == bdf.segment
            && (self.bus_start..=self.bus_end).contains(&bdf.bus)
            && bdf.device < 32
            && bdf.function < 8
    }

    fn register_offset(self, bdf: PciBdf, offset: u16) -> Option<usize> {
        if !self.contains(bdf) || offset as usize >= 0x1000 {
            return None;
        }

        let bus_index = usize::from(bdf.bus.checked_sub(self.bus_start)?);
        Some(
            (bus_index << 20)
                | (usize::from(bdf.device) << 15)
                | (usize::from(bdf.function) << 12)
                | usize::from(offset),
        )
    }

    fn read_u8(self, bdf: PciBdf, offset: u16) -> Option<u8> {
        let offset = self.register_offset(bdf, offset)?;
        unsafe { self.mmio.read_u8(offset) }
    }

    fn read_u16(self, bdf: PciBdf, offset: u16) -> Option<u16> {
        let offset = self.register_offset(bdf, offset)?;
        unsafe { self.mmio.read_u16(offset) }
    }

    fn read_u32(self, bdf: PciBdf, offset: u16) -> Option<u32> {
        let offset = self.register_offset(bdf, offset)?;
        unsafe { self.mmio.read_u32(offset) }
    }

    fn write_u8(self, bdf: PciBdf, offset: u16, value: u8) -> bool {
        let Some(offset) = self.register_offset(bdf, offset) else {
            return false;
        };
        unsafe { self.mmio.write_u8(offset, value) }
    }

    fn write_u16(self, bdf: PciBdf, offset: u16, value: u16) -> bool {
        let Some(offset) = self.register_offset(bdf, offset) else {
            return false;
        };
        unsafe { self.mmio.write_u16(offset, value) }
    }

    fn write_u32(self, bdf: PciBdf, offset: u16, value: u32) -> bool {
        let Some(offset) = self.register_offset(bdf, offset) else {
            return false;
        };
        unsafe { self.mmio.write_u32(offset, value) }
    }
}

/// ECAM-backed PCI configuration-space accessor discovered from ACPI MCFG.
pub struct PciEcamConfig {
    regions: Vec<PciEcamRegion>,
}

impl PciEcamConfig {
    /// Discover the platform PCIe ECAM regions from firmware tables.
    pub fn discover_from_firmware() -> IrqResult<Self> {
        let boot = RuntimeServices::boot_info();
        let rsdp = boot.rsdp.ok_or(IrqError::NotSupported)?;
        let handler = AcpiIdentityHandler {
            physical_memory_offset: boot.physical_memory_offset,
        };
        let tables =
            unsafe { AcpiTables::from_rsdp(handler, rsdp) }.map_err(|_| IrqError::NotSupported)?;
        let regions = PciConfigRegions::new(&tables).map_err(|_| IrqError::NotSupported)?;

        let mut ecam_regions = Vec::with_capacity(regions.regions.len());
        for entry in &regions.regions {
            let bus_count = usize::from(entry.bus_number_end - entry.bus_number_start) + 1;
            let phys_base = entry.base_address as usize;
            let virt_base = Machine::phys_to_virt(PhysAddr::new(phys_base))
                .ok_or(IrqError::NotSupported)?
                .as_usize();
            ecam_regions.push(PciEcamRegion {
                segment_group: entry.pci_segment_group,
                bus_start: entry.bus_number_start,
                bus_end: entry.bus_number_end,
                mmio: MmioRegion {
                    phys_base: PhysAddr::new(phys_base),
                    virt_base: libakarin_machine_core::memory::VirtAddr::new(virt_base),
                    size: bus_count << 20,
                },
            });
        }

        if ecam_regions.is_empty() {
            return Err(IrqError::NotSupported);
        }

        Ok(Self {
            regions: ecam_regions,
        })
    }

    fn region(&self, bdf: PciBdf) -> Option<PciEcamRegion> {
        self.regions
            .iter()
            .copied()
            .find(|region| region.contains(bdf))
    }

    /// Return the number of discovered ECAM regions.
    pub fn region_count(&self) -> usize {
        self.regions.len()
    }

    /// Enumerate every present PCI function visible through the discovered
    /// ECAM regions.
    pub fn for_each_present_function(&self, mut visit: impl FnMut(PciBdf)) {
        for region in &self.regions {
            for bus in region.bus_start..=region.bus_end {
                for device in 0..32u8 {
                    let function0 = PciBdf::new(region.segment_group, bus, device, 0);
                    if self.read_u16(function0, 0x00) == u16::MAX {
                        continue;
                    }

                    visit(function0);

                    let header_type = self.read_u8(function0, 0x0e);
                    if header_type & 0x80 == 0 {
                        continue;
                    }

                    for function in 1..8u8 {
                        let bdf = PciBdf::new(region.segment_group, bus, device, function);
                        if self.read_u16(bdf, 0x00) != u16::MAX {
                            visit(bdf);
                        }
                    }
                }
            }
        }
    }
}

/// Kernel-private PCI configuration-space accessor.
pub trait PciConfigAccess: Send + Sync {
    /// Return whether the supplied function falls within one known config
    /// space region.
    fn contains(&self, bdf: PciBdf) -> bool;

    /// Read one 8-bit register from the supplied PCI function.
    fn read_u8(&self, bdf: PciBdf, offset: u16) -> u8;

    /// Read one 16-bit register from the supplied PCI function.
    fn read_u16(&self, bdf: PciBdf, offset: u16) -> u16;

    /// Read one 32-bit register from the supplied PCI function.
    fn read_u32(&self, bdf: PciBdf, offset: u16) -> u32;

    /// Read one 64-bit register from the supplied PCI function.
    fn read_u64(&self, bdf: PciBdf, offset: u16) -> u64 {
        u64::from(self.read_u32(bdf, offset)) | (u64::from(self.read_u32(bdf, offset + 4)) << 32)
    }

    /// Write one 8-bit register on the supplied PCI function.
    fn write_u8(&self, bdf: PciBdf, offset: u16, value: u8);

    /// Write one 16-bit register on the supplied PCI function.
    fn write_u16(&self, bdf: PciBdf, offset: u16, value: u16);

    /// Write one 32-bit register on the supplied PCI function.
    fn write_u32(&self, bdf: PciBdf, offset: u16, value: u32);

    /// Write one 64-bit register on the supplied PCI function.
    fn write_u64(&self, bdf: PciBdf, offset: u16, value: u64) {
        self.write_u32(bdf, offset, value as u32);
        self.write_u32(bdf, offset + 4, (value >> 32) as u32);
    }
}

impl PciConfigAccess for PciEcamConfig {
    fn contains(&self, bdf: PciBdf) -> bool {
        self.region(bdf).is_some()
    }

    fn read_u8(&self, bdf: PciBdf, offset: u16) -> u8 {
        self.region(bdf)
            .and_then(|region| region.read_u8(bdf, offset))
            .unwrap_or(u8::MAX)
    }

    fn read_u16(&self, bdf: PciBdf, offset: u16) -> u16 {
        self.region(bdf)
            .and_then(|region| region.read_u16(bdf, offset))
            .unwrap_or(u16::MAX)
    }

    fn read_u32(&self, bdf: PciBdf, offset: u16) -> u32 {
        self.region(bdf)
            .and_then(|region| region.read_u32(bdf, offset))
            .unwrap_or(u32::MAX)
    }

    fn write_u8(&self, bdf: PciBdf, offset: u16, value: u8) {
        let _ = self
            .region(bdf)
            .is_some_and(|region| region.write_u8(bdf, offset, value));
    }

    fn write_u16(&self, bdf: PciBdf, offset: u16, value: u16) {
        let _ = self
            .region(bdf)
            .is_some_and(|region| region.write_u16(bdf, offset, value));
    }

    fn write_u32(&self, bdf: PciBdf, offset: u16, value: u32) {
        let _ = self
            .region(bdf)
            .is_some_and(|region| region.write_u32(bdf, offset, value));
    }
}
