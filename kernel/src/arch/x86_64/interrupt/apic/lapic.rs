//! Local APIC access helpers.
//!
//! This module hides the xAPIC/x2APIC split behind one small runtime object so
//! the rest of the interrupt subsystem can program timer and IPI delivery
//! without branching on the APIC mode at every call site.

use core::{ptr, slice};

use libakarin_machine_core::{
    interrupt::IpiTarget,
    memory::{AddressSpaceTrait, PhysAddr},
};
use raw_cpuid::CpuId;
use x86::{
    apic::{
        ApicControl, ApicId, DeliveryMode, DeliveryStatus, DestinationMode, DestinationShorthand,
        Icr, Level, TriggerMode,
        x2apic::X2APIC,
        xapic::{
            XAPIC, XAPIC_ICR0, XAPIC_LVT_ERROR, XAPIC_LVT_TIMER, XAPIC_SVR,
            XAPIC_TIMER_CURRENT_COUNT, XAPIC_TIMER_DIV_CONF, XAPIC_TIMER_INIT_COUNT,
        },
    },
    msr::{
        IA32_APIC_BASE, IA32_X2APIC_CUR_COUNT, IA32_X2APIC_DIV_CONF, IA32_X2APIC_ICR,
        IA32_X2APIC_INIT_COUNT, IA32_X2APIC_LVT_ERROR, IA32_X2APIC_LVT_TIMER, IA32_X2APIC_SIVR,
        rdmsr, wrmsr,
    },
};

use super::consts::{APIC_ERROR_INTERRUPT, APIC_SPURIOUS_INTERRUPT, APIC_TIMER_INTERRUPT};
use crate::arch::Machine;

/// Singleton LAPIC instance for the current machine mode.
///
/// The controller is initialized once on the BSP and then re-attached on APs.
static mut LAPIC: Option<LocalApic> = None;

/// Timer delivery modes supported by the local APIC timer.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum TimerMode {
    OneShot,
    Periodic,
    TscDeadline,
}

/// Hardware divide settings for the LAPIC timer input clock.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
#[allow(dead_code)]
pub enum TimerDivide {
    Div1,
    Div2,
    Div4,
    Div8,
    Div16,
    Div32,
    Div64,
    Div128,
}

/// APIC access mode selected during BSP initialization.
enum LocalApicBackend {
    /// Memory-mapped xAPIC register block.
    XApic(XAPIC),
    /// MSR-based x2APIC interface.
    X2Apic(X2APIC),
}

/// Local APIC programming helper for timer, EOI, and IPI delivery.
pub struct LocalApic {
    /// Active programming backend chosen from CPUID feature detection.
    backend: LocalApicBackend,
    /// Virtual base address of the LAPIC MMIO window.
    ///
    /// This is used only for xAPIC mode; x2APIC uses MSRs but still keeps the
    /// mapped base for diagnostics.
    base_vaddr: usize,
}

impl LocalApic {
    /// Replace the low vector byte inside one APIC LVT value.
    fn set_low_vector(value: &mut u64, vector: u8) {
        *value = (*value & !0xff) | vector as u64;
    }

    /// Toggle one APIC LVT bit in place.
    fn set_bit(value: &mut u64, bit: usize, on: bool) {
        if on {
            *value |= 1u64 << bit;
        } else {
            *value &= !(1u64 << bit);
        }
    }

    /// Return the initialized global LAPIC instance.
    pub unsafe fn get<'a>() -> &'a mut LocalApic {
        unsafe {
            (*&raw mut LAPIC)
                .as_mut()
                .expect("Local APIC not initialized")
        }
    }

