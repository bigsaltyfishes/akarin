use alloc::vec::Vec;
use core::u32;

use libakarin_core::clock::{
    source::{ClockMode, ClockSource},
    time::{Duration, Instant},
};
use raw_cpuid::CpuId;
use x86::msr::{IA32_TSC_DEADLINE, wrmsr};

use super::tsc::TscClock;
use crate::arch::x86_64::{
    clock::{hpet::HpetClock, tools::average_without_some_outliers},
    interrupt::apic::{Apic, TimerMode, lapic::TimerDivide},
};

const CALIBRATION_ROUNDS: usize = 32;
const CALIBRATION_WINDOW_MS: u64 = 5;

pub struct ApicClock {
    frequency_hz: u64,
    tsc_deadline: bool,
}

impl ApicClock {
    pub fn probe(hpet: Option<&HpetClock>) -> Option<Self> {
        Some(Self {
            frequency_hz: Self::calibrate_with_hpet(hpet?),
            tsc_deadline: CpuId::new()
                .get_feature_info()
                .is_some_and(|f| f.has_tsc_deadline()),
        })
    }

    fn calibrate_with_hpet(hpet: &HpetClock) -> u64 {
        if hpet.frequency() == 0 {
            return 0;
        }

        info!(
            "[x86_64/apic] Calibrating APIC PM frequency with HPET ({} Hz)",
            hpet.frequency()
        );
        let lapic = Apic::lapic();
        let mut tick_counts = Vec::with_capacity(CALIBRATION_ROUNDS);
        for _ in 0..CALIBRATION_ROUNDS {
            lapic.set_timer_divide(TimerDivide::Div2);
            lapic.set_timer_initial(u32::MAX);
            lapic.set_timer_mode(TimerMode::OneShot);

            let current = hpet.now();
            hpet.enable();
            lapic.enable_timer();
            while hpet.now().duration_since(current) < Duration::from_millis(CALIBRATION_WINDOW_MS)
            {
                core::hint::spin_loop();
            }
            hpet.disable();
            lapic.disable_timer();

            tick_counts.push(u32::MAX.wrapping_sub(lapic.timer_count()) as u64);
        }

        let average = average_without_some_outliers(&mut tick_counts);
        if average == 0 {
            return 0;
        }

        info!(
            "[x86_64/apic] APIC PM calibration complete: average ticks in {} ms = {}, estimated \
             frequency = {} Hz",
            CALIBRATION_WINDOW_MS,
            average,
            average / CALIBRATION_WINDOW_MS
        );
        average / CALIBRATION_WINDOW_MS
    }
}

impl ClockSource for ApicClock {
    fn is_percpu_clock(&self) -> bool {
        true
    }

    fn ticks(&self) -> usize {
        Apic::lapic().timer_count() as usize
    }

    fn frequency(&self) -> u64 {
        self.frequency_hz
    }

    fn max_ticks(&self) -> usize {
        u32::MAX as usize
    }

    fn now(&self) -> Instant {
        super::instant_from_ticks(self.ticks(), self.frequency())
    }

    fn set_clock_mode(&self, mode: ClockMode) {
        match mode {
            ClockMode::OneShot => {
                let timer_mode = if self.tsc_deadline {
                    TimerMode::TscDeadline
                } else {
                    TimerMode::OneShot
                };
                Apic::lapic().set_timer_mode(timer_mode);
            }
            ClockMode::Periodic => {
                Apic::lapic().set_timer_mode(TimerMode::Periodic);
            }
        }
    }

    fn set_deadline(&self, deadline: usize) {
        if self.tsc_deadline {
            let now = TscClock::read_ticks();
            unsafe {
                wrmsr(IA32_TSC_DEADLINE, (now.wrapping_add(deadline)) as u64);
            }
            return;
        }
        self.set_initial_count(deadline);
    }

    fn set_initial_count(&self, count: usize) {
        Apic::lapic().set_timer_initial(count as u32);
    }

    fn enable_interrupt(&self) {
        Apic::lapic().enable_timer();
    }

    fn disable_interrupt(&self) {
        Apic::lapic().disable_timer();
    }

    fn end_of_interrupt(&self) {
        Apic::lapic().eoi();
    }

    fn enable(&self) {
        Apic::lapic().enable_timer();
    }

    fn disable(&self) {
        if self.tsc_deadline {
            unsafe {
                wrmsr(IA32_TSC_DEADLINE, 0);
            }
        }
        Apic::lapic().disable_timer();
    }
}
