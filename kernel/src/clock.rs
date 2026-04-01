use alloc::{
    boxed::Box,
    string::{String, ToString},
};
use core::sync::atomic::{AtomicUsize, Ordering};

use hashbrown::HashMap;
use libakarin_core::clock::{
    Clock,
    source::{ClockMode, ClockSource},
    time::Instant,
};
use libakarin_machine_core::{cpu::PerCpuTrait, interrupt::InterruptControllerTrait};
use libakarin_object::{Capability, Handle, ObjectError, Payload, WriteOperation};
use libakarin_sync::spin::{SpinLock, SpinRwLock};
use libakarin_syscall::ObjectLifecycleFlags;

use crate::arch::{PerCpu, guards::IrqSaveGuard};

/// Scheduler-facing tick callback invoked from the active timer interrupt path.
pub type ClockTickHook = fn(cpu_id: usize, now: Instant);

/// Manager for objects under `/Kernel/ClockSource`.
///
/// Direct plane:
/// - register and unregister clock sources
/// - manage the current default clock selection
/// - drive the scheduler tick hook and timer ISR wiring
///
/// Capability plane:
/// - discover published clock objects by relative path
/// - delegate clock handles to other subsystems or user space
pub struct ClockSourceManager {
    namespace: Handle,
    sources: SpinRwLock<HashMap<String, Clock>, IrqSaveGuard>,
    default_clock_name: SpinLock<Option<String>, IrqSaveGuard>,
    default_clock: SpinLock<Option<Clock>, IrqSaveGuard>,
    tick_hook: SpinLock<Option<ClockTickHook>, IrqSaveGuard>,
}

const SCHED_TICK_HZ: u64 = 250;
static TIMER_TICK_LOGGED_MASK: AtomicUsize = AtomicUsize::new(0);

impl ClockSourceManager {
    /// Create a manager from a namespace handle with `WRITE` capability.
    pub fn new(namespace: Handle) -> Self {
        Self {
            namespace,
            sources: SpinRwLock::new(HashMap::new()),
            default_clock_name: SpinLock::new(None),
            default_clock: SpinLock::new(None),
            tick_hook: SpinLock::new(None),
        }
    }

    /// Register a clock source object under `/Kernel/ClockSource/{name}`.
    pub fn register_source(&self, name: &str, clock: Clock) -> Result<Handle, ObjectError> {
        let published_name = name.to_string();
        let handle = self.namespace.write_with(|ns: &dyn WriteOperation| {
            ns.add_child(
                published_name.clone(),
                Capability::ADMIN | Capability::AGENT,
                Payload::new(clock.clone()),
            )
        })??;

        let previous = self.sources.write().insert(published_name, clock);
        debug_assert!(
            previous.is_none(),
            "clock source registry replaced an existing entry after successful publish"
        );
        handle.set_lifecycle_flags(ObjectLifecycleFlags::STICKY)?;
        Ok(handle)
    }

    /// Remove a clock source object from `/Kernel/ClockSource/{name}`.
    pub fn unregister_source(&self, name: &str) -> Result<(), ObjectError> {
        self.namespace
            .write_with(|ns: &dyn WriteOperation| ns.remove_child(name, true).map(|_| ()))??;
        self.sources.write().remove(name);

        let mut default_name = self.default_clock_name.lock();
        if default_name.as_deref() == Some(name) {
            *default_name = None;
            *self.default_clock.lock() = None;
        }

        Ok(())
    }

    /// Lookup a clock source object through the discoverable namespace plane.
    pub fn lookup_handle(&self, name: &str) -> Result<Handle, ObjectError> {
        self.namespace.locate(name)
    }

    /// Select the default clock through the discoverable namespace plane.
    pub fn set_default_by_name(&'static self, name: &str) -> Result<(), ObjectError> {
        let clock = self
            .sources
            .read()
            .get(name)
            .cloned()
            .ok_or(ObjectError::ObjectNotFound)?;
        *self.default_clock_name.lock() = Some(name.to_string());
        *self.default_clock.lock() = Some(clock);
        self.setup_timer()?;
        Ok(())
    }

    /// Run one closure against the default clock through the manager's private
    /// direct plane.
    pub fn with_default_clock<F, R>(&self, f: F) -> Result<R, ObjectError>
    where
        F: FnOnce(&Clock) -> Result<R, ObjectError>,
    {
        let clock = self
            .default_clock
            .lock()
            .as_ref()
            .cloned()
            .ok_or(ObjectError::ObjectNotFound)?;
        f(&clock)
    }

    /// Install the scheduler tick hook invoked by the default clock ISR.
    pub fn register_tick_hook(&self, hook: ClockTickHook) {
        *self.tick_hook.lock() = Some(hook);
    }

    /// Remove the scheduler tick hook.
    pub fn unregister_tick_hook(&self) {
        *self.tick_hook.lock() = None;
    }

    /// Bind the default clock's timer ISR on the current CPU.
    pub fn setup_timer(&'static self) -> Result<(), ObjectError> {
        if self.default_clock.lock().is_none() {
            return Err(ObjectError::ObjectNotFound);
        }

        let manager = self;
        crate::RuntimeServices::global()
            .interrupt_controller()
            .set_timer_isr(Box::new(move || {
                if let Err(err) = manager.handle_timer_interrupt_current_cpu() {
                    let cpu_id = PerCpu::id();
                    log::warn!(
                        "[clock cpu={}] timer interrupt dispatch failed: {err:?}",
                        cpu_id
                    );
                }
            }))
            .map_err(|_| ObjectError::InvalidArgument)?;

        self.arm_timer_for_current_cpu()?;
        Ok(())
    }

    fn arm_timer_for_current_cpu(&self) -> Result<(), ObjectError> {
        self.with_default_clock(|clock: &Clock| {
            let frequency = clock.frequency();
            if frequency == 0 {
                return Err(ObjectError::InvalidArgument);
            }
            let interval = (frequency / SCHED_TICK_HZ).max(1) as usize;
            clock.disable();
            clock.set_clock_mode(ClockMode::OneShot);
            clock.enable_interrupt();
            clock.enable();
            clock.set_deadline(interval);
            Ok(())
        })
    }

    fn handle_timer_interrupt_current_cpu(&self) -> Result<(), ObjectError> {
        let cpu_id = PerCpu::id();
        let now = self.with_default_clock(|clock: &Clock| -> Result<Instant, ObjectError> {
            let now = clock.now();
            let frequency = clock.frequency();
            if frequency == 0 {
                return Err(ObjectError::InvalidArgument);
            }
            let interval = (frequency / SCHED_TICK_HZ).max(1) as usize;
            clock.end_of_interrupt();
            clock.set_deadline(interval);
            Ok(now)
        })?;

        let first_tick_for_cpu = if cpu_id >= usize::BITS as usize {
            true
        } else {
            let bit = 1usize << cpu_id;
            let previous = TIMER_TICK_LOGGED_MASK.fetch_or(bit, Ordering::AcqRel);
            previous & bit == 0
        };
        if first_tick_for_cpu {
            log::info!(
                "[clock cpu={}] timer interrupt armed, tick_hz={}",
                cpu_id,
                SCHED_TICK_HZ
            );
        }

        if let Some(hook) = *self.tick_hook.lock() {
            hook(cpu_id, now);
        }

        Ok(())
    }
}
