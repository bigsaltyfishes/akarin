use core::{
    ptr,
    sync::atomic::{AtomicU8, Ordering},
};

use acpi::sdt::hpet::HpetTable;
use libakarin_core::clock::{
    source::{ClockMode, ClockSource},
    time::Instant,
};
use libakarin_machine_core::memory::{AddressSpaceTrait, PhysAddr};
use libakarin_object::{Handle, ObjectPath, Registry};

use crate::{RuntimeServices, arch::Machine, device::acpi::AcpiTable};

const HPET_CAPS_OFFSET: usize = 0x000;
const HPET_CONFIG_OFFSET: usize = 0x010;
const HPET_INT_STATUS_OFFSET: usize = 0x020;
const HPET_COUNTER_OFFSET: usize = 0x0f0;
const HPET_T0_CONFIG_CAP_OFFSET: usize = 0x100;
const HPET_T0_COMPARATOR_OFFSET: usize = 0x108;

const HPET_CONFIG_ENABLE_CNF: u64 = 1 << 0;
const HPET_CONFIG_LEG_RT_CNF: u64 = 1 << 1;

const HPET_TN_INT_TYPE_CNF: u64 = 1 << 1;
const HPET_TN_INT_ENB_CNF: u64 = 1 << 2;
const HPET_TN_TYPE_CNF: u64 = 1 << 3;
const HPET_TN_PER_INT_CAP: u64 = 1 << 4;
const HPET_TN_VAL_SET_CNF: u64 = 1 << 6;

const MODE_ONESHOT: u8 = 0;
const MODE_PERIODIC: u8 = 1;

pub struct HpetClock {
    base_virt: usize,
    counter_clk_period_fs: u64,
    frequency_hz: u64,
    max_ticks: usize,
    mode: AtomicU8,
}

impl Clone for HpetClock {
    fn clone(&self) -> Self {
        Self {
            base_virt: self.base_virt,
            counter_clk_period_fs: self.counter_clk_period_fs,
            frequency_hz: self.frequency_hz,
            max_ticks: self.max_ticks,
            mode: AtomicU8::new(self.mode.load(Ordering::Relaxed)),
        }
    }
}

impl HpetClock {
    fn read_u64(&self, offset: usize) -> u64 {
        unsafe { ptr::read_volatile((self.base_virt + offset) as *const u64) }
    }

    fn write_u64(&self, offset: usize, value: u64) {
        unsafe { ptr::write_volatile((self.base_virt + offset) as *mut u64, value) }
    }

    fn initialize_registers(&self) {
        // Disable main counter before programming.
        let mut config = self.read_u64(HPET_CONFIG_OFFSET);
        config &= !HPET_CONFIG_ENABLE_CNF;
        self.write_u64(HPET_CONFIG_OFFSET, config);

        // Reset counter and timer0 configuration.
        self.write_u64(HPET_COUNTER_OFFSET, 0);

        let mut t0 = self.read_u64(HPET_T0_CONFIG_CAP_OFFSET);
        t0 &=
            !(HPET_TN_INT_TYPE_CNF | HPET_TN_INT_ENB_CNF | HPET_TN_TYPE_CNF | HPET_TN_VAL_SET_CNF);
        self.write_u64(HPET_T0_CONFIG_CAP_OFFSET, t0);
        self.write_u64(HPET_T0_COMPARATOR_OFFSET, 0);
    }

    fn set_counter_enabled(&self, enable: bool) {
        let mut config = self.read_u64(HPET_CONFIG_OFFSET);
        if enable {
            config |= HPET_CONFIG_ENABLE_CNF;
        } else {
            config &= !HPET_CONFIG_ENABLE_CNF;
        }
        self.write_u64(HPET_CONFIG_OFFSET, config);
    }

    fn set_legacy_replacement_if_supported(&self) {
        let caps = self.read_u64(HPET_CAPS_OFFSET);
        if (caps & (1 << 15)) != 0 {
            let mut config = self.read_u64(HPET_CONFIG_OFFSET);
            config |= HPET_CONFIG_LEG_RT_CNF;
            self.write_u64(HPET_CONFIG_OFFSET, config);
        }
    }

