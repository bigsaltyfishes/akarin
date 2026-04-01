//! APIC-local IRQ state machines and vector allocation helpers.
//!
//! The public APIC controller delegates almost all mutable IRQ bookkeeping to
//! this module:
//! - [`SharedIrqLine`] multiplexes kernel handlers and user sessions for one
//!   logical IRQ line;
//! - [`SharedIrqManager`] owns contiguous sets of such lines;
//! - [`PerCpuIrqManager`] allocates LAPIC-local vectors on one CPU;
//! - [`IrqSessionCore`] implements the user-visible shared-session state
//!   machine.

use alloc::{sync::Arc, vec::Vec};
use core::{
    ops::Range,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use hashbrown::HashMap;
use libakarin_machine_core::interrupt::{
    IrqDelivery, IrqError, IrqHandler, IrqLineFlags, IrqLineState, IrqResult, IrqSessionState,
    MessageIrqDescriptor, MessageIrqFlags, MessageIrqKind,
};
use libakarin_sync::{
    asynchronous::Event,
    collections::{ConcurrentQueue, IdAllocator, PushError},
    spin::SpinLock,
};
use libakarin_syscall::{IrqAckDisposition, IrqOpenFlags};

use crate::arch::guards::IrqSaveGuard;

/// Pending epochs kept per session before userspace drains and ACKs them.
///
/// The queue lives on the IRQ hot path, so it must never allocate while
/// handling one interrupt delivery. Once this bounded backlog overflows the
/// session reports `OutOfResources` and stops accepting new epochs until the
/// caller tears it down.
const IRQ_SESSION_PENDING_CAPACITY: usize = 256;

/// APIC-private trigger mode carried by one shared line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineTriggerMode {
    Edge,
    Level,
}

/// Result of one session ACK against one shared line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrqAckOutcome {
    pub remaining_participants: usize,
    pub pending_count: usize,
    pub should_unmask: bool,
    pub claimed: bool,
}

/// Action the physical controller must perform after handling one IRQ delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqHandleAction {
    None,
    MaskLine,
}

struct SharedIrqSessionSlot {
    /// Optional kernel-side handler attached to this session slot.
    handler: Option<IrqHandler>,
    /// Optional user-visible shared session attached to this slot.
    user: Option<Arc<IrqSessionCore>>,
}

impl SharedIrqSessionSlot {
    /// Build one kernel-only session slot.
    fn kernel_handler(handler: Option<IrqHandler>) -> Self {
        Self {
            handler,
            user: None,
        }
    }

    /// Build one user-visible session slot.
    fn user_session(core: Arc<IrqSessionCore>) -> Self {
        Self {
            handler: None,
            user: Some(core),
        }
    }

    /// Return whether the slot no longer carries any state.
    fn is_empty(&self) -> bool {
        self.handler.is_none() && self.user.is_none()
    }
}

/// In-flight level-triggered epoch waiting for user acknowledgements.
struct InFlightEpoch {
    /// Epoch number currently blocked behind one level-triggered delivery.
    epoch: u64,
    /// Number of non-monitor sessions that still need to ACK.
    awaiting_participants: usize,
    /// Whether at least one session claimed this epoch.
    claimed: bool,
}

/// Shared state machine for one logical IRQ line.
///
/// Each line multiplexes:
/// - zero or more kernel handlers;
/// - zero or more user-visible sessions;
/// - optional level-triggered mask/unmask state.
struct SharedIrqLine {
    /// Stable logical IRQ identifier.
    irq: usize,
    /// Session id allocator scoped to this line.
    session_ids: IdAllocator<usize>,
    /// Physical trigger mode.
    trigger: LineTriggerMode,
    /// Administrative mask requested by control-plane code.
    admin_masked: bool,
    /// Delivery mask held while one level-triggered epoch is still in flight.
    delivery_masked: bool,
    /// Last delivered epoch number.
    epoch: u64,
    /// State of the current level-triggered epoch, if any.
    inflight: Option<InFlightEpoch>,
    /// Session slots keyed by session id.
    sessions: HashMap<usize, SharedIrqSessionSlot>,
}

