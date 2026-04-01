use alloc::boxed::Box;
use core::{future::Future, pin::Pin};

use bitflags::bitflags;
use libakarin_syscall::{IrqAckDisposition, IrqOpenFlags, IrqWaitFlags};
use thiserror::Error;

/// Type alias for one kernel IRQ handler closure.
pub type IrqHandler = Box<dyn Fn() + Send + Sync>;

/// Abstract IPI reasons understood by one interrupt controller backend.
#[derive(Debug, Copy, Clone)]
pub enum IpiReason {
    Reschedule,
    FlushTlb,
    Panic,
    Mailbox,
    Custom(usize),
}

/// Abstract IPI targets understood by one interrupt controller backend.
#[derive(Debug, Copy, Clone)]
pub enum IpiTarget {
    Current,
    All,
    AllExceptCurrent,
    Specific(usize),
}

/// Common interrupt-controller errors surfaced to the kernel.
#[derive(Error, Debug)]
pub enum IrqError {
    #[error("invalid IRQ id: {0}")]
    InvalidIrq(usize),
    #[error("invalid parameter")]
    InvalidParameter,
    #[error("out of resources")]
    OutOfResources,
    #[error("operation not supported")]
    NotSupported,
    #[error("operation would block")]
    WouldBlock,
    #[error("session has been closed")]
    Closed,
    #[error("finite deadlines are not supported")]
    DeadlineUnsupported,
}

/// Canonical result type returned by machine IRQ backends.
pub type IrqResult<T = ()> = Result<T, IrqError>;

/// Stable controller metadata used by the kernel object layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterruptControllerInfo {
    pub line_count: usize,
    pub local_irq_base: usize,
    pub cpu_count: usize,
    pub feature_bits: usize,
}

bitflags! {
    /// Shared line-state flags exported by one machine IRQ line backend.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct IrqLineFlags: usize {
        const ADMIN_ENABLED = 1 << 0;
        const DELIVERY_BLOCKED = 1 << 1;
        const HAS_CURRENT_EPOCH = 1 << 2;
    }
}

/// Stable state snapshot exported by one machine IRQ line backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrqLineState {
    pub irq: usize,
    pub flags: IrqLineFlags,
    pub session_count: usize,
    pub enabled_session_count: usize,
    pub current_epoch: u64,
}

/// Stable state snapshot exported by one machine IRQ session backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrqSessionState {
    pub irq: usize,
    pub session_id: usize,
    pub flags: IrqOpenFlags,
    pub enabled: bool,
    pub closed: bool,
    pub inflight_epoch: Option<u64>,
    pub pending_count: usize,
    pub last_acked_epoch: u64,
}

/// One shared IRQ delivery observed by one user-visible session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrqDelivery {
    pub irq: usize,
    pub epoch: u64,
    pub pending_count: usize,
    pub flags: IrqOpenFlags,
}

/// Boxed future returned by one user-visible IRQ wait operation.
pub type IrqWaitFuture = Pin<Box<dyn Future<Output = IrqResult<IrqDelivery>> + Send>>;

/// Message-interrupt kind allocated from one interrupt controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageIrqKind {
    /// PCI MSI block with one contiguous vector run on one target CPU.
    Msi,
    /// PCI MSI-X block with independently routable entries.
    Msix,
}

bitflags! {
    /// Stable flags reported by one allocated message-interrupt block.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct MessageIrqFlags: usize {
        const CONTIGUOUS = 1 << 0;
        const PER_ENTRY_ROUTE = 1 << 1;
    }
}

/// Allocation request for one controller-owned message interrupt block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageIrqRequest {
    pub kind: MessageIrqKind,
    pub count: usize,
    pub target_cpu: Option<usize>,
    pub allow_spread: bool,
}

/// Stable metadata exported by one allocated message interrupt block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageIrqInfo {
    pub kind: MessageIrqKind,
    pub count: usize,
    pub flags: MessageIrqFlags,
}

/// One machine-programmable message-interrupt descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageIrqDescriptor {
    pub address_lo: u32,
    pub address_hi: u32,
    pub data: u32,
    pub cpu_id: usize,
    pub vector: usize,
}

/// Backend contract for one allocated message-interrupt block.
pub trait MessageIrqBlockTrait: Send + Sync {
    type Session: IrqSessionTrait;

    /// Return the message-interrupt kind backed by this block.
    fn kind(&self) -> MessageIrqKind;

    /// Return the number of descriptors contained in this block.
    fn len(&self) -> usize;

    /// Return stable block metadata.
    fn info(&self) -> IrqResult<MessageIrqInfo>;

    /// Return the machine descriptor for the supplied block entry.
    fn descriptor(&self, index: usize) -> IrqResult<MessageIrqDescriptor>;