    pub fn probe() -> Option<Self> {
        let registry: &'static Registry = RuntimeServices::global().registry();
        let path = ObjectPath::new("/Resource/ACPI/HPET");
        let result = registry.locate_and_then::<Handle, _, _>(None, &path, |handle| {
            handle.read_cp_with::<AcpiTable<HpetTable>, _, _>(|mapping| {
                let table = unsafe { mapping.virtual_start.as_ref() };
                if table.base_address.address_space != 0 {
                    log::warn!(
                        "[x86_64/clock] HPET address space is not system memory: {}",
                        table.base_address.address_space
                    );
                    return None;
                }
                let base_phys = table.base_address.address as usize;
                let base_virt = Machine::phys_to_virt(PhysAddr::new(base_phys))?.as_usize();
                let caps =
                    unsafe { ptr::read_volatile((base_virt + HPET_CAPS_OFFSET) as *const u64) };
                let counter_clk_period_fs = caps >> 32;
                if counter_clk_period_fs == 0 {
                    log::warn!("[x86_64/clock] HPET reports zero counter clock period");
                    return None;
                }

                let frequency_hz = ((1_000_000_000_000_000u128 + counter_clk_period_fs as u128 - 1)
                    / counter_clk_period_fs as u128) as u64;
                if frequency_hz == 0 {
                    return None;
                }

                let count_size_64 = (caps & (1 << 13)) != 0;
                let clock = Self {
                    base_virt,
                    counter_clk_period_fs,
                    frequency_hz,
                    max_ticks: if count_size_64 {
                        usize::MAX
                    } else {
                        u32::MAX as usize
                    },
                    mode: AtomicU8::new(MODE_ONESHOT),
                };
                clock.initialize_registers();
                clock.set_legacy_replacement_if_supported();
                clock.set_counter_enabled(true);
                Some(clock)
            })
        });

        match result {
            Ok(Ok(Some(clock))) => Some(clock),
            Ok(Ok(None)) => None,
            Ok(Err(err)) => {
                log::warn!("[x86_64/clock] failed to read HPET table payload: {err:?}");
                None
            }
            Err(err) => {
                log::warn!("[x86_64/clock] failed to lookup /Resource/ACPI/HPET: {err:?}");
                None
            }
        }
    }
}

impl ClockSource for HpetClock {
    fn is_percpu_clock(&self) -> bool {
        false
    }

    fn ticks(&self) -> usize {
        unsafe { ptr::read_volatile((self.base_virt + HPET_COUNTER_OFFSET) as *const u64) as usize }
    }

    fn frequency(&self) -> u64 {
        self.frequency_hz
    }

    fn max_ticks(&self) -> usize {
        self.max_ticks
    }

    fn now(&self) -> Instant {
        super::instant_from_ticks(self.ticks(), self.frequency())
    }

    fn set_clock_mode(&self, mode: ClockMode) {
        let mut t0 = self.read_u64(HPET_T0_CONFIG_CAP_OFFSET);
        t0 &= !(HPET_TN_TYPE_CNF | HPET_TN_VAL_SET_CNF);

        match mode {
            ClockMode::OneShot => {
                self.mode.store(MODE_ONESHOT, Ordering::Release);
            }
            ClockMode::Periodic => {
                if (t0 & HPET_TN_PER_INT_CAP) == 0 {
                    log::warn!("[x86_64/clock] HPET timer0 does not support periodic mode");
                    self.mode.store(MODE_ONESHOT, Ordering::Release);
                } else {
                    t0 |= HPET_TN_TYPE_CNF | HPET_TN_VAL_SET_CNF;
                    self.mode.store(MODE_PERIODIC, Ordering::Release);
                }
            }
        }
        self.write_u64(HPET_T0_CONFIG_CAP_OFFSET, t0);
    }

    fn set_deadline(&self, deadline: usize) {
        let current = self.ticks();
        let mode = self.mode.load(Ordering::Acquire);
        let value = if mode == MODE_PERIODIC {
            deadline
        } else {
            current.wrapping_add(deadline)
        };
        self.write_u64(HPET_T0_COMPARATOR_OFFSET, value as u64);
    }

    fn set_initial_count(&self, count: usize) {
        self.write_u64(HPET_COUNTER_OFFSET, count as u64);
    }

    fn enable_interrupt(&self) {
        let mut t0 = self.read_u64(HPET_T0_CONFIG_CAP_OFFSET);
        t0 |= HPET_TN_INT_ENB_CNF;
        self.write_u64(HPET_T0_CONFIG_CAP_OFFSET, t0);
    }

    fn disable_interrupt(&self) {
        let mut t0 = self.read_u64(HPET_T0_CONFIG_CAP_OFFSET);
        t0 &= !HPET_TN_INT_ENB_CNF;
        self.write_u64(HPET_T0_CONFIG_CAP_OFFSET, t0);
    }

    fn end_of_interrupt(&self) {
        // If configured level-triggered, clear timer0 interrupt status.
        let t0 = self.read_u64(HPET_T0_CONFIG_CAP_OFFSET);
        if (t0 & HPET_TN_INT_TYPE_CNF) != 0 {
            let status = self.read_u64(HPET_INT_STATUS_OFFSET);
            self.write_u64(HPET_INT_STATUS_OFFSET, status | 1);
        }
    }

    fn enable(&self) {
        self.set_counter_enabled(true);
    }

    fn disable(&self) {
        self.set_counter_enabled(false);
    }
}