impl SharedIrqLine {
    /// Create one empty shared-line state machine.
    fn new(irq: usize) -> Self {
        Self {
            irq,
            session_ids: IdAllocator::new(1, 1),
            trigger: LineTriggerMode::Edge,
            admin_masked: false,
            delivery_masked: false,
            epoch: 0,
            inflight: None,
            sessions: HashMap::new(),
        }
    }

    /// Allocate one backend-only handler session.
    fn open_kernel_handler(&mut self) -> usize {
        let session_id = self.session_ids.allocate();
        self.sessions
            .insert(session_id, SharedIrqSessionSlot::kernel_handler(None));
        session_id
    }

    /// Allocate one user-visible shared session.
    fn open_user_session(&mut self, flags: IrqOpenFlags) -> Arc<IrqSessionCore> {
        let session_id = self.session_ids.allocate();
        let core = Arc::new(IrqSessionCore::new(self.irq, session_id, flags));
        self.sessions
            .insert(session_id, SharedIrqSessionSlot::user_session(core.clone()));
        core
    }

    /// Record the current trigger mode.
    fn set_trigger_mode(&mut self, trigger: LineTriggerMode) {
        self.trigger = trigger;
    }

    /// Close one session slot and wake any blocked user waiter.
    fn close(&mut self, session_id: usize) -> IrqResult {
        let slot = self
            .sessions
            .remove(&session_id)
            .ok_or(IrqError::InvalidParameter)?;
        self.session_ids.recycle(session_id);
        if let Some(user) = slot.user {
            user.close();
        }
        Ok(())
    }

    /// Attach one kernel handler to an existing session.
    fn attach(&mut self, session_id: usize, handler: IrqHandler) -> IrqResult {
        let slot = self
            .sessions
            .get_mut(&session_id)
            .ok_or(IrqError::InvalidParameter)?;
        slot.handler = Some(handler);
        Ok(())
    }

    /// Detach one kernel handler and drop the slot if it becomes empty.
    fn detach(&mut self, session_id: usize) -> IrqResult {
        let slot = self
            .sessions
            .get_mut(&session_id)
            .ok_or(IrqError::InvalidParameter)?;
        if slot.handler.is_none() {
            return Err(IrqError::InvalidParameter);
        }
        slot.handler = None;
        if slot.is_empty() {
            self.sessions.remove(&session_id);
            self.session_ids.recycle(session_id);
        }
        Ok(())
    }

    /// Record one administrative mask bit.
    fn set_admin_masked(&mut self, masked: bool) {
        self.admin_masked = masked;
    }

    /// Return one discoverable IRQ-line snapshot.
    fn state_snapshot(&self) -> IrqLineState {
        let enabled_session_count = self
            .sessions
            .values()
            .filter_map(|slot| slot.user.as_ref())
            .filter(|user| user.is_enabled() && !user.is_closed())
            .count();
        let mut flags = IrqLineFlags::empty();
        if !self.admin_masked {
            flags |= IrqLineFlags::ADMIN_ENABLED;
        }
        if self.delivery_masked {
            flags |= IrqLineFlags::DELIVERY_BLOCKED;
        }
        if self.epoch != 0 {
            flags |= IrqLineFlags::HAS_CURRENT_EPOCH;
        }
        IrqLineState {
            irq: self.irq,
            flags,
            session_count: self.sessions.len(),
            enabled_session_count,
            current_epoch: self.epoch,
        }
    }

    /// Deliver one physical IRQ occurrence into the shared-line state machine.
    ///
    /// Edge-triggered lines simply fan out the epoch. Level-triggered lines
    /// retain one in-flight epoch and request physical masking until every
    /// non-monitor user session acknowledges the delivery.
    fn handle(&mut self) -> IrqHandleAction {
        if self.sessions.is_empty() {
            return IrqHandleAction::None;
        }

        self.epoch = self.epoch.saturating_add(1);
        let epoch = self.epoch;
        let mut awaiting_participants = 0usize;

        for slot in self.sessions.values() {
            if let Some(handler) = slot.handler.as_ref() {
                handler();
            }
            if let Some(user) = slot.user.as_ref() {
                if user.is_closed() || !user.is_enabled() {
                    continue;
                }
                user.deliver(epoch);
                if !user.is_monitor() {
                    awaiting_participants = awaiting_participants.saturating_add(1);
                }
            }
        }

        if matches!(self.trigger, LineTriggerMode::Level) && awaiting_participants > 0 {
            self.delivery_masked = true;
            self.inflight = Some(InFlightEpoch {
                epoch,
                awaiting_participants,
                claimed: false,
            });
            IrqHandleAction::MaskLine
        } else {
            IrqHandleAction::None
        }
    }

