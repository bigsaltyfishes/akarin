mod apic;
mod hpet;
mod tools;
mod tsc;

use libakarin_core::clock::{
    Clock,
    time::{Duration, Instant},
};
use libakarin_object::ObjectError;

use crate::{ClockSourceManager, RuntimeServices};

pub(super) fn instant_from_ticks(ticks: usize, frequency_hz: u64) -> Instant {
    if frequency_hz == 0 {
        return Instant::new(Duration::from_nanos(0));
    }
    let nanos = (ticks as u128)
        .saturating_mul(1_000_000_000u128)
        .checked_div(frequency_hz as u128)
        .unwrap_or(0) as u64;
    Instant::new(Duration::from_nanos(nanos))
}

fn register_clock(
    manager: &ClockSourceManager,
    name: &str,
    clock: Clock,
) -> Result<(), ObjectError> {
    match manager.register_source(name, clock) {
        Ok(owner) => {
            // `register_source` returns an ADMIN(owner) handle. Dropping it
            // would remove the object from namespace; keep the object alive.
            unsafe { owner.forget() };
            Ok(())
        }
        Err(ObjectError::DuplicateChildName) => Ok(()),
        Err(err) => Err(err),
    }
}

pub fn init_clock_sources() {
    let manager: &ClockSourceManager = RuntimeServices::global()
        .namespaces()
        .clock_source_manager();

    let hpet_clock = hpet::HpetClock::probe();
    if let Some(clock) = hpet_clock.as_ref() {
        if let Err(err) = register_clock(manager, "HPET", Clock::new(clock.clone())) {
            log::warn!("[x86_64/clock] register HPET clock failed: {err:?}");
        } else {
            log::info!("[x86_64/clock] registered clock source: HPET");
        }
    } else {
        log::warn!("[x86_64/clock] HPET unavailable");
    }

    if let Some(tsc_clock) = tsc::TscClock::probe(hpet_clock.as_ref()) {
        if let Err(err) = register_clock(manager, "TSC", Clock::new(tsc_clock)) {
            log::warn!("[x86_64/clock] register TSC clock failed: {err:?}");
        } else {
            log::info!("[x86_64/clock] registered clock source: TSC");
        }
    } else {
        log::warn!("[x86_64/clock] TSC unavailable");
    }

    if let Err(err) = register_clock(
        manager,
        "APIC_PM",
        Clock::new(apic::ApicClock::probe(hpet_clock.as_ref()).unwrap()),
    ) {
        log::warn!("[x86_64/clock] register APIC_PM clock failed: {err:?}");
    } else {
        log::info!("[x86_64/clock] registered clock source: APIC_PM");
    }

    if manager.set_default_by_name("TSC").is_ok() {
        log::info!("[x86_64/clock] default clock set to TSC");
    } else if manager.set_default_by_name("HPET").is_ok() {
        log::info!("[x86_64/clock] default clock set to HPET");
    } else if manager.set_default_by_name("APIC_PM").is_ok() {
        log::info!("[x86_64/clock] default clock set to APIC_PM");
    } else {
        log::warn!("[x86_64/clock] failed to set default clock source");
    }
}
