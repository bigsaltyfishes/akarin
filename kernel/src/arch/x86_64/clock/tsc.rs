use alloc::vec::Vec;
use core::arch::x86_64::{__rdtscp, _mm_lfence, _rdtsc};

use libakarin_core::clock::{
    source::{ClockMode, ClockSource},
    time::{Duration, Instant},
};
use log::info;
use raw_cpuid::CpuId;
use x86::msr::{IA32_TSC_DEADLINE, wrmsr};

use super::hpet::HpetClock;
use crate::arch::x86_64::{
    Apic, clock::tools::average_without_some_outliers, interrupt::apic::TimerMode,
};

const CALIBRATION_ROUNDS: usize = 32;
const CALIBRATION_WINDOW_MS: u64 = 5;

pub struct TscClock {
    frequency_hz: u64,
    has_rdtscp: bool,
    has_tsc_deadline: bool,
}

impl TscClock {
    pub fn probe(hpet: Option<&HpetClock>) -> Option<Self> {
        let cpuid = CpuId::new();
        let has_tsc = cpuid.get_feature_info().is_some_and(|f| f.has_tsc());
        if !has_tsc {
            return None;
        }
        let has_rdtscp = cpuid
            .get_extended_processor_and_feature_identifiers()
            .is_some_and(|f| f.has_rdtscp());
        let has_tsc_deadline = cpuid
            .get_feature_info()
            .is_some_and(|f| f.has_tsc_deadline());

        let frequency_hz = Self::calibrate_with_hpet(hpet?);
        Some(Self {
            frequency_hz,
            has_rdtscp,
            has_tsc_deadline,
        })
    }

    pub fn read_ticks() -> usize {
        unsafe {
            _mm_lfence();
            _rdtsc() as usize
        }
    }

    fn read_ticks_fast(&self) -> usize {
        if self.has_rdtscp {
            let mut aux = 0u32;
            unsafe { __rdtscp(&mut aux) as usize }
        } else {
            Self::read_ticks()
        }
    }

    fn calibrate_with_hpet(hpet: &HpetClock) -> u64 {
        if hpet.frequency() == 0 {
            return 0;
        }

        info!(
            "[x86_64/tsc] Calibrating TSC frequency with HPET ({} Hz)",
            hpet.frequency()
        );
        let mut tick_counts = Vec::with_capacity(CALIBRATION_ROUNDS);
        for _ in 0..CALIBRATION_ROUNDS {
            let begin = Self::read_ticks() as u64;
            let current = hpet.now();
            hpet.enable();
            while hpet.now().duration_since(current) < Duration::from_millis(CALIBRATION_WINDOW_MS)
            {
                core::hint::spin_loop();
            }
            hpet.disable();
            tick_counts.push((Self::read_ticks() as u64).wrapping_sub(begin));
        }

        let average = average_without_some_outliers(&mut tick_counts);
        if average == 0 {
            return 0;
        }

        info!(
            "[x86_64/tsc] TSC calibration complete: average ticks in {} ms = {}, estimated \
             frequency = {} Hz",
            CALIBRATION_WINDOW_MS,
            average,
            average / CALIBRATION_WINDOW_MS
        );
        average / CALIBRATION_WINDOW_MS
    }
}

impl ClockSource for TscClock {
    fn is_percpu_clock(&self) -> bool {
        true
    }

    fn ticks(&self) -> usize {
        self.read_ticks_fast()
    }

    fn frequency(&self) -> u64 {
        self.frequency_hz
    }

    fn max_ticks(&self) -> usize {
        usize::MAX
    }

    fn now(&self) -> Instant {
        super::instant_from_ticks(self.ticks(), self.frequency())
    }

    fn set_clock_mode(&self, _mode: ClockMode) {
        if self.has_tsc_deadline {
            Apic::lapic().set_timer_mode(TimerMode::TscDeadline);
        }
    }

    fn set_deadline(&self, deadline: usize) {
        if self.has_tsc_deadline {
            let now = self.read_ticks_fast();
            unsafe {
                wrmsr(IA32_TSC_DEADLINE, now.wrapping_add(deadline) as u64);
            }
        }
    }

    fn set_initial_count(&self, _count: usize) {}

    fn enable_interrupt(&self) {
        if self.has_tsc_deadline {
            Apic::lapic().enable_timer();
        }
    }

    fn disable_interrupt(&self) {
        if self.has_tsc_deadline {
            unsafe {
                wrmsr(IA32_TSC_DEADLINE, 0);
            }
            Apic::lapic().disable_timer();
        }
    }

    fn end_of_interrupt(&self) {
        if self.has_tsc_deadline {
            Apic::lapic().eoi();
        }
    }

    fn enable(&self) {
        if self.has_tsc_deadline {
            Apic::lapic().enable_timer();
        }
    }

    fn disable(&self) {
        if self.has_tsc_deadline {
            unsafe {
                wrmsr(IA32_TSC_DEADLINE, 0);
            }
            Apic::lapic().disable_timer();
        }
    }
}