    /// Open one shared IRQ session for the supplied entry.
    fn open_session(&self, index: usize, flags: IrqOpenFlags) -> IrqResult<Self::Session>;

    /// Retarget one block entry and return its updated descriptor.
    fn retarget(&self, index: usize, cpu_id: usize) -> IrqResult<MessageIrqDescriptor>;

    /// Close every session currently attached to this block.
    fn close_sessions(&self);
}

/// Backend contract for one discoverable IRQ line.
pub trait IrqLineTrait: Send + Sync {
    type Session: IrqSessionTrait;

    /// Return the stable IRQ identifier of this line.
    fn irq_id(&self) -> usize;

    /// Return a stable state snapshot for the object/control plane.
    fn state(&self) -> IrqResult<IrqLineState>;

    /// Open one shared session on this line.
    fn open_session(&self, flags: IrqOpenFlags) -> IrqResult<Self::Session>;

    /// Retarget this line to the supplied CPU, if supported.
    fn set_destination(&self, cpu_id: usize) -> IrqResult {
        let _ = cpu_id;
        Err(IrqError::NotSupported)
    }
}

/// Backend contract for one shared IRQ session.
pub trait IrqSessionTrait: Send + Sync {
    /// Return the stable session identifier within one line.
    fn session_id(&self) -> usize;

    /// Return the owning IRQ identifier.
    fn irq_id(&self) -> usize;

    /// Return the open flags used for this shared session.
    fn flags(&self) -> IrqOpenFlags;

    /// Return a stable session-state snapshot for the control plane.
    fn state(&self) -> IrqSessionState;

    /// Enable or disable delivery to this shared session.
    fn set_enabled(&self, enabled: bool);

    /// Close this shared session. Implementations must tolerate repeated calls.
    fn close(&self);

    /// Wait until one delivery becomes visible to this session.
    fn wait(&self, deadline: usize, flags: IrqWaitFlags) -> IrqWaitFuture;

    /// Acknowledge one delivered epoch.
    fn ack(&self, epoch: u64, disposition: IrqAckDisposition) -> IrqResult<usize>;
}

/// Machine-visible interrupt controller contract.
pub trait InterruptControllerTrait: Send + Sync {
    type Line: IrqLineTrait<Session = Self::Session>;
    type Session: IrqSessionTrait;
    type MessageBlock: MessageIrqBlockTrait<Session = Self::Session>;

    /// Return the stable kernel/device name of this controller.
    fn controller_name(&self) -> &'static str;

    /// Return stable controller metadata for the kernel object layer.
    fn controller_info(&self) -> InterruptControllerInfo;

    /// Resolve one discoverable IRQ line object.
    fn line(&'static self, irq: usize) -> IrqResult<Self::Line>;

    /// Allocate one message-interrupt block for MSI/MSI-X style delivery.
    fn allocate_message_block(
        &'static self,
        request: MessageIrqRequest,
    ) -> IrqResult<Self::MessageBlock>;

    /// Wait for the next interrupt.
    fn wait_for_interrupt(&self);

    /// Check whether the supplied IRQ identifier is valid.
    fn is_valid_irq(&self, irq: usize) -> bool;

    /// Enable the controller on the current CPU.
    fn enable_ic(&self) -> IrqResult;

    /// Disable the controller on the current CPU.
    fn disable_ic(&self) -> IrqResult;

    /// Signal end-of-interrupt for the current vector.
    fn end_of_interrupt(&self) -> IrqResult;

    /// Return whether interrupts are currently enabled.
    fn is_interrupt_enabled(&self) -> bool;

    /// Mask one physical IRQ line under kernel control.
    fn mask_irq(&self, irq: usize) -> IrqResult;

    /// Unmask one physical IRQ line under kernel control.
    fn unmask_irq(&self, irq: usize) -> IrqResult;

    /// Register one kernel IRQ handler on the supplied IRQ.
    fn register_irq_handler(&self, irq: usize, handler: IrqHandler) -> IrqResult;

    /// Unregister the kernel IRQ handler on the supplied IRQ.
    fn unregister_irq_handler(&self, irq: usize) -> IrqResult;

    /// Install the current CPU's reserved timer ISR.
    fn set_timer_isr(&self, _handler: IrqHandler) -> IrqResult {
        Err(IrqError::NotSupported)
    }

    /// Clear the current CPU's reserved timer ISR.
    fn clear_timer_isr(&self) -> IrqResult {
        Err(IrqError::NotSupported)
    }

    /// Handle one incoming IRQ on this controller.
    fn handle_irq(&self, irq: usize) -> IrqResult;

    /// Send one inter-processor interrupt.
    fn send_ipi(&self, reason: IpiReason, dest: IpiTarget) -> IrqResult;
}
