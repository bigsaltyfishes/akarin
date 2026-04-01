use alloc::{collections::BTreeMap, string::ToString, vec::Vec};
use core::{marker::PhantomData, ops::Deref, pin::Pin, ptr::NonNull};

use acpi::{
    AcpiTable as FirmwareTable, AcpiTables, Handle as AcpiHandle, Handler, PciAddress,
    PhysicalMapping, aml,
    sdt::{hpet::HpetTable, madt::Madt},
};
use libakarin_object::{
    Capability, ControlPlane, Handle, ObjectError, Payload, ResourceManager, SyscallDispatch,
    WriteOperation,
};
use libakarin_sync::spin::SpinLock;
use libakarin_syscall::ObjectLifecycleFlags;
use linkme::distributed_slice;
use thiserror::Error;

use super::{
    DeviceProbeError,
    manager::{DEVICE_PROBES, DeviceProbe, DeviceProbeContext, KernelDeviceManager},
};
use crate::arch::guards::IrqSaveGuard;

/// ACPI device errors.
#[derive(Debug, Error)]
pub enum AcpiError {
    #[error("failed to parse ACPI tables")]
    ParseFailed,
    #[error("failed to initialize ACPI object namespace: {0}")]
    NamespaceInitFailed(#[from] ObjectError),
}

#[derive(Clone)]
pub struct AcpiIdentityHandler {
    pub physical_memory_offset: usize,
}

impl AcpiIdentityHandler {
    #[inline]
    fn to_virt(&self, phys_or_virt: usize) -> usize {
        if phys_or_virt >= self.physical_memory_offset {
            phys_or_virt
        } else {
            phys_or_virt + self.physical_memory_offset
        }
    }
}

impl Handler for AcpiIdentityHandler {
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> PhysicalMapping<Self, T> {
        let virt = self.to_virt(physical_address);
        unsafe {
            PhysicalMapping {
                physical_start: physical_address,
                virtual_start: NonNull::new_unchecked(virt as *mut T),
                region_length: size,
                mapped_length: size,
                handler: self.clone(),
            }
        }
    }

    fn unmap_physical_region<T>(_region: &PhysicalMapping<Self, T>) {}

    fn read_u8(&self, address: usize) -> u8 {
        unsafe { *(self.to_virt(address) as *const u8) }
    }

    fn read_u16(&self, address: usize) -> u16 {
        unsafe { *(self.to_virt(address) as *const u16) }
    }

    fn read_u32(&self, address: usize) -> u32 {
        unsafe { *(self.to_virt(address) as *const u32) }
    }

    fn read_u64(&self, address: usize) -> u64 {
        unsafe { *(self.to_virt(address) as *const u64) }
    }

    fn write_u8(&self, address: usize, value: u8) {
        unsafe { *(self.to_virt(address) as *mut u8) = value }
    }

    fn write_u16(&self, address: usize, value: u16) {
        unsafe { *(self.to_virt(address) as *mut u16) = value }
    }

    fn write_u32(&self, address: usize, value: u32) {
        unsafe { *(self.to_virt(address) as *mut u32) = value }
    }

    fn write_u64(&self, address: usize, value: u64) {
        unsafe { *(self.to_virt(address) as *mut u64) = value }
    }

    fn read_io_u8(&self, _port: u16) -> u8 {
        0
    }

    fn read_io_u16(&self, _port: u16) -> u16 {
        0
    }

    fn read_io_u32(&self, _port: u16) -> u32 {
        0
    }

    fn write_io_u8(&self, _port: u16, _value: u8) {}
    fn write_io_u16(&self, _port: u16, _value: u16) {}
    fn write_io_u32(&self, _port: u16, _value: u32) {}

    fn read_pci_u8(&self, _address: PciAddress, _offset: u16) -> u8 {
        0
    }

    fn read_pci_u16(&self, _address: PciAddress, _offset: u16) -> u16 {
        0
    }

    fn read_pci_u32(&self, _address: PciAddress, _offset: u16) -> u32 {
        0
    }

