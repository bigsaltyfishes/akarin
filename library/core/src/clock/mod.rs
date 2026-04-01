use alloc::sync::Arc;

use libakarin_object::{ControlPlane, SyscallDispatch};

pub mod source;
pub mod time;

/// Type-erased clock object used for namespace registration and late binding.
#[derive(Clone)]
pub struct Clock(Arc<dyn source::ClockSource + Send + Sync>);

impl Clock {
    /// Wrap one concrete clock source into the shared clock object.
    pub fn new<T>(inner: T) -> Self
    where
        T: source::ClockSource + Send + Sync + 'static,
    {
        Self(Arc::new(inner))
    }
}

impl source::ClockSource for Clock {
    fn is_percpu_clock(&self) -> bool {
        self.0.is_percpu_clock()
    }

    fn ticks(&self) -> usize {
        self.0.ticks()
    }

    fn frequency(&self) -> u64 {
        self.0.frequency()
    }

    fn max_ticks(&self) -> usize {
        self.0.max_ticks()
    }

    fn now(&self) -> time::Instant {
        self.0.now()
    }

    fn set_clock_mode(&self, mode: source::ClockMode) {
        self.0.set_clock_mode(mode);
    }

    fn set_deadline(&self, deadline: usize) {
        self.0.set_deadline(deadline);
    }

    fn set_initial_count(&self, count: usize) {
        self.0.set_initial_count(count);
    }

    fn enable_interrupt(&self) {
        self.0.enable_interrupt();
    }

    fn disable_interrupt(&self) {
        self.0.disable_interrupt();
    }

    fn end_of_interrupt(&self) {
        self.0.end_of_interrupt();
    }

    fn enable(&self) {
        self.0.enable();
    }

    fn disable(&self) {
        self.0.disable();
    }
}

impl ControlPlane for Clock {
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

impl SyscallDispatch<libakarin_object::ObjectSyscallContext> for Clock {}
