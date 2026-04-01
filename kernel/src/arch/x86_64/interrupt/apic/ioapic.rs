//! IOAPIC discovery and redirection-table programming.
//!
//! The APIC controller owns one immutable list of IOAPIC instances discovered
//! from MADT. Each IOAPIC is programmed through one tiny MMIO helper guarded
//! by an IRQ-safe spin lock.

use alloc::vec::Vec;
use core::{fmt, pin::Pin, ptr};

use acpi::sdt::madt::{Madt, MadtEntry};
use libakarin_machine_core::{
    memory::{AddressSpaceTrait, PhysAddr, VirtAddr},
    sync::ScopedGuard,
};
use libakarin_object::{Handle, ObjectPath, Registry};
use libakarin_sync::spin::SpinLock;

use crate::{
    RuntimeServices,
    arch::{Machine, x86_64::sync::IrqSaveGuard},
    device::acpi::AcpiTable,
};

const IOAPIC_REG_VER: u8 = 0x01;
const IOAPIC_REG_TABLE: u8 = 0x10;

const REDIR_VECTOR_MASK: u64 = 0xff;
const REDIR_DELIVERY_MODE_MASK: u64 = 0x7 << 8;
const REDIR_MASKED: u64 = 1 << 16;
const REDIR_DEST_SHIFT: u64 = 56;

/// Raw IOAPIC register-window pointers.
///
/// The hardware exposes one index register and one data register. Higher-level
/// code must serialize access because every transaction is a two-step
/// index-then-data sequence.
struct IoApicInner {
    /// MMIO address of the IOAPIC register selector.
    reg: *mut u32,
    /// MMIO address of the IOAPIC data window.
    data: *mut u32,
}

// SAFETY: IOAPIC MMIO register pointers are fixed hardware mappings and are
// only accessed under `SpinLock` serialization.
unsafe impl Send for IoApicInner {}

impl IoApicInner {
    /// Build one raw IOAPIC accessor from the mapped MMIO base.
    unsafe fn new(base_vaddr: usize) -> Self {
        Self {
            reg: base_vaddr as *mut u32,
            data: (base_vaddr + 0x10) as *mut u32,
        }
    }

    /// Read one IOAPIC register through the selector/data pair.
    fn read_reg(&mut self, reg: u8) -> u32 {
        unsafe {
            ptr::write_volatile(self.reg, reg as u32);
            ptr::read_volatile(self.data)
        }
    }

    /// Write one IOAPIC register through the selector/data pair.
    fn write_reg(&mut self, reg: u8, value: u32) {
        unsafe {
            ptr::write_volatile(self.reg, reg as u32);
            ptr::write_volatile(self.data, value);
        }
    }

    /// Return the highest valid redirection-table entry index.
    fn max_table_entry(&mut self) -> u8 {
        ((self.read_reg(IOAPIC_REG_VER) >> 16) & 0xff) as u8
    }

    /// Read one 64-bit redirection-table entry.
    fn table_entry(&mut self, index: u8) -> u64 {
        let low = self.read_reg(IOAPIC_REG_TABLE + index * 2) as u64;
        let high = self.read_reg(IOAPIC_REG_TABLE + index * 2 + 1) as u64;
        (high << 32) | low
    }

    /// Write one 64-bit redirection-table entry.
    fn set_table_entry(&mut self, index: u8, value: u64) {
        self.write_reg(IOAPIC_REG_TABLE + index * 2, value as u32);
        self.write_reg(IOAPIC_REG_TABLE + index * 2 + 1, (value >> 32) as u32);
    }

    /// Unmask one redirected IRQ line.
    fn enable_irq(&mut self, index: u8) {
        let mut entry = self.table_entry(index);
        entry &= !REDIR_MASKED;
        self.set_table_entry(index, entry);
    }

    /// Mask one redirected IRQ line.
    fn disable_irq(&mut self, index: u8) {
        let mut entry = self.table_entry(index);
        entry |= REDIR_MASKED;
        self.set_table_entry(index, entry);
    }
}

/// An I/O APIC structure.
pub struct IoApic {
    /// Hardware IOAPIC id from MADT.
    id: u8,
    /// First GSI routed by this IOAPIC.
    gsi_start: u32,
    /// Highest valid redirection-table entry.
    max_entry: u8,
    /// Serialized MMIO accessor for the selector/data register pair.
    inner: SpinLock<IoApicInner, ScopedGuard<IrqSaveGuard>>,
}