    fn write_pci_u8(&self, _address: PciAddress, _offset: u16, _value: u8) {}
    fn write_pci_u16(&self, _address: PciAddress, _offset: u16, _value: u16) {}
    fn write_pci_u32(&self, _address: PciAddress, _offset: u16, _value: u32) {}

    fn nanos_since_boot(&self) -> u64 {
        0
    }
    fn stall(&self, _microseconds: u64) {}
    fn sleep(&self, _milliseconds: u64) {}

    fn create_mutex(&self) -> AcpiHandle {
        AcpiHandle(0)
    }

    fn acquire(&self, _mutex: AcpiHandle, _timeout: u16) -> Result<(), aml::AmlError> {
        Ok(())
    }

    fn release(&self, _mutex: AcpiHandle) {}
}

/// CPU topology extracted from MADT and cached by the ACPI device.
#[derive(Clone, Debug)]
pub struct CpuTopology {
    lapic_ids: Vec<u32>,
}

impl CpuTopology {
    fn new(lapic_ids: Vec<u32>) -> Option<Self> {
        if lapic_ids.is_empty() {
            return None;
        }
        Some(Self { lapic_ids })
    }

    fn from_madt(mapping: &PhysicalMapping<AcpiIdentityHandler, Madt>) -> Option<Self> {
        let madt = unsafe { Pin::new_unchecked(mapping.virtual_start.as_ref()) };
        let mut lapic_ids = Vec::new();
        for entry in madt.entries() {
            match entry {
                acpi::sdt::madt::MadtEntry::LocalApic(local) if local.flags & 1 != 0 => {
                    lapic_ids.push(local.apic_id as u32);
                }
                acpi::sdt::madt::MadtEntry::LocalX2Apic(local) if local.flags & 1 != 0 => {
                    lapic_ids.push(local.x2apic_id);
                }
                _ => {}
            }
        }
        Self::new(lapic_ids)
    }

    /// Return the number of enabled CPUs described by MADT.
    pub fn cpu_count(&self) -> usize {
        self.lapic_ids.len()
    }

    /// Return the enabled LAPIC IDs in firmware order.
    pub fn lapic_ids(&self) -> &[u32] {
        &self.lapic_ids
    }
}

/// Return an ACPI table mapping before the runtime object graph exists.
pub fn early_table<T>(
    rsdp_addr: Option<usize>,
    physical_memory_offset: usize,
) -> Option<PhysicalMapping<AcpiIdentityHandler, T>>
where
    T: FirmwareTable,
{
    let rsdp_addr = rsdp_addr?;
    let handler = AcpiIdentityHandler {
        physical_memory_offset,
    };
    let tables = unsafe { AcpiTables::from_rsdp(handler, rsdp_addr).ok()? };
    tables.find_table::<T>()
}

/// Execute a closure with an early ACPI table mapping.
pub fn early_table_with<T, R>(
    rsdp_addr: Option<usize>,
    physical_memory_offset: usize,
    f: impl FnOnce(&PhysicalMapping<AcpiIdentityHandler, T>) -> R,
) -> Option<R>
where
    T: FirmwareTable,
{
    let table = early_table::<T>(rsdp_addr, physical_memory_offset)?;
    Some(f(&table))
}

/// Return CPU topology from MADT before formal ACPI device publication.
pub fn early_cpu_topology(
    rsdp_addr: Option<usize>,
    physical_memory_offset: usize,
) -> Option<CpuTopology> {
    early_table_with::<Madt, _>(rsdp_addr, physical_memory_offset, CpuTopology::from_madt).flatten()
}

/// Wrapper payload for ACPI table objects.
pub struct AcpiTable<T, H = AcpiIdentityHandler>
where
    H: Handler,
{
    mapping: PhysicalMapping<H, T>,
}

pub struct AcpiTableMapping<'a, H, T>
where
    H: Handler,
{
    mapping: *const PhysicalMapping<H, T>,
    _marker: PhantomData<&'a PhysicalMapping<H, T>>,
}

impl<'a, H, T> AcpiTableMapping<'a, H, T>
where
    H: Handler,
{
    fn new(mapping: &'a PhysicalMapping<H, T>) -> Self {
        Self {
            mapping,
            _marker: PhantomData,
        }
    }
}

impl<H, T> Deref for AcpiTableMapping<'_, H, T>
where
    H: Handler,
{
    type Target = PhysicalMapping<H, T>;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.mapping }
    }
}

