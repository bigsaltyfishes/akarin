use libakarin_core::clock::time::Duration;

/// Fixed-point virtual time unit used by the EEVDF scheduler.
pub type VirtualTime = u128;

/// Virtual eligible timestamp for a task.
pub type TaskVirtualEligible = VirtualTime;

/// Virtual deadline timestamp for a task.
pub type TaskVirtualDeadline = VirtualTime;

/// Parameters used to derive EEVDF virtual timestamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EevdfParams {
    pub weight: u64,
    pub slice: Duration,
}

impl Default for EevdfParams {
    fn default() -> Self {
        Self {
            weight: 1024,
            slice: Duration::from_millis(5),
        }
    }
}

impl EevdfParams {
    const BASE_WEIGHT: u128 = 1024;

    /// Convert a physical runtime slice into virtual time for the given weight.
    pub fn slice_to_virtual(self) -> VirtualTime {
        self.runtime_to_virtual(self.slice)
    }

    /// Convert an observed runtime duration into virtual time for the given
    /// weight.
    pub fn runtime_to_virtual(self, runtime: Duration) -> VirtualTime {
        let weight = self.weight.max(1) as u128;
        runtime.as_nanos().saturating_mul(Self::BASE_WEIGHT) / weight
    }

    /// Convert a physical runtime duration into scheduler virtual time using
    /// the aggregate runnable weight of the CPU.
    pub fn scheduler_runtime_to_virtual(runtime: Duration, total_weight: u64) -> VirtualTime {
        let total_weight = total_weight.max(1) as u128;
        runtime.as_nanos().saturating_mul(Self::BASE_WEIGHT) / total_weight
    }

    /// Compute the virtual eligible timestamp of one task.
    pub fn eligible(
        self,
        scheduler_vtime: VirtualTime,
        vruntime: VirtualTime,
    ) -> TaskVirtualEligible {
        vruntime.max(scheduler_vtime)
    }

    /// Compute the virtual deadline from an already-updated eligible time.
    pub fn deadline_from_eligible(self, veligible: TaskVirtualEligible) -> TaskVirtualDeadline {
        veligible.saturating_add(self.slice_to_virtual())
    }
}
