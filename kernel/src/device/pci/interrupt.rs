//! Kernel-internal binder between PCI interrupt capabilities and
//! controller-owned message-interrupt blocks.

use alloc::vec::Vec;

use libakarin_machine_core::interrupt::{
    InterruptControllerTrait, IrqError, IrqResult, MessageIrqBlockTrait, MessageIrqKind,
    MessageIrqRequest,
};
use libakarin_syscall::IrqOpenFlags;

use super::{PciBdf, PciConfigAccess, PciInterruptCapabilities};

/// One allocated PCI interrupt bundle returned by the binder.
pub struct PciInterruptAllocation<B, S> {
    pub block: B,
    pub sessions: Vec<S>,
}

/// Kernel-internal interrupt binder used by future PCI function objects.
pub struct PciInterruptBinder<C: InterruptControllerTrait + 'static, A: PciConfigAccess + 'static> {
    controller: &'static C,
    config: &'static A,
}

impl<C: InterruptControllerTrait + 'static, A: PciConfigAccess + 'static> PciInterruptBinder<C, A> {
    /// Create one binder over the supplied interrupt controller and PCI config
    /// access.
    pub const fn new(controller: &'static C, config: &'static A) -> Self {
        Self { controller, config }
    }

    /// Reserve one MSI message block for the supplied PCI function.
    pub fn allocate_msi(
        &self,
        bdf: PciBdf,
        count: usize,
        cpu_hint: Option<usize>,
    ) -> IrqResult<PciInterruptAllocation<C::MessageBlock, C::Session>> {
        let caps = PciInterruptCapabilities::new(self.config, bdf);
        if !caps.present() {
            return Err(IrqError::InvalidParameter);
        }
        let capability = caps.msi().ok_or(IrqError::NotSupported)?;
        if !capability.supports_count(count) {
            return Err(IrqError::InvalidParameter);
        }

        let block = self.controller.allocate_message_block(MessageIrqRequest {
            kind: MessageIrqKind::Msi,
            count,
            target_cpu: cpu_hint,
            allow_spread: false,
        })?;
        let mut descriptors = Vec::with_capacity(block.len());
        let mut sessions = Vec::with_capacity(block.len());
        for index in 0..block.len() {
            descriptors.push(block.descriptor(index)?);
            sessions.push(block.open_session(index, IrqOpenFlags::SHARED)?);
        }
        caps.disable_all_interrupts();
        capability.enable(&caps, &descriptors)?;
        Ok(PciInterruptAllocation { block, sessions })
    }

    /// Reserve one MSI-X message block for the supplied PCI function.
    pub fn allocate_msix(
        &self,
        bdf: PciBdf,
        count: usize,
        cpu_hint: Option<usize>,
    ) -> IrqResult<PciInterruptAllocation<C::MessageBlock, C::Session>> {
        let caps = PciInterruptCapabilities::new(self.config, bdf);
        if !caps.present() {
            return Err(IrqError::InvalidParameter);
        }
        let capability = caps.msix().ok_or(IrqError::NotSupported)?;
        if count == 0 || count > capability.table_size {
            return Err(IrqError::InvalidParameter);
        }

        let block = self.controller.allocate_message_block(MessageIrqRequest {
            kind: MessageIrqKind::Msix,
            count,
            target_cpu: cpu_hint,
            allow_spread: true,
        })?;
        let mut descriptors = Vec::with_capacity(block.len());
        let mut sessions = Vec::with_capacity(block.len());
        for index in 0..block.len() {
            descriptors.push(block.descriptor(index)?);
            sessions.push(block.open_session(index, IrqOpenFlags::SHARED)?);
        }
        caps.disable_all_interrupts();
        capability.enable(&caps, &descriptors)?;
        Ok(PciInterruptAllocation { block, sessions })
    }

    /// Disable every interrupt mode currently programmed on the supplied
    /// function.
    pub fn disable_interrupts(&self, bdf: PciBdf) -> IrqResult {
        let caps = PciInterruptCapabilities::new(self.config, bdf);
        if !caps.present() {
            return Err(IrqError::InvalidParameter);
        }
        caps.disable_all_interrupts();
        Ok(())
    }
}
