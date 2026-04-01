use core::{
    cell::UnsafeCell,
    hint::spin_loop,
    mem::MaybeUninit,
    sync::atomic::{AtomicU8, Ordering},
};

use libakarin_machine_core::interrupt::IrqResult;

use crate::{
    arch::interrupt::{InterruptController, IrqMessageBlock, IrqSession},
    device::pci::{
        PciBarSummary, PciBdf, PciCapabilitySummary, PciEcamConfig, PciInterruptAllocation,
        PciInterruptBinder, PciInterruptCapabilities,
    },
};

type InstalledPciInterruptBinder = PciInterruptBinder<InterruptController, PciEcamConfig>;

/// Runtime-owned PCI ECAM configuration access and interrupt binder registry.
pub struct PciRuntime {
    state: AtomicU8,
    config_slot: UnsafeCell<MaybeUninit<&'static PciEcamConfig>>,
    binder_slot: UnsafeCell<MaybeUninit<&'static InstalledPciInterruptBinder>>,
}

unsafe impl Sync for PciRuntime {}

impl PciRuntime {
    /// Create an empty PCI runtime registry.
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(0),
            config_slot: UnsafeCell::new(MaybeUninit::uninit()),
            binder_slot: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    fn wait_until_ready(&self) {
        while self.state.load(Ordering::Acquire) == 1 {
            spin_loop();
        }
    }

    /// Install the active PCI configuration access and interrupt binder.
    pub fn install(
        &self,
        config: &'static PciEcamConfig,
        binder: &'static InstalledPciInterruptBinder,
    ) -> &'static InstalledPciInterruptBinder {
        match self
            .state
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                unsafe {
                    (*self.config_slot.get()).write(config);
                    (*self.binder_slot.get()).write(binder);
                }
                self.state.store(2, Ordering::Release);
                self.binder()
            }
            Err(1) => {
                self.wait_until_ready();
                self.binder()
            }
            Err(2) => self.binder(),
            Err(_) => unreachable!(),
        }
    }

    /// Return whether one PCI runtime instance has been installed.
    pub fn is_installed(&self) -> bool {
        self.state.load(Ordering::Acquire) == 2
    }

    /// Return the installed PCI ECAM configuration access.
    fn config(&self) -> &'static PciEcamConfig {
        self.wait_until_ready();
        assert!(
            self.state.load(Ordering::Acquire) == 2,
            "pci runtime is not installed"
        );
        unsafe { *(*self.config_slot.get()).as_ptr() }
    }

    /// Return the installed PCI interrupt binder.
    fn binder(&self) -> &'static InstalledPciInterruptBinder {
        self.wait_until_ready();
        assert!(
            self.state.load(Ordering::Acquire) == 2,
            "pci runtime is not installed"
        );
        unsafe { *(*self.binder_slot.get()).as_ptr() }
    }

    /// Return one interrupt-capability view over the supplied PCI function.
    pub fn interrupt_capabilities(
        &self,
        bdf: PciBdf,
    ) -> PciInterruptCapabilities<'_, PciEcamConfig> {
        PciInterruptCapabilities::new(self.config(), bdf)
    }

    /// Return one compact BAR summary over the supplied PCI function.
    pub fn bar_summary(&self, bdf: PciBdf) -> PciBarSummary {
        self.interrupt_capabilities(bdf).bar_summary()
    }

    /// Return one compact capability summary over the supplied PCI function.
    pub fn capability_summary(&self, bdf: PciBdf) -> PciCapabilitySummary {
        self.interrupt_capabilities(bdf).capability_summary()
    }

    /// Allocate and program MSI for the supplied PCI function.
    pub fn allocate_msi(
        &self,
        bdf: PciBdf,
        count: usize,
        cpu_hint: Option<usize>,
    ) -> IrqResult<PciInterruptAllocation<IrqMessageBlock, IrqSession>> {
        self.binder().allocate_msi(bdf, count, cpu_hint)
    }

    /// Allocate and program MSI-X for the supplied PCI function.
    pub fn allocate_msix(
        &self,
        bdf: PciBdf,
        count: usize,
        cpu_hint: Option<usize>,
    ) -> IrqResult<PciInterruptAllocation<IrqMessageBlock, IrqSession>> {
        self.binder().allocate_msix(bdf, count, cpu_hint)
    }

    /// Disable every interrupt mode programmed on the supplied PCI function.
    pub fn disable_interrupts(&self, bdf: PciBdf) -> IrqResult {
        self.binder().disable_interrupts(bdf)
    }
}