    fn ack_user_session(
        &mut self,
        session_id: usize,
        epoch: u64,
        disposition: IrqAckDisposition,
    ) -> IrqResult<IrqAckOutcome> {
        let slot = self
            .sessions
            .get(&session_id)
            .ok_or(IrqError::InvalidParameter)?;
        let user = slot.user.as_ref().ok_or(IrqError::InvalidParameter)?;
        let pending_count = user.acknowledge(epoch, disposition)?;

        let mut remaining_participants = 0usize;
        let mut should_unmask = false;
        let mut claimed = false;

        if let Some(inflight) = self.inflight.as_mut() {
            if inflight.epoch == epoch && !user.is_monitor() {
                if inflight.awaiting_participants == 0 {
                    return Err(IrqError::InvalidParameter);
                }
                inflight.awaiting_participants -= 1;
                if matches!(disposition, IrqAckDisposition::Claimed) {
                    inflight.claimed = true;
                }
                remaining_participants = inflight.awaiting_participants;
                claimed = inflight.claimed;
                if inflight.awaiting_participants == 0 {
                    should_unmask = self.delivery_masked;
                }
            }
        }

        if should_unmask {
            self.delivery_masked = false;
            self.inflight = None;
        }

        Ok(IrqAckOutcome {
            remaining_participants,
            pending_count,
            should_unmask,
            claimed,
        })
    }

    /// Return whether any session is still attached.
    fn has_sessions(&self) -> bool {
        !self.sessions.is_empty()
    }

    /// Close every session and clear in-flight level-trigger state.
    fn close_all(&mut self) {
        let sessions = core::mem::take(&mut self.sessions);
        for (session_id, slot) in sessions {
            self.session_ids.recycle(session_id);
            if let Some(user) = slot.user {
                user.close();
            }
        }
        self.inflight = None;
        self.delivery_masked = false;
    }
}

/// Current programmed route of one synthetic message interrupt.
#[derive(Debug, Clone, Copy)]
pub struct MessageVectorRoute {
    /// Logical CPU currently targeted by this message interrupt.
    pub cpu_id: usize,
    /// Physical APIC id encoded into the message address.
    pub apic_id: usize,
    /// Local vector allocated on `cpu_id`.
    pub vector: usize,
}

/// One APIC-backed message-interrupt entry routed to one local vector.
pub struct MessageIrqEntryCore {
    /// Stable synthetic message identifier.
    message_id: usize,
    /// Delivery kind requested by the allocating subsystem.
    kind: MessageIrqKind,
    /// Block-wide allocation flags.
    flags: MessageIrqFlags,
    /// Current CPU/APIC/vector route.
    route: SpinLock<MessageVectorRoute, IrqSaveGuard>,
    /// Shared line state machine behind the entry.
    line: SpinLock<SharedIrqLine, IrqSaveGuard>,
}

impl MessageIrqEntryCore {
    /// Create one message-interrupt entry with an initial CPU/vector route.
    pub fn new(
        message_id: usize,
        kind: MessageIrqKind,
        flags: MessageIrqFlags,
        route: MessageVectorRoute,
    ) -> Self {
        let mut line = SharedIrqLine::new(message_id);
        line.set_trigger_mode(LineTriggerMode::Edge);
        Self {
            message_id,
            kind,
            flags,
            route: SpinLock::new(route),
            line: SpinLock::new(line),
        }
    }

    /// Return the stable synthetic identifier of this message entry.
    pub fn message_id(&self) -> usize {
        self.message_id
    }