    /// Detect, attach, and configure the BSP local APIC.
    pub unsafe fn init_bsp() {
        let base_msr = unsafe { rdmsr(IA32_APIC_BASE) };
        let base_paddr = (base_msr as usize) & 0xFFFF_F000;
        let base_vaddr = Machine::phys_to_virt(PhysAddr::new(base_paddr))
            .expect("Invalid LAPIC base")
            .as_usize();
        let has_x2apic = CpuId::new()
            .get_feature_info()
            .is_some_and(|f| f.has_x2apic());

        log::info!(
            "[x86_64/apic] LAPIC base paddr={:#x}, vaddr={:#x}, mode={}",
            base_paddr,
            base_vaddr,
            if has_x2apic { "x2apic" } else { "xapic" }
        );

        let mut local = if has_x2apic {
            let mut apic = X2APIC::new();
            apic.attach();
            LocalApic {
                backend: LocalApicBackend::X2Apic(apic),
                base_vaddr,
            }
        } else {
            let region = unsafe { slice::from_raw_parts_mut(base_vaddr as *mut u32, 1024) };
            let mut apic = XAPIC::new(region);
            apic.attach();
            LocalApic {
                backend: LocalApicBackend::XApic(apic),
                base_vaddr,
            }
        };
        local.configure_vectors();

        unsafe {
            LAPIC = Some(local);
        }
        log::info!("[x86_64/apic] BSP local APIC enabled");
    }

    /// Re-attach and configure the already chosen LAPIC mode on one AP.
    pub unsafe fn init_ap() {
        let local = unsafe { Self::get() };
        match &mut local.backend {
            LocalApicBackend::XApic(apic) => apic.attach(),
            LocalApicBackend::X2Apic(apic) => apic.attach(),
        }
        local.configure_vectors();
        log::info!("[x86_64/apic] AP local APIC enabled");
    }

    /// Program the timer, error, and spurious-interrupt vectors shared by all
    /// CPUs.
    fn configure_vectors(&mut self) {
        let mut timer_lvt = self.read_timer_lvt();
        Self::set_low_vector(&mut timer_lvt, APIC_TIMER_INTERRUPT as u8);
        Self::set_bit(&mut timer_lvt, 16, true);
        self.write_timer_lvt(timer_lvt);

        let mut err_lvt = self.read_lvt_error();
        Self::set_low_vector(&mut err_lvt, APIC_ERROR_INTERRUPT as u8);
        Self::set_bit(&mut err_lvt, 16, false);
        self.write_lvt_error(err_lvt);

        let svr = (1u64 << 8) | APIC_SPURIOUS_INTERRUPT as u64;
        self.write_svr(svr);
    }

    /// Encode one logical APIC id for the active backend.
    fn destination_id(&self, id: u32) -> ApicId {
        match self.backend {
            LocalApicBackend::XApic(_) => ApicId::XApic(id as u8),
            LocalApicBackend::X2Apic(_) => ApicId::X2Apic(id),
        }
    }

    /// Build one interrupt-command register image for the active backend.
    fn for_mode(
        &self,
        vector: u8,
        destination: ApicId,
        shorthand: DestinationShorthand,
        delivery_mode: DeliveryMode,
        destination_mode: DestinationMode,
        delivery_status: DeliveryStatus,
        level: Level,
        trigger_mode: TriggerMode,
    ) -> Icr {
        match self.backend {
            LocalApicBackend::XApic(_) => Icr::for_xapic(
                vector,
                destination,
                shorthand,
                delivery_mode,
                destination_mode,
                delivery_status,
                level,
                trigger_mode,
            ),
            LocalApicBackend::X2Apic(_) => Icr::for_x2apic(
                vector,
                destination,
                shorthand,
                delivery_mode,
                destination_mode,
                delivery_status,
                level,
                trigger_mode,
            ),
        }
    }

    /// Read one xAPIC MMIO register.
    fn read_xapic(&self, offset: u32) -> u32 {
        debug_assert_eq!(offset % 4, 0);
        unsafe { ptr::read_volatile((self.base_vaddr as *const u32).add((offset / 4) as usize)) }
    }

    /// Write one xAPIC MMIO register.
    fn write_xapic(&self, offset: u32, value: u32) {
        debug_assert_eq!(offset % 4, 0);
        unsafe {
            ptr::write_volatile(
                (self.base_vaddr as *mut u32).add((offset / 4) as usize),
                value,
            );
        }
    }