impl IoApic {
    /// Create one IOAPIC wrapper and mask every redirection-table entry.
    pub fn new(id: u8, base_vaddr: usize, gsi_start: u32) -> Self {
        let mut inner = unsafe { IoApicInner::new(base_vaddr) };
        let max_entry = inner.max_table_entry();
        for i in 0..=max_entry {
            let mut entry = inner.table_entry(i);
            entry &= !(REDIR_VECTOR_MASK | REDIR_DELIVERY_MODE_MASK);
            entry |= REDIR_MASKED;
            entry &= !(0xff << REDIR_DEST_SHIFT);
            inner.set_table_entry(i, entry);
        }
        Self {
            id,
            gsi_start,
            max_entry,
            inner: SpinLock::new(inner),
        }
    }

    /// Mask or unmask one routed GSI on this IOAPIC.
    pub fn toggle(&self, gsi: u32, enabled: bool) {
        let idx = (gsi - self.gsi_start) as u8;
        let mut inner = self.inner.lock();
        if enabled {
            inner.enable_irq(idx);
        } else {
            inner.disable_irq(idx);
        }
    }

    /// Program the vector and destination APIC id for one GSI.
    pub fn map_vector(&self, gsi: u32, vector: u8, dest: u8) {
        let idx = (gsi - self.gsi_start) as u8;
        let mut inner = self.inner.lock();
        let mut entry = inner.table_entry(idx);
        entry &= !REDIR_VECTOR_MASK;
        entry |= vector as u64;
        entry &= !(0xff << REDIR_DEST_SHIFT);
        entry |= (dest as u64) << REDIR_DEST_SHIFT;
        inner.set_table_entry(idx, entry);
    }
}

#[derive(Debug)]
pub struct IoApicList {
    /// MADT-discovered IOAPIC devices ordered by discovery order.
    io_apics: Vec<IoApic>,
}

impl IoApicList {
    /// Run one closure with the MADT mapping if it is available.
    fn with_madt<R>(f: impl FnOnce(Pin<&Madt>) -> R) -> Option<R> {
        let registry: &'static Registry = RuntimeServices::global().registry();
        let path = ObjectPath::new("/Resource/ACPI/MADT");
        match registry.locate_and_then::<Handle, _, _>(None, &path, |handle| {
            handle.read_cp_with::<AcpiTable<Madt>, _, _>(|mapping| {
                let madt = unsafe { mapping.virtual_start.as_ref() };
                let madt = unsafe { Pin::new_unchecked(madt) };
                f(madt)
            })
        }) {
            Ok(Ok(value)) => Some(value),
            Ok(Err(err)) => {
                log::warn!("[x86_64/apic] failed to read /Resource/ACPI/MADT: {err:?}");
                None
            }
            Err(err) => {
                log::warn!("[x86_64/apic] failed to lookup /Resource/ACPI/MADT: {err:?}");
                None
            }
        }
    }

    /// Discover every IOAPIC described by MADT.
    pub fn new() -> Self {
        let mut io_apics = Vec::new();
        let Some(()) = Self::with_madt(|madt| {
            for entry in madt.entries() {
                match entry {
                    MadtEntry::IoApic(entry) => {
                        let io_apic_id = entry.io_apic_id;
                        let io_apic_address = entry.io_apic_address;
                        let gsi_base = entry.global_system_interrupt_base;
                        let base_vaddr =
                            Machine::phys_to_virt(PhysAddr::new(io_apic_address as usize))
                                .unwrap_or(VirtAddr::new(io_apic_address as usize))
                                .as_usize();
                        io_apics.push(IoApic::new(io_apic_id, base_vaddr, gsi_base));
                        log::info!(
                            "[x86_64/apic] IOAPIC id={} gsi_base={} vaddr={:#x}",
                            io_apic_id,
                            gsi_base,
                            base_vaddr
                        );
                    }
                    MadtEntry::LocalApicAddressOverride(_) => {}
                    _ => {}
                }
            }
        }) else {
            log::warn!("[x86_64/apic] MADT unavailable, IOAPIC list empty");
            return Self {
                io_apics: Vec::new(),
            };
        };

        if io_apics.is_empty() {
            log::warn!("[x86_64/apic] no IOAPIC entries in MADT");
        }

        log::info!("[x86_64/apic] detected {} IOAPIC(s)", io_apics.len());

        Self { io_apics }
    }

    /// Return the IOAPIC that routes the supplied GSI.
    pub fn find(&self, gsi: u32) -> Option<&IoApic> {
        self.io_apics
            .iter()
            .find(|i| i.gsi_start <= gsi && gsi <= i.gsi_start + i.max_entry as u32)
    }

    /// Return the highest GSI reachable through the discovered IOAPIC set.
    pub fn max_gsi(&self) -> usize {
        self.io_apics
            .iter()
            .map(|i| (i.gsi_start + i.max_entry as u32) as usize)
            .max()
            .unwrap_or(0)
    }
}

impl fmt::Debug for IoApic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IoApic")
            .field("id", &self.id)
            .field("gsi_start", &self.gsi_start)
            .field("max_entry", &self.max_entry)
            .finish()
    }
}