    /// Return the backing message kind.
    pub fn kind(&self) -> MessageIrqKind {
        self.kind
    }

    /// Return the stable block flags for this entry.
    pub fn flags(&self) -> MessageIrqFlags {
        self.flags
    }

    /// Return the current message descriptor programmed into devices.
    pub fn descriptor(&self) -> MessageIrqDescriptor {
        let route = *self.route.lock();
        MessageIrqDescriptor {
            address_lo: 0xFEE0_0000u32 | (((route.apic_id as u32) & 0xff) << 12),
            address_hi: ((route.apic_id as u32) >> 8),
            data: route.vector as u32,
            cpu_id: route.cpu_id,
            vector: route.vector,
        }
    }

    /// Return one line-like state snapshot for diagnostics.
    pub fn state_snapshot(&self) -> IrqLineState {
        self.line.lock().state_snapshot()
    }

    /// Open one shared user-visible session for this message entry.
    pub fn open_user_session(&self, flags: IrqOpenFlags) -> Arc<IrqSessionCore> {
        self.line.lock().open_user_session(flags)
    }

    /// Close one previously opened session.
    pub fn close_session(&self, session_id: usize) -> IrqResult {
        self.line.lock().close(session_id)
    }

    /// Close every session currently attached to this message entry.
    pub fn close_all_sessions(&self) {
        self.line.lock().close_all();
    }

    /// Update the route for this message entry after vector reallocation.
    pub fn retarget(&self, cpu_id: usize, apic_id: usize, vector: usize) -> MessageIrqDescriptor {
        let mut route = self.route.lock();
        *route = MessageVectorRoute {
            cpu_id,
            apic_id,
            vector,
        };
        drop(route);
        self.descriptor()
    }

    /// Deliver one incoming message interrupt to every attached session.
    pub fn handle_delivery(&self) -> IrqHandleAction {
        self.line.lock().handle()
    }

    /// Record one session ACK against this message entry.
    pub fn ack_session(
        &self,
        session_id: usize,
        epoch: u64,
        disposition: IrqAckDisposition,
    ) -> IrqResult<usize> {
        Ok(self
            .line
            .lock()
            .ack_user_session(session_id, epoch, disposition)?
            .pending_count)
    }
}

/// Shared user-visible IRQ session core.
pub struct IrqSessionCore {
    /// Owning logical IRQ number.
    irq: usize,
    /// Stable session id within the owning shared line.
    session_id: usize,
    /// Session creation flags.
    flags: IrqOpenFlags,
    /// Whether this session currently accepts delivery.
    enabled: AtomicBool,
    /// Whether the session has been closed.
    closed: AtomicBool,
    /// Whether one delivery was dropped because the bounded backlog overflowed.
    overflowed: AtomicBool,
    /// Epoch currently visible to userspace and awaiting ACK.
    inflight_epoch: AtomicU64,
    /// Most recent acknowledged epoch.
    last_acked_epoch: AtomicU64,
    /// Additional epochs delivered while one older epoch is still in flight.
    pending_epochs: ConcurrentQueue<u64>,
    /// Waiter notification primitive for blocking `wait()`.
    event: Event,
}

impl IrqSessionCore {
    /// Create one user-visible IRQ session core.
    fn new(irq: usize, session_id: usize, flags: IrqOpenFlags) -> Self {
        Self {
            irq,
            session_id,
            flags,
            enabled: AtomicBool::new(true),
            closed: AtomicBool::new(false),
            overflowed: AtomicBool::new(false),
            inflight_epoch: AtomicU64::new(0),
            last_acked_epoch: AtomicU64::new(0),
            pending_epochs: ConcurrentQueue::bounded(IRQ_SESSION_PENDING_CAPACITY),
            event: Event::new(),
        }
    }

    /// Return the owning IRQ number of this session.
    pub fn irq(&self) -> usize {
        self.irq
    }

    /// Return the stable session identifier within one IRQ line.
    pub fn session_id(&self) -> usize {
        self.session_id
    }

    /// Return the session creation flags.
    pub fn flags(&self) -> IrqOpenFlags {
        self.flags
    }