    /// Read the timer LVT entry through the selected backend.
    fn read_timer_lvt(&self) -> u64 {
        match self.backend {
            LocalApicBackend::XApic(_) => self.read_xapic(XAPIC_LVT_TIMER) as u64,
            LocalApicBackend::X2Apic(_) => unsafe { rdmsr(IA32_X2APIC_LVT_TIMER) },
        }
    }

    /// Write the timer LVT entry through the selected backend.
    fn write_timer_lvt(&self, value: u64) {
        match self.backend {
            LocalApicBackend::XApic(_) => self.write_xapic(XAPIC_LVT_TIMER, value as u32),
            LocalApicBackend::X2Apic(_) => unsafe { wrmsr(IA32_X2APIC_LVT_TIMER, value) },
        }
    }

    /// Read the error LVT entry through the selected backend.
    fn read_lvt_error(&self) -> u64 {
        match self.backend {
            LocalApicBackend::XApic(_) => self.read_xapic(XAPIC_LVT_ERROR) as u64,
            LocalApicBackend::X2Apic(_) => unsafe { rdmsr(IA32_X2APIC_LVT_ERROR) },
        }
    }

    /// Write the error LVT entry through the selected backend.
    fn write_lvt_error(&self, value: u64) {
        match self.backend {
            LocalApicBackend::XApic(_) => self.write_xapic(XAPIC_LVT_ERROR, value as u32),
            LocalApicBackend::X2Apic(_) => unsafe { wrmsr(IA32_X2APIC_LVT_ERROR, value) },
        }
    }

    /// Program the spurious-interrupt vector register.
    fn write_svr(&self, value: u64) {
        match self.backend {
            LocalApicBackend::XApic(_) => self.write_xapic(XAPIC_SVR, value as u32),
            LocalApicBackend::X2Apic(_) => unsafe { wrmsr(IA32_X2APIC_SIVR, value) },
        }
    }

    /// Encode the APIC timer divide register bit pattern.
    #[allow(dead_code)]
    fn divide_code(divide: TimerDivide) -> u32 {
        match divide {
            TimerDivide::Div2 => 0b0000,
            TimerDivide::Div4 => 0b0001,
            TimerDivide::Div8 => 0b0010,
            TimerDivide::Div16 => 0b0011,
            TimerDivide::Div32 => 0b1000,
            TimerDivide::Div64 => 0b1001,
            TimerDivide::Div128 => 0b1010,
            TimerDivide::Div1 => 0b1011,
        }
    }

    /// Return the local APIC identifier of the current CPU.
    #[allow(dead_code)]
    pub fn id(&mut self) -> u32 {
        match &self.backend {
            LocalApicBackend::XApic(apic) => apic.id(),
            LocalApicBackend::X2Apic(apic) => apic.id(),
        }
    }

    /// Signal end-of-interrupt to the local APIC.
    pub fn eoi(&mut self) {
        match &mut self.backend {
            LocalApicBackend::XApic(apic) => apic.eoi(),
            LocalApicBackend::X2Apic(apic) => apic.eoi(),
        }
    }

    /// Mask the local timer vector.
    pub fn disable_timer(&mut self) {
        let mut lvt = self.read_timer_lvt();
        Self::set_bit(&mut lvt, 16, true);
        self.write_timer_lvt(lvt);
    }

    /// Unmask the local timer vector.
    pub fn enable_timer(&mut self) {
        let mut lvt = self.read_timer_lvt();
        Self::set_bit(&mut lvt, 16, false);
        self.write_timer_lvt(lvt);
    }

    /// Read the current LAPIC timer countdown value.
    pub fn timer_count(&mut self) -> u32 {
        match self.backend {
            LocalApicBackend::XApic(_) => self.read_xapic(XAPIC_TIMER_CURRENT_COUNT),
            LocalApicBackend::X2Apic(_) => unsafe { rdmsr(IA32_X2APIC_CUR_COUNT) as u32 },
        }
    }