// SAFETY: the wrapper only exposes shared immutable access to firmware table
// mappings that are treated as read-only after publication.
unsafe impl<H, T> Send for AcpiTableMapping<'_, H, T> where H: Handler + Send {}
// SAFETY: the wrapper only dereferences immutable firmware table mappings.
unsafe impl<H, T> Sync for AcpiTableMapping<'_, H, T> where H: Handler + Sync {}

impl<H, T> SyscallDispatch<libakarin_object::ObjectSyscallContext> for AcpiTableMapping<'_, H, T> where
    H: Handler + Send + Sync
{
}

impl<T, H> AcpiTable<T, H>
where
    H: Handler,
{
    pub fn new(mapping: PhysicalMapping<H, T>) -> Self {
        Self { mapping }
    }
}

// SAFETY: ACPI tables are immutable firmware data after mapping and access is
// read-only in current kernel stage.
unsafe impl<T, H> Send for AcpiTable<T, H>
where
    T: Send,
    H: Handler + Send,
{
}

// SAFETY: ACPI table mapping is treated as shared immutable data.
unsafe impl<T, H> Sync for AcpiTable<T, H>
where
    T: Sync,
    H: Handler + Sync,
{
}

impl<T, H> ControlPlane for AcpiTable<T, H>
where
    T: Send + Sync + 'static,
    H: Handler + Send + Sync + 'static,
{
    type ReadGuard<'a>
        = AcpiTableMapping<'a, H, T>
    where
        Self: 'a;
    type WriteGuard<'a>
        = AcpiTableMapping<'a, H, T>
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = AcpiTableMapping<'a, H, T>
    where
        Self: 'a;
    type AgentGuard<'a>
        = AcpiTableMapping<'a, H, T>
    where
        Self: 'a;
    type AdminGuard<'a>
        = AcpiTableMapping<'a, H, T>
    where
        Self: 'a;

    fn read(&self, _interface_caps: u32) -> Self::ReadGuard<'_> {
        AcpiTableMapping::new(&self.mapping)
    }

    fn write(&self, _interface_caps: u32) -> Self::WriteGuard<'_> {
        AcpiTableMapping::new(&self.mapping)
    }

    fn execute(&self, _interface_caps: u32) -> Self::ExecuteGuard<'_> {
        AcpiTableMapping::new(&self.mapping)
    }

    fn agent(&self, _interface_caps: u32) -> Self::AgentGuard<'_> {
        AcpiTableMapping::new(&self.mapping)
    }

    fn admin(&self, _interface_caps: u32) -> Self::AdminGuard<'_> {
        AcpiTableMapping::new(&self.mapping)
    }
}

/// ACPI device object registered under `/Kernel/Device/ACPI`.
pub struct Acpi {
    _acpi_namespace: Handle,
    tables: SpinLock<BTreeMap<&'static str, Handle>, IrqSaveGuard>,
    cpu_topology: SpinLock<Option<CpuTopology>, IrqSaveGuard>,
}

impl Acpi {
    fn new(acpi_namespace: Handle) -> Self {
        Self {
            _acpi_namespace: acpi_namespace,
            tables: SpinLock::new(BTreeMap::new()),
            cpu_topology: SpinLock::new(None),
        }
    }