    /// Return whether this session is a monitor-only observer.
    pub fn is_monitor(&self) -> bool {
        self.flags.contains(IrqOpenFlags::MONITOR)
    }

    /// Return whether this session currently accepts deliveries.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// Enable or disable delivery to this session.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    /// Return whether this session has been closed.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Close this session and wake every waiter.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.event.notify_all();
    }

    /// Return one state snapshot suitable for object query operations.
    pub fn state_snapshot(&self) -> IrqSessionState {
        IrqSessionState {
            irq: self.irq,
            session_id: self.session_id,
            flags: self.flags,
            enabled: self.is_enabled(),
            closed: self.is_closed(),
            inflight_epoch: match self.inflight_epoch.load(Ordering::Acquire) {
                0 => None,
                epoch => Some(epoch),
            },
            pending_count: self.pending_epochs.len(),
            last_acked_epoch: self.last_acked_epoch.load(Ordering::Acquire),
        }
    }

    /// Attempt to consume one pending delivery without blocking.
    pub fn try_wait(&self) -> IrqResult<Option<IrqDelivery>> {
        if self.is_closed() {
            return Err(IrqError::Closed);
        }

        let inflight = self.inflight_epoch.load(Ordering::Acquire);
        if inflight != 0 {
            let pending_count = self.pending_epochs.len().saturating_add(1);
            return Ok(Some(IrqDelivery {
                irq: self.irq,
                epoch: inflight,
                pending_count,
                flags: self.flags,
            }));
        }

        let Some(epoch) = self.pending_epochs.pop() else {
            if self.overflowed.load(Ordering::Acquire) {
                return Err(IrqError::OutOfResources);
            }
            return Ok(None);
        };
        let pending_count = self.pending_epochs.len().saturating_add(1);
        self.inflight_epoch.store(epoch, Ordering::Release);
        Ok(Some(IrqDelivery {
            irq: self.irq,
            epoch,
            pending_count,
            flags: self.flags,
        }))
    }

    /// Wait until one delivery becomes visible to this session.
    pub async fn wait(&self, deadline: usize, nonblock: bool) -> IrqResult<IrqDelivery> {
        if let Some(snapshot) = self.try_wait()? {
            return Ok(snapshot);
        }
        if nonblock || deadline == 0 {
            return Err(IrqError::WouldBlock);
        }
        if deadline != usize::MAX {
            return Err(IrqError::DeadlineUnsupported);
        }

        loop {
            let listener = self.event.listen();
            if let Some(snapshot) = self.try_wait()? {
                return Ok(snapshot);
            }
            listener.await;
        }
    }

    /// Append one delivered epoch and wake blocked waiters.
    fn deliver(&self, epoch: u64) {
        if self.is_closed() || self.overflowed.load(Ordering::Acquire) {
            return;
        }

        if let Err(PushError::Full(_)) = self.pending_epochs.push(epoch) {
            self.overflowed.store(true, Ordering::Release);
        }
        self.event.notify_all();
    }

    /// Acknowledge the currently visible epoch for this session.
    fn acknowledge(&self, epoch: u64, disposition: IrqAckDisposition) -> IrqResult<usize> {
        if self.is_closed() {
            return Err(IrqError::Closed);
        }
        if self.is_monitor() && matches!(disposition, IrqAckDisposition::Claimed) {
            return Err(IrqError::InvalidParameter);
        }

        let inflight = self.inflight_epoch.load(Ordering::Acquire);
        if inflight == 0 || inflight != epoch {
            return Err(IrqError::InvalidParameter);
        }

        self.inflight_epoch.store(0, Ordering::Release);
        self.last_acked_epoch.store(epoch, Ordering::Release);
        Ok(self.pending_epochs.len())
    }
}

/// Shared IRQ line manager backing one contiguous IRQ index range.
pub struct SharedIrqManager {
    /// First logical IRQ covered by this manager.
    index_base: usize,
    /// Shared-line state for each logical IRQ in the covered range.
    lines: Vec<SharedIrqLine>,
}