    /// Select one timer mode for subsequent deadlines.
    pub fn set_timer_mode(&mut self, mode: TimerMode) {
        let mut lvt = self.read_timer_lvt();
        Self::set_bit(&mut lvt, 17, false);
        Self::set_bit(&mut lvt, 18, false);
        match mode {
            TimerMode::OneShot => {}
            TimerMode::Periodic => Self::set_bit(&mut lvt, 17, true),
            TimerMode::TscDeadline => Self::set_bit(&mut lvt, 18, true),
        }
        self.write_timer_lvt(lvt);
    }

    /// Program the timer divide ratio.
    #[allow(dead_code)]
    pub fn set_timer_divide(&mut self, divide: TimerDivide) {
        let code = Self::divide_code(divide) as u64;
        match self.backend {
            LocalApicBackend::XApic(_) => self.write_xapic(XAPIC_TIMER_DIV_CONF, code as u32),
            LocalApicBackend::X2Apic(_) => unsafe { wrmsr(IA32_X2APIC_DIV_CONF, code) },
        }
    }

    /// Program the timer initial count register.
    pub fn set_timer_initial(&mut self, initial: u32) {
        match self.backend {
            LocalApicBackend::XApic(_) => self.write_xapic(XAPIC_TIMER_INIT_COUNT, initial),
            LocalApicBackend::X2Apic(_) => unsafe { wrmsr(IA32_X2APIC_INIT_COUNT, initial as u64) },
        }
    }

    /// Send one fixed IPI to the requested logical destination.
    pub fn send_ipi(&mut self, vector: u8, dest: IpiTarget) {
        let (destination, shorthand) = match dest {
            IpiTarget::Current => (self.destination_id(0), DestinationShorthand::Myself),
            IpiTarget::All => (
                self.destination_id(0),
                DestinationShorthand::AllIncludingSelf,
            ),
            IpiTarget::AllExceptCurrent => (
                self.destination_id(0),
                DestinationShorthand::AllExcludingSelf,
            ),
            IpiTarget::Specific(apic_id) => (
                self.destination_id(apic_id as u32),
                DestinationShorthand::NoShorthand,
            ),
        };

        let icr = self.for_mode(
            vector,
            destination,
            shorthand,
            DeliveryMode::Fixed,
            DestinationMode::Physical,
            DeliveryStatus::Idle,
            Level::Assert,
            TriggerMode::Edge,
        );

        unsafe {
            match &mut self.backend {
                LocalApicBackend::XApic(apic) => apic.send_ipi(icr),
                LocalApicBackend::X2Apic(apic) => apic.send_ipi(icr),
            }
        }
    }

    /// Send one INIT IPI to one AP.
    pub fn send_init_ipi(&mut self, apic_id: u32) {
        let destination = self.destination_id(apic_id);
        unsafe {
            match &mut self.backend {
                LocalApicBackend::XApic(apic) => apic.ipi_init(destination),
                LocalApicBackend::X2Apic(apic) => apic.ipi_init(destination),
            }
        }
    }

    /// Broadcast one deasserted INIT sequence when required by legacy flows.
    #[allow(dead_code)]
    pub fn send_init_ipi_deassert(&mut self) {
        unsafe {
            match &mut self.backend {
                LocalApicBackend::XApic(apic) => apic.ipi_init_deassert(),
                LocalApicBackend::X2Apic(apic) => apic.ipi_init_deassert(),
            }
        }
    }

    /// Send one SIPI to one AP bootstrap vector.
    pub fn send_sipi(&mut self, vector: u8, apic_id: u32) {
        let destination = self.destination_id(apic_id);
        unsafe {
            match &mut self.backend {
                LocalApicBackend::XApic(apic) => apic.ipi_startup(destination, vector),
                LocalApicBackend::X2Apic(apic) => apic.ipi_startup(destination, vector),
            }
        }
    }

    /// Return whether the APIC still reports one pending IPI delivery.
    #[allow(dead_code)]
    pub fn ipi_delivery_pending(&self) -> bool {
        match self.backend {
            LocalApicBackend::XApic(_) => ((self.read_xapic(XAPIC_ICR0) >> 12) & 0x1) != 0,
            LocalApicBackend::X2Apic(_) => unsafe { ((rdmsr(IA32_X2APIC_ICR) >> 12) & 0x1) != 0 },
        }
    }
}
