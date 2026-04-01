use crate::clock::time::{Duration, Instant};

/// System Clock Mode
#[repr(u8)]
#[derive(Debug, Eq, PartialEq)]
pub enum ClockMode {
    OneShot,
    Periodic,
}

/// Trait for system clock source
pub trait ClockSource {
    /// Check if the clock is a per-cpu clock
    fn is_percpu_clock(&self) -> bool;

    /// Get the current ticks
    fn ticks(&self) -> usize;

    /// Get the frequency of the clock
    fn frequency(&self) -> u64;

    /// Get the maximum ticks that the clock counter can record
    fn max_ticks(&self) -> usize;

    /// Get the current instant from the clock
    fn now(&self) -> Instant;

    /// Wait for the specified duration
    fn wait(&self, duration: Duration) {
        let start = self.now();
        while self.now() - start < duration {
            core::hint::spin_loop();
        }
    }

    /// Set the mode of the clock
    fn set_clock_mode(&self, mode: ClockMode);

    /// Set the deadline of the clock
    fn set_deadline(&self, deadline: usize);

    /// Set the initial count of the clock counter
    fn set_initial_count(&self, count: usize);

    /// Enable interrupt
    fn enable_interrupt(&self);

    /// Disable interrupt
    fn disable_interrupt(&self);

    /// End of interrupt
    fn end_of_interrupt(&self);

    /// Enable the clock
    fn enable(&self);

    /// Disable the clock
    fn disable(&self);
}