    fn register_table<T>(
        &self,
        name: &'static str,
        mapping: PhysicalMapping<AcpiIdentityHandler, T>,
    ) -> Result<(), AcpiError>
    where
        T: Send + Sync + 'static,
    {
        let table_super = self
            ._acpi_namespace
            .write_with(|ns: &dyn WriteOperation| {
                ns.add_child(
                    name.to_string(),
                    Capability::AGENT | Capability::ADMIN,
                    Payload::new(AcpiTable::new(mapping)),
                )
            })??;
        table_super.set_lifecycle_flags(ObjectLifecycleFlags::STICKY)?;

        // Keep an AGENT handle for later handle derivation by `get_table`.
        let table_agent = table_super.derive_handle(Capability::AGENT, u32::MAX)?;
        self.tables.lock().insert(name, table_agent);

        // Super handle is not retained by design.
        unsafe { table_super.forget() };
        Ok(())
    }

    fn discover_tables(
        &self,
        rsdp_addr: usize,
        physical_memory_offset: usize,
    ) -> Result<(), AcpiError> {
        let handler = AcpiIdentityHandler {
            physical_memory_offset,
        };
        let tables = unsafe {
            AcpiTables::from_rsdp(handler, rsdp_addr).map_err(|_| AcpiError::ParseFailed)?
        };

        if let Some(madt) = tables.find_table::<Madt>() {
            if let Some(topology) = CpuTopology::from_madt(&madt) {
                log::info!(
                    "[device/acpi] cached CPU topology with {} CPU(s)",
                    topology.cpu_count()
                );
                *self.cpu_topology.lock() = Some(topology);
            }
            self.register_table("MADT", madt)?;
        }
        if let Some(hpet) = tables.find_table::<HpetTable>() {
            self.register_table("HPET", hpet)?;
        }
        Ok(())
    }

    /// Probe ACPI and register it as `/Kernel/Device/ACPI`.
    ///
    /// If `rsdp_addr` is missing, probing is skipped and `Ok(None)` is
    /// returned.
    pub fn probe(
        device_manager: &KernelDeviceManager,
        resource_manager: &ResourceManager,
        rsdp_addr: Option<usize>,
        physical_memory_offset: usize,
    ) -> Result<Option<Handle>, AcpiError> {
        let Some(rsdp_addr) = rsdp_addr else {
            return Ok(None);
        };

        let acpi_namespace = resource_manager.create_namespace("ACPI")?;
        let acpi = Self::new(acpi_namespace);
        let device_handle = device_manager.register_driver("ACPI", acpi)?;
        device_handle.read_cp_with::<Self, _, _>(|acpi_ref| {
            acpi_ref.discover_tables(rsdp_addr, physical_memory_offset)
        })??;
        Ok(Some(device_handle))
    }
}

fn probe_acpi_device(context: &DeviceProbeContext<'_>) -> Result<Option<Handle>, DeviceProbeError> {
    let (rsdp_addr, physical_memory_offset) = context
        .bootloader_manager()
        .firmware_info()
        .map_err(|_| DeviceProbeError::InitializationFailed)?;

    match Acpi::probe(
        context.device_manager(),
        context.resource_manager(),
        rsdp_addr,
        physical_memory_offset,
    ) {
        Ok(handle) => Ok(handle),
        Err(err) => {
            log::warn!("[device/acpi] probe failed: {:?}", err);
            Err(DeviceProbeError::InitializationFailed)
        }
    }
}

#[distributed_slice(DEVICE_PROBES)]
static ACPI_DEVICE_PROBE: DeviceProbe = DeviceProbe {
    name: "ACPI",
    probe: probe_acpi_device,
};

impl ControlPlane for Acpi {
    type ReadGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type WriteGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type AgentGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type AdminGuard<'a>
        = &'a Self
    where
        Self: 'a;

    fn read(&self, _interface_caps: u32) -> Self::ReadGuard<'_> {
        self
    }

    fn write(&self, _interface_caps: u32) -> Self::WriteGuard<'_> {
        self
    }

    fn execute(&self, _interface_caps: u32) -> Self::ExecuteGuard<'_> {
        self
    }

    fn agent(&self, _interface_caps: u32) -> Self::AgentGuard<'_> {
        self
    }

    fn admin(&self, _interface_caps: u32) -> Self::AdminGuard<'_> {
        self
    }
}

impl SyscallDispatch<libakarin_object::ObjectSyscallContext> for Acpi {}