impl SharedIrqManager {
    /// Create one shared manager for the supplied IRQ range.
    pub fn new(range: Range<usize>) -> Self {
        let len = range.len();
        let mut lines = Vec::with_capacity(len);
        lines.extend((range.start..range.end).map(SharedIrqLine::new));
        Self {
            index_base: range.start,
            lines,
        }
    }

    /// Translate one logical IRQ number into one local line index.
    fn line_index(&self, index: usize) -> IrqResult<usize> {
        let idx = index
            .checked_sub(self.index_base)
            .ok_or(IrqError::InvalidIrq(index))?;
        if idx >= self.lines.len() {
            return Err(IrqError::InvalidIrq(index));
        }
        Ok(idx)
    }

    /// Open one backend-only handler session.
    pub fn open_session(&mut self, index: usize) -> IrqResult<usize> {
        let idx = self.line_index(index)?;
        Ok(self.lines[idx].open_kernel_handler())
    }

    /// Open one shared user-visible IRQ session.
    pub fn open_user_session(
        &mut self,
        index: usize,
        flags: IrqOpenFlags,
    ) -> IrqResult<Arc<IrqSessionCore>> {
        let idx = self.line_index(index)?;
        Ok(self.lines[idx].open_user_session(flags))
    }

    /// Register one kernel handler on one IRQ line.
    pub fn register_handler(&mut self, index: usize, handler: IrqHandler) -> IrqResult<usize> {
        let session_id = self.open_session(index)?;
        self.attach_handler(index, session_id, handler)?;
        Ok(index)
    }

    /// Close one existing session.
    pub fn close_session(&mut self, index: usize, session_id: usize) -> IrqResult {
        let idx = self.line_index(index)?;
        self.lines[idx].close(session_id)
    }

    /// Remove every kernel handler session from one IRQ line.
    pub fn unregister_handler(&mut self, index: usize) -> IrqResult {
        let idx = self.line_index(index)?;
        if self.lines[idx].sessions.is_empty() {
            return Err(IrqError::InvalidIrq(index));
        }
        self.lines[idx].sessions.clear();
        Ok(())
    }

    /// Attach one kernel handler to one backend session.
    pub fn attach_handler(
        &mut self,
        index: usize,
        session_id: usize,
        handler: IrqHandler,
    ) -> IrqResult {
        let idx = self.line_index(index)?;
        self.lines[idx].attach(session_id, handler)
    }

    /// Detach one kernel handler from one backend session.
    pub fn detach_handler(&mut self, index: usize, session_id: usize) -> IrqResult {
        let idx = self.line_index(index)?;
        self.lines[idx].detach(session_id)
    }

    /// Record whether one line is administratively masked.
    pub fn set_admin_masked(&mut self, index: usize, masked: bool) -> IrqResult {
        let idx = self.line_index(index)?;
        self.lines[idx].set_admin_masked(masked);
        Ok(())
    }

    /// Return one state snapshot of one IRQ line.
    pub fn line_state(&self, index: usize) -> IrqResult<IrqLineState> {
        let idx = self.line_index(index)?;
        Ok(self.lines[idx].state_snapshot())
    }

    /// Record one session ACK against one line.
    pub fn ack_session(
        &mut self,
        index: usize,
        session_id: usize,
        epoch: u64,
        disposition: IrqAckDisposition,
    ) -> IrqResult<IrqAckOutcome> {
        let idx = self.line_index(index)?;
        self.lines[idx].ack_user_session(session_id, epoch, disposition)
    }

    /// Deliver one physical IRQ occurrence into the shared line state machine.
    pub fn handle_irq(&mut self, index: usize) -> IrqResult<IrqHandleAction> {
        let idx = self.line_index(index)?;
        Ok(self.lines[idx].handle())
    }

    /// Return whether one IRQ line currently has any registered sessions.
    pub fn has_sessions(&self, index: usize) -> bool {
        let Some(idx) = index.checked_sub(self.index_base) else {
            return false;
        };
        self.lines.get(idx).is_some_and(SharedIrqLine::has_sessions)
    }
}

