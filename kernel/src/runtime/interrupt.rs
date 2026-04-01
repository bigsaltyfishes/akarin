use core::{
    cell::UnsafeCell,
    hint::spin_loop,
    mem::MaybeUninit,
    sync::atomic::{AtomicU8, Ordering},
};

use crate::arch::interrupt::InterruptController;

/// Runtime-owned interrupt controller and reschedule-hook registry.
pub struct InterruptRuntime {
    controller_state: AtomicU8,
    controller_slot: UnsafeCell<MaybeUninit<&'static InterruptController>>,
    reschedule_hook_state: AtomicU8,
    reschedule_hook_slot: UnsafeCell<MaybeUninit<fn()>>,
}

unsafe impl Sync for InterruptRuntime {}

impl InterruptRuntime {
    /// Create an empty interrupt runtime registry.
    pub const fn new() -> Self {
        Self {
            controller_state: AtomicU8::new(0),
            controller_slot: UnsafeCell::new(MaybeUninit::uninit()),
            reschedule_hook_state: AtomicU8::new(0),
            reschedule_hook_slot: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    fn wait_until_ready(state: &AtomicU8) {
        while state.load(Ordering::Acquire) == 1 {
            spin_loop();
        }
    }

    /// Install the active interrupt controller exactly once.
    pub fn install_controller(
        &self,
        controller: &'static InterruptController,
    ) -> &'static InterruptController {
        match self
            .controller_state
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                unsafe {
                    (*self.controller_slot.get()).write(controller);
                }
                self.controller_state.store(2, Ordering::Release);
                self.controller()
            }
            Err(1) => {
                Self::wait_until_ready(&self.controller_state);
                self.controller()
            }
            Err(2) => self.controller(),
            Err(_) => unreachable!(),
        }
    }

    /// Return the installed interrupt controller.
    pub fn controller(&self) -> &'static InterruptController {
        Self::wait_until_ready(&self.controller_state);
        assert!(
            self.controller_state.load(Ordering::Acquire) == 2,
            "interrupt controller is not installed"
        );
        unsafe { *(*self.controller_slot.get()).as_ptr() }
    }

    /// Install the local reschedule-IPI hook exactly once.
    pub fn install_reschedule_hook(&self, hook: fn()) -> fn() {
        match self
            .reschedule_hook_state
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                unsafe {
                    (*self.reschedule_hook_slot.get()).write(hook);
                }
                self.reschedule_hook_state.store(2, Ordering::Release);
                self.reschedule_hook()
            }
            Err(1) => {
                Self::wait_until_ready(&self.reschedule_hook_state);
                self.reschedule_hook()
            }
            Err(2) => self.reschedule_hook(),
            Err(_) => unreachable!(),
        }
    }

    /// Return the installed local reschedule-IPI hook.
    pub fn reschedule_hook(&self) -> fn() {
        Self::wait_until_ready(&self.reschedule_hook_state);
        assert!(
            self.reschedule_hook_state.load(Ordering::Acquire) == 2,
            "reschedule IPI hook is not installed"
        );
        unsafe { *(*self.reschedule_hook_slot.get()).as_ptr() }
    }
}