/// Per-CPU vector manager for timer, IPI, and routed external IRQ handlers.
pub struct PerCpuIrqManager {
    /// First local vector managed by this allocator.
    vector_base: usize,
    /// Handler table indexed relative to `vector_base`.
    handlers: Vec<Option<IrqHandler>>,
    /// Dedicated timer vector excluded from general allocation.
    timer_vector: usize,
    /// Timer ISR currently installed on this CPU.
    timer_handler: Option<IrqHandler>,
}

impl PerCpuIrqManager {
    /// Create one per-CPU vector allocator over the supplied vector range.
    pub fn new(vector_range: Range<usize>, timer_vector: usize) -> Self {
        Self {
            vector_base: vector_range.start,
            handlers: (0..vector_range.len()).map(|_| None).collect(),
            timer_vector,
            timer_handler: None,
        }
    }

    /// Allocate one local vector slot for one handler.
    pub fn alloc_handler(&mut self, handler: IrqHandler) -> IrqResult<usize> {
        let idx = self
            .handlers
            .iter()
            .position(Option::is_none)
            .ok_or(IrqError::OutOfResources)?;
        self.handlers[idx] = Some(handler);
        Ok(self.vector_base + idx)
    }

    /// Allocate one contiguous run of local vector slots.
    pub fn alloc_handlers_contiguous(
        &mut self,
        handlers: Vec<IrqHandler>,
    ) -> IrqResult<Vec<usize>> {
        self.alloc_handlers_contiguous_aligned(handlers, 1)
    }

    /// Allocate one aligned contiguous run of local vector slots.
    pub fn alloc_handlers_contiguous_aligned(
        &mut self,
        handlers: Vec<IrqHandler>,
        align: usize,
    ) -> IrqResult<Vec<usize>> {
        let count = handlers.len();
        if count == 0 {
            return Ok(Vec::new());
        }
        let align = align.max(1);

        let Some(start) = self
            .handlers
            .windows(count)
            .enumerate()
            .find_map(|(start, window)| {
                let vector = self.vector_base + start;
                ((vector % align == 0) && window.iter().all(Option::is_none)).then_some(start)
            })
        else {
            return Err(IrqError::OutOfResources);
        };

        let mut vectors = Vec::with_capacity(count);
        for (offset, handler) in handlers.into_iter().enumerate() {
            self.handlers[start + offset] = Some(handler);
            vectors.push(self.vector_base + start + offset);
        }
        Ok(vectors)
    }

    /// Free one previously allocated local vector slot.
    pub fn free_handler(&mut self, vector: usize) -> IrqResult {
        if vector == self.timer_vector {
            return Err(IrqError::InvalidIrq(vector));
        }
        let idx = vector
            .checked_sub(self.vector_base)
            .ok_or(IrqError::InvalidIrq(vector))?;
        if idx >= self.handlers.len() {
            return Err(IrqError::InvalidIrq(vector));
        }
        if self.handlers[idx].is_none() {
            return Err(IrqError::InvalidIrq(vector));
        }
        self.handlers[idx] = None;
        Ok(())
    }

    /// Dispatch one local vector or timer interrupt.
    pub fn handle_irq(&self, vector: usize) -> IrqResult {
        if vector == self.timer_vector {
            let handler = self
                .timer_handler
                .as_ref()
                .ok_or(IrqError::InvalidIrq(vector))?;
            handler();
            return Ok(());
        }
        let idx = vector
            .checked_sub(self.vector_base)
            .ok_or(IrqError::InvalidIrq(vector))?;
        if idx >= self.handlers.len() {
            return Err(IrqError::InvalidIrq(vector));
        }
        let handler = self.handlers[idx]
            .as_ref()
            .ok_or(IrqError::InvalidIrq(vector))?;
        handler();
        Ok(())
    }

    /// Install the reserved timer ISR on this CPU.
    pub fn set_timer_isr(&mut self, handler: IrqHandler) -> IrqResult<usize> {
        self.timer_handler = Some(handler);
        Ok(self.timer_vector)
    }

    /// Clear the reserved timer ISR on this CPU.
    pub fn clear_timer_isr(&mut self) -> IrqResult {
        if self.timer_handler.is_none() {
            return Err(IrqError::InvalidIrq(self.timer_vector));
        }
        self.timer_handler = None;
        Ok(())
    }
}
