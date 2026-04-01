//! APIC-backed x86_64 interrupt controller.
//!
//! This controller owns three delivery domains:
//! - IOAPIC-routed external GSIs;
//! - LAPIC-local vectors such as timer and reschedule IPIs;
//! - synthetic message IRQ blocks used for MSI/MSI-X style routing.
//!
//! The actual per-line and per-vector state machines live in
//! [`manager`]; this module wires them into the machine-level
//! [`InterruptControllerTrait`] implementation.

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
use core::{
    arch::asm,
    hint::spin_loop,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use libakarin_machine_core::{
    cpu::PerCpuTrait,
    interrupt::{
        InterruptControllerInfo, InterruptControllerTrait, IpiReason, IpiTarget, IrqError,
        IrqHandler, IrqLineFlags, IrqLineState, IrqLineTrait, IrqResult, IrqSessionState,
        IrqSessionTrait, IrqWaitFuture, MessageIrqBlockTrait, MessageIrqDescriptor,
        MessageIrqFlags, MessageIrqInfo, MessageIrqKind, MessageIrqRequest,
    },
    sync::{NoOp, ScopedGuard},
};
use libakarin_sync::{
    collections::IdAllocator,
    spin::{Once, SpinLock},
};
use libakarin_syscall::{IrqAckDisposition, IrqOpenFlags, IrqWaitFlags};

use crate::arch::{PerCpu, guards::IrqSaveGuard};

pub mod consts;
mod ioapic;
pub mod lapic;
mod manager;
pub use consts::APIC_IPI_MAILBOX;
use consts::{
    APIC_IPI_CUSTOM_BASE, APIC_IPI_FLUSH_TLB, APIC_IPI_PANIC, APIC_IPI_RESCHEDULE,
    APIC_TIMER_INTERRUPT, IOAPIC_IRQ_RANGE, LAPIC_BASE,
};
use ioapic::IoApicList;
use lapic::LocalApic;
pub use lapic::TimerMode;
use manager::{
    IrqHandleAction, IrqSessionCore, MessageIrqEntryCore, MessageVectorRoute, PerCpuIrqManager,
    SharedIrqManager,
};

const APIC_MESSAGE_IRQ_BASE: usize = 0x1_0000;

/// Top-level APIC interrupt controller implementation.
pub struct Apic {
    /// Discovered IOAPIC devices used for external GSI routing.
    io_apic_list: IoApicList,
    /// Shared line state for external GSIs.
    manager_gsi: SpinLock<SharedIrqManager, ScopedGuard<IrqSaveGuard>>,
    /// Shared line state for LAPIC-local vectors such as IPIs.
    manager_lapic: SpinLock<SharedIrqManager, ScopedGuard<IrqSaveGuard>>,
    /// Current GSI-to-local-vector route for each external IRQ.
    routes: SpinLock<Vec<Option<IoRoute>>, ScopedGuard<IrqSaveGuard>>,
    /// Synthetic message-IRQ id allocator.
    message_ids: IdAllocator<usize>,
}

/// Per-CPU local vector managers indexed by logical CPU id.
///
/// Each entry owns LAPIC-local vector allocation for one CPU. Cross-CPU
/// routing consults this table when assigning or freeing vectors.
static PERCPU_IRQ_MANAGERS: Once<
    Vec<SpinLock<PerCpuIrqManager, ScopedGuard<IrqSaveGuard>>>,
    ScopedGuard<NoOp>,
> = Once::new();
/// Self-test accounting for mailbox/IPI validation.
static LOCAL_IPI_SELFTEST_HITS: AtomicUsize = AtomicUsize::new(0);
/// Per-CPU once-only logging mask for the first reschedule IPI.
static RESCHEDULE_IPI_LOGGED_MASK: AtomicUsize = AtomicUsize::new(0);

/// Cached route from one external GSI to one per-CPU local vector.
#[derive(Clone, Copy)]
struct IoRoute {
    /// Per-CPU vector currently programmed into the IOAPIC entry.
    vector: usize,
    /// Destination CPU that owns `vector`.
    cpu_id: usize,
}

/// Machine-visible APIC IRQ line backend.
pub struct ApicIrqLine {
    controller: &'static Apic,
    irq: usize,
}

enum ApicSessionTarget {
    /// Traditional external or LAPIC-local IRQ line.
    Gsi(usize),
    /// Synthetic message interrupt entry.
    Message(Arc<MessageIrqEntryCore>),
}

/// APIC-backed allocated message interrupt block.
pub struct ApicMessageBlock {
    /// Controller used to release message vectors on drop.
    controller: &'static Apic,
    /// Message-delivery mode requested by the caller.
    kind: MessageIrqKind,
    /// Block-wide allocation flags.
    flags: MessageIrqFlags,
    /// Backing synthetic message entries.
    entries: Arc<[Arc<MessageIrqEntryCore>]>,
}

impl ApicMessageBlock {
    /// Build one owned message block from already allocated entries.
    fn new(
        controller: &'static Apic,
        kind: MessageIrqKind,
        flags: MessageIrqFlags,
        entries: Vec<Arc<MessageIrqEntryCore>>,
    ) -> Self {
        Self {
            controller,
            kind,
            flags,
            entries: entries.into(),
        }
    }

    /// Return the entry at `index`, validating user-visible bounds.
    fn entry(&self, index: usize) -> IrqResult<&Arc<MessageIrqEntryCore>> {
        self.entries.get(index).ok_or(IrqError::InvalidParameter)
    }
}

impl Drop for ApicMessageBlock {
    fn drop(&mut self) {
        for entry in self.entries.iter() {
            let _ = self.controller.release_message_entry(entry);
        }
    }
}

impl ApicIrqLine {
    /// Build one IRQ-line wrapper for discovery and session open operations.
    fn new(controller: &'static Apic, irq: usize) -> Self {
        Self { controller, irq }
    }
}

impl IrqLineTrait for ApicIrqLine {
    type Session = ApicIrqSession;

    fn irq_id(&self) -> usize {
        self.irq
    }

    fn state(&self) -> IrqResult<IrqLineState> {
        self.controller.irq_line_state(self.irq)
    }

    fn open_session(&self, flags: IrqOpenFlags) -> IrqResult<Self::Session> {
        self.controller.open_shared_session(self.irq, flags)
    }

    fn set_destination(&self, cpu_id: usize) -> IrqResult {
        self.controller.route_irq_line(self.irq, cpu_id)
    }
}

impl MessageIrqBlockTrait for ApicMessageBlock {
    type Session = ApicIrqSession;

    fn kind(&self) -> MessageIrqKind {
        self.kind
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn info(&self) -> IrqResult<MessageIrqInfo> {
        Ok(MessageIrqInfo {
            kind: self.kind,
            count: self.entries.len(),
            flags: self.flags,
        })
    }

    fn descriptor(&self, index: usize) -> IrqResult<MessageIrqDescriptor> {
        Ok(self.entry(index)?.descriptor())
    }

    fn open_session(&self, index: usize, flags: IrqOpenFlags) -> IrqResult<Self::Session> {
        let entry = self.entry(index)?.clone();
        let core = entry.open_user_session(flags);
        Ok(ApicIrqSession::new_message(self.controller, entry, core))
    }

    fn retarget(&self, index: usize, cpu_id: usize) -> IrqResult<MessageIrqDescriptor> {
        let entry = self.entry(index)?;
        self.controller.retarget_message_entry(entry, cpu_id)
    }

    fn close_sessions(&self) {
        for entry in self.entries.iter() {
            entry.close_all_sessions();
        }
    }
}

/// Machine-visible APIC shared IRQ session backend.
pub struct ApicIrqSession {
    /// Owning controller used for close and ACK operations.
    controller: &'static Apic,
    /// Backend object that will observe close/ack requests.
    target: ApicSessionTarget,
    /// Shared session core exposed through the generic IRQ trait.
    core: Arc<IrqSessionCore>,
    /// Ensures close is only forwarded once.
    released: AtomicBool,
}

impl ApicIrqSession {
    /// Build one session over one traditional IRQ line.
    fn new_line(controller: &'static Apic, irq: usize, core: Arc<IrqSessionCore>) -> Self {
        Self {
            controller,
            target: ApicSessionTarget::Gsi(irq),
            core,
            released: AtomicBool::new(false),
        }
    }

    /// Build one session over one synthetic message entry.
    fn new_message(
        controller: &'static Apic,
        entry: Arc<MessageIrqEntryCore>,
        core: Arc<IrqSessionCore>,
    ) -> Self {
        Self {
            controller,
            target: ApicSessionTarget::Message(entry),
            core,
            released: AtomicBool::new(false),
        }
    }

    /// Close the backing session exactly once.
    fn close_once(&self) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        match &self.target {
            ApicSessionTarget::Gsi(irq) => {
                let _ = self
                    .controller
                    .close_shared_session(*irq, self.core.session_id());
            }
            ApicSessionTarget::Message(entry) => {
                let _ = entry.close_session(self.core.session_id());
            }
        }
    }
}

impl Drop for ApicIrqSession {
    fn drop(&mut self) {
        self.close_once();
    }
}

impl IrqSessionTrait for ApicIrqSession {
    fn session_id(&self) -> usize {
        self.core.session_id()
    }

    fn irq_id(&self) -> usize {
        self.core.irq()
    }

    fn flags(&self) -> IrqOpenFlags {
        self.core.flags()
    }

    fn state(&self) -> IrqSessionState {
        self.core.state_snapshot()
    }

    fn set_enabled(&self, enabled: bool) {
        self.core.set_enabled(enabled);
    }

    fn close(&self) {
        self.close_once();
    }

    fn wait(&self, deadline: usize, flags: IrqWaitFlags) -> IrqWaitFuture {
        let core = self.core.clone();
        Box::pin(async move {
            core.wait(deadline, flags.contains(IrqWaitFlags::NONBLOCK))
                .await
        })
    }

    fn ack(&self, epoch: u64, disposition: IrqAckDisposition) -> IrqResult<usize> {
        match &self.target {
            ApicSessionTarget::Gsi(irq) => {
                self.controller
                    .ack_shared_session(*irq, self.core.session_id(), epoch, disposition)
            }
            ApicSessionTarget::Message(entry) => {
                entry.ack_session(self.core.session_id(), epoch, disposition)
            }
        }
    }
}

impl Apic {
    /// Mark one CPU as having emitted one once-only APIC log message.
    fn mark_cpu_logged(mask: &AtomicUsize, cpu_id: usize) -> bool {
        if cpu_id >= usize::BITS as usize {
            return true;
        }

        let bit = 1usize << cpu_id;
        let previous = mask.fetch_or(bit, Ordering::AcqRel);
        previous & bit == 0
    }

    /// Register LAPIC-local handlers that exist on every CPU.
    fn register_local_ipi_handlers(&self) {
        let _ = self.register_lapic_handler(
            APIC_IPI_RESCHEDULE,
            Box::new(|| {
                let cpu_id = PerCpu::id();
                if Self::mark_cpu_logged(&RESCHEDULE_IPI_LOGGED_MASK, cpu_id) {
                    log::info!("[x86_64/apic cpu={}] first reschedule IPI", cpu_id);
                }
                crate::RuntimeServices::global().reschedule_ipi_hook()();
            }),
        );
        let _ = self.register_lapic_handler(
            APIC_IPI_MAILBOX,
            Box::new(|| {
                let _ = crate::RuntimeServices::global().drain_current_cpu_mailbox();
            }),
        );
    }

    /// Construct the APIC controller after IOAPIC discovery.
    pub fn new() -> Self {
        log::info!("[x86_64/apic] create APIC controller");
        let io_apic_list = IoApicList::new();
        let max_gsi = io_apic_list.max_gsi();
        Self {
            io_apic_list,
            manager_gsi: SpinLock::new(SharedIrqManager::new(0..(max_gsi + 1))),
            manager_lapic: SpinLock::new(SharedIrqManager::new(LAPIC_BASE..0x100)),
            routes: SpinLock::new(vec![None; max_gsi + 1]),
            message_ids: IdAllocator::new(APIC_MESSAGE_IRQ_BASE, 1),
        }
    }

    /// Run one closure on the IOAPIC that owns `gsi`.
    fn with_ioapic<F>(&self, gsi: u32, op: F) -> IrqResult
    where
        F: FnOnce(&ioapic::IoApic) -> IrqResult,
    {
        if let Some(ioapic) = self.io_apic_list.find(gsi) {
            op(ioapic)
        } else {
            Err(IrqError::InvalidIrq(gsi as usize))
        }
    }

    /// Allocate and install one per-CPU vector manager for every CPU.
    fn init_percpu_irq_managers() {
        let _ = PERCPU_IRQ_MANAGERS.try_init({
            let mut managers = Vec::with_capacity(PerCpu::count());
            for _ in 0..PerCpu::count() {
                managers.push(SpinLock::new(PerCpuIrqManager::new(
                    IOAPIC_IRQ_RANGE,
                    APIC_TIMER_INTERRUPT,
                )));
            }
            managers
        });
    }

    /// Return the initialized per-CPU vector-manager table.
    fn percpu_irq_managers() -> &'static Vec<SpinLock<PerCpuIrqManager, ScopedGuard<IrqSaveGuard>>>
    {
        PERCPU_IRQ_MANAGERS.get()
    }

    /// Build one handler closure that dispatches one shared GSI through the
    /// controller state machine.
    fn route_closure(&self, gsi: usize) -> IrqHandler {
        let controller = unsafe { &*(self as *const Self) };
        Box::new(move || {
            let _ = controller.dispatch_shared_irq(gsi);
        })
    }

    /// Allocate one synthetic message-IRQ identifier.
    fn allocate_message_id(&self) -> usize {
        self.message_ids.allocate()
    }

    /// Build one local-vector handler for one synthetic message entry.
    fn message_entry_handler(entry: Arc<MessageIrqEntryCore>) -> IrqHandler {
        Box::new(move || {
            let _ = entry.handle_delivery();
        })
    }

    /// Allocate a contiguous run of message entries on one CPU.
    fn allocate_message_entries_contiguous(
        &'static self,
        kind: MessageIrqKind,
        count: usize,
        cpu_id: usize,
        flags: MessageIrqFlags,
    ) -> IrqResult<Vec<Arc<MessageIrqEntryCore>>> {
        let apic_id = PerCpu::lapic_id_of(cpu_id).ok_or(IrqError::InvalidParameter)?;
        let mut entries = Vec::with_capacity(count);
        let mut handlers = Vec::with_capacity(count);

        for _ in 0..count {
            let entry = Arc::new(MessageIrqEntryCore::new(
                self.allocate_message_id(),
                kind,
                flags,
                MessageVectorRoute {
                    cpu_id,
                    apic_id,
                    vector: 0,
                },
            ));
            handlers.push(Self::message_entry_handler(entry.clone()));
            entries.push(entry);
        }

        let vectors = Self::percpu_irq_managers()
            .get(cpu_id)
            .ok_or(IrqError::InvalidParameter)?
            .lock()
            .alloc_handlers_contiguous_aligned(handlers, count)?;

        for (entry, vector) in entries.iter().zip(vectors.into_iter()) {
            entry.retarget(cpu_id, apic_id, vector);
        }

        Ok(entries)
    }

    /// Allocate one possibly scattered message entry set across CPUs.
    fn allocate_message_entries_scattered(
        &'static self,
        request: MessageIrqRequest,
        flags: MessageIrqFlags,
    ) -> IrqResult<Vec<Arc<MessageIrqEntryCore>>> {
        let cpu_count = PerCpu::count();
        if cpu_count == 0 {
            return Err(IrqError::OutOfResources);
        }

        let base_cpu = request
            .target_cpu
            .unwrap_or(PerCpu::id().min(cpu_count - 1));
        let mut entries = Vec::with_capacity(request.count);

        for index in 0..request.count {
            let cpu_id = if request.target_cpu.is_some() || !request.allow_spread {
                base_cpu
            } else {
                (base_cpu + index) % cpu_count
            };
            let apic_id = PerCpu::lapic_id_of(cpu_id).ok_or(IrqError::InvalidParameter)?;
            let entry = Arc::new(MessageIrqEntryCore::new(
                self.allocate_message_id(),
                request.kind,
                flags,
                MessageVectorRoute {
                    cpu_id,
                    apic_id,
                    vector: 0,
                },
            ));
            let vector = match Self::percpu_irq_managers().get(cpu_id) {
                Some(manager) => manager
                    .lock()
                    .alloc_handler(Self::message_entry_handler(entry.clone())),
                None => Err(IrqError::InvalidParameter),
            };
            let vector = match vector {
                Ok(vector) => vector,
                Err(err) => {
                    for allocated in &entries {
                        let _ = self.release_message_entry(allocated);
                    }
                    return Err(err);
                }
            };
            entry.retarget(cpu_id, apic_id, vector);
            entries.push(entry);
        }

        Ok(entries)
    }

    /// Release the local vector assigned to one synthetic message entry.
    fn release_message_entry(&self, entry: &Arc<MessageIrqEntryCore>) -> IrqResult {
        let descriptor = entry.descriptor();
        self.message_ids.recycle(entry.message_id());
        Self::percpu_irq_managers()
            .get(descriptor.cpu_id)
            .ok_or(IrqError::InvalidParameter)?
            .lock()
            .free_handler(descriptor.vector)
    }

    /// Move one message entry to a new destination CPU and vector.
    fn retarget_message_entry(
        &self,
        entry: &Arc<MessageIrqEntryCore>,
        cpu_id: usize,
    ) -> IrqResult<MessageIrqDescriptor> {
        let apic_id = PerCpu::lapic_id_of(cpu_id).ok_or(IrqError::InvalidParameter)?;
        let previous = entry.descriptor();
        let vector = Self::percpu_irq_managers()
            .get(cpu_id)
            .ok_or(IrqError::InvalidParameter)?
            .lock()
            .alloc_handler(Self::message_entry_handler(entry.clone()))?;
        let descriptor = entry.retarget(cpu_id, apic_id, vector);
        if let Some(manager) = Self::percpu_irq_managers().get(previous.cpu_id) {
            let _ = manager.lock().free_handler(previous.vector);
        }
        Ok(descriptor)
    }

    /// Dispatch one external shared IRQ and apply any resulting mask action.
    fn dispatch_shared_irq(&self, gsi: usize) -> IrqResult {
        let action = self.manager_gsi.lock().handle_irq(gsi)?;
        if matches!(action, IrqHandleAction::MaskLine) {
            self.toggle_ioapic_line(gsi, false)?;
        }
        Ok(())
    }

    /// Toggle one IOAPIC line after one shared-line state transition.
    fn toggle_ioapic_line(&self, irq: usize, enabled: bool) -> IrqResult {
        let gsi = irq as u32;
        self.with_ioapic(gsi, |ioapic| {
            ioapic.toggle(gsi, enabled);
            Ok(())
        })
    }

    /// Return one snapshot of one discoverable external IRQ line.
    fn irq_line_state(&self, irq: usize) -> IrqResult<IrqLineState> {
        if irq >= LAPIC_BASE {
            return Err(IrqError::InvalidIrq(irq));
        }
        self.manager_gsi.lock().line_state(irq)
    }

    /// Open one shared user-visible session on one external GSI.
    fn open_shared_session(
        &'static self,
        irq: usize,
        flags: IrqOpenFlags,
    ) -> IrqResult<ApicIrqSession> {
        if irq >= LAPIC_BASE {
            return Err(IrqError::InvalidIrq(irq));
        }
        self.with_ioapic(irq as u32, |_| Ok(()))?;
        let (first_session, core) = {
            let mut mux = self.manager_gsi.lock();
            let first = !mux.has_sessions(irq);
            let core = mux.open_user_session(irq, flags)?;
            (first, core)
        };
        if first_session {
            self.install_io_route(irq, PerCpu::id())?;
        }
        Ok(ApicIrqSession::new_line(self, irq, core))
    }

    /// Close one previously opened shared external-IRQ session.
    fn close_shared_session(&self, irq: usize, session_id: usize) -> IrqResult {
        self.close_irq_session(irq, session_id)
    }

    /// Record one session ACK and unmask the physical line when the in-flight
    /// level-triggered epoch is fully drained.
    fn ack_shared_session(
        &self,
        irq: usize,
        session_id: usize,
        epoch: u64,
        disposition: IrqAckDisposition,
    ) -> IrqResult<usize> {
        let outcome = self
            .manager_gsi
            .lock()
            .ack_session(irq, session_id, epoch, disposition)?;

        if outcome.should_unmask {
            let state = self.irq_line_state(irq)?;
            if state.flags.contains(IrqLineFlags::ADMIN_ENABLED) {
                self.toggle_ioapic_line(irq, true)?;
            }
        }

        Ok(outcome.pending_count)
    }

    /// Route one external IRQ line to the supplied CPU.
    fn route_irq_line(&self, irq: usize, cpu_id: usize) -> IrqResult {
        self.reroute_ioapic(irq as u32, cpu_id)
    }

    /// Record the administrative enabled state for one external line.
    fn set_irq_line_admin_enabled(&self, irq: usize, enabled: bool) -> IrqResult {
        self.manager_gsi.lock().set_admin_masked(irq, !enabled)?;
        if !enabled {
            self.toggle_ioapic_line(irq, false)?;
            return Ok(());
        }
        let state = self.irq_line_state(irq)?;
        if !state.flags.contains(IrqLineFlags::DELIVERY_BLOCKED) {
            self.toggle_ioapic_line(irq, true)?;
        }
        Ok(())
    }

    /// Install one fresh IOAPIC-to-local-vector route.
    fn install_io_route(&self, gsi_idx: usize, cpu_id: usize) -> IrqResult {
        let apic_id = PerCpu::lapic_id_of(cpu_id).ok_or(IrqError::InvalidParameter)?;
        let vector = Self::percpu_irq_managers()
            .get(cpu_id)
            .ok_or(IrqError::InvalidParameter)?
            .lock()
            .alloc_handler(self.route_closure(gsi_idx))?;

        {
            let mut routes = self.routes.lock();
            if gsi_idx >= routes.len() {
                return Err(IrqError::InvalidIrq(gsi_idx));
            }
            routes[gsi_idx] = Some(IoRoute { vector, cpu_id });
        }

        self.with_ioapic(gsi_idx as u32, |ioapic| {
            ioapic.map_vector(gsi_idx as u32, vector as u8, apic_id as u8);
            Ok(())
        })
    }

    /// Remove any installed IOAPIC route for the supplied GSI.
    fn remove_io_route(&self, gsi_idx: usize) -> IrqResult {
        let route = {
            let mut routes = self.routes.lock();
            if gsi_idx >= routes.len() {
                return Err(IrqError::InvalidIrq(gsi_idx));
            }
            routes[gsi_idx].take()
        };

        if let Some(route) = route {
            if let Some(manager) = Self::percpu_irq_managers().get(route.cpu_id) {
                let _ = manager.lock().free_handler(route.vector);
            }
            self.with_ioapic(gsi_idx as u32, |ioapic| {
                ioapic.map_vector(gsi_idx as u32, 0, 0);
                Ok(())
            })?;
        }
        Ok(())
    }

    /// Close one session across the shared-external-IRQ state machine.
    fn close_irq_session(&self, irq: usize, session_id: usize) -> IrqResult {
        if irq >= LAPIC_BASE {
            return Err(IrqError::NotSupported);
        }
        let last_session = {
            let mut mux = self.manager_gsi.lock();
            mux.close_session(irq, session_id)?;
            !mux.has_sessions(irq)
        };
        if last_session {
            self.remove_io_route(irq)?;
        }
        Ok(())
    }

    /// Register one handler on an external IRQ routed through the IOAPIC.
    fn register_ioapic_handler(&self, gsi: u32, handler: IrqHandler) -> IrqResult {
        self.with_ioapic(gsi, |_| Ok(()))?;
        let gsi_idx = gsi as usize;
        let first_handler = {
            let mut mux = self.manager_gsi.lock();
            let first = !mux.has_sessions(gsi_idx);
            mux.register_handler(gsi_idx, handler)?;
            first
        };

        if !first_handler {
            return Ok(());
        }

        self.install_io_route(gsi_idx, PerCpu::id())
    }

    /// Unregister one external IOAPIC-backed handler.
    fn unregister_ioapic_handler(&self, gsi: u32) -> IrqResult {
        self.with_ioapic(gsi, |_| Ok(()))?;
        let gsi_idx = gsi as usize;
        self.manager_gsi.lock().unregister_handler(gsi_idx)?;

        self.remove_io_route(gsi_idx)
    }

    /// Move one external IOAPIC-backed IRQ route to another CPU.
    fn reroute_ioapic(&self, gsi: u32, target_cpu: usize) -> IrqResult {
        self.with_ioapic(gsi, |_| Ok(()))?;
        let gsi_idx = gsi as usize;
        let apic_id = PerCpu::lapic_id_of(target_cpu).ok_or(IrqError::InvalidParameter)?;
        let new_vector = Self::percpu_irq_managers()
            .get(target_cpu)
            .ok_or(IrqError::InvalidParameter)?
            .lock()
            .alloc_handler(self.route_closure(gsi_idx))?;

        let old_route = {
            let mut routes = self.routes.lock();
            if gsi_idx >= routes.len() {
                return Err(IrqError::InvalidIrq(gsi_idx));
            }
            let old = routes[gsi_idx];
            routes[gsi_idx] = Some(IoRoute {
                vector: new_vector,
                cpu_id: target_cpu,
            });
            old
        };

        self.with_ioapic(gsi, |ioapic| {
            ioapic.map_vector(gsi, new_vector as u8, apic_id as u8);
            Ok(())
        })?;

        if let Some(old) = old_route {
            if let Some(manager) = Self::percpu_irq_managers().get(old.cpu_id) {
                let _ = manager.lock().free_handler(old.vector);
            }
        }
        Ok(())
    }

    /// Initialize the BSP LAPIC and publish the controller singleton.
    pub fn init_lapic_bsp() -> &'static Self {
        log::info!("[x86_64/apic] init BSP local APIC");
        let controller = Box::leak(Box::new(Self::new()));
        controller.register_local_ipi_handlers();
        Self::init_percpu_irq_managers();
        unsafe { LocalApic::init_bsp() }
        controller
    }

    /// Initialize the current AP's LAPIC state.
    pub fn init_lapic_ap() {
        log::debug!("[x86_64/apic] init AP local APIC");
        unsafe { LocalApic::init_ap() }
    }

    /// Verify local IPI delivery by sending one self-targeted custom vector.
    pub fn selftest_local_ipi(&'static self) -> IrqResult {
        const TEST_VECTOR: usize = APIC_IPI_CUSTOM_BASE + 1;
        const MAX_SPIN: usize = 200_000;

        LOCAL_IPI_SELFTEST_HITS.store(0, Ordering::Release);

        self.register_irq_handler(
            TEST_VECTOR,
            Box::new(|| {
                LOCAL_IPI_SELFTEST_HITS.fetch_add(1, Ordering::AcqRel);
            }),
        )?;

        let was_enabled = self.is_interrupt_enabled();
        if !was_enabled {
            self.enable_ic()?;
        }

        self.send_ipi(IpiReason::Custom(TEST_VECTOR), IpiTarget::Current)?;

        let mut spin = 0usize;
        while LOCAL_IPI_SELFTEST_HITS.load(Ordering::Acquire) == 0 && spin < MAX_SPIN {
            spin += 1;
            spin_loop();
        }

        let _ = self.unregister_irq_handler(TEST_VECTOR);
        if !was_enabled {
            let _ = self.disable_ic();
        }

        if LOCAL_IPI_SELFTEST_HITS.load(Ordering::Acquire) == 0 {
            log::error!("[x86_64/apic] local IPI self-test failed: no interrupt observed");
            return Err(IrqError::NotSupported);
        }

        log::info!("[x86_64/apic] local IPI self-test passed");
        Ok(())
    }

    /// Return the mutable LAPIC singleton.
    pub fn lapic<'a>() -> &'a mut LocalApic {
        unsafe { LocalApic::get() }
    }

    /// Register one LAPIC-local vector handler.
    pub fn register_lapic_handler(&self, vector: usize, handler: IrqHandler) -> IrqResult {
        if vector >= LAPIC_BASE {
            self.manager_lapic
                .lock()
                .register_handler(vector, handler)?;
            Ok(())
        } else {
            Err(IrqError::InvalidIrq(vector))
        }
    }

    /// Unregister one LAPIC-local vector handler.
    pub fn unregister_lapic_handler(&self, vector: usize) -> IrqResult {
        if vector >= LAPIC_BASE {
            self.manager_lapic.lock().unregister_handler(vector)
        } else {
            Err(IrqError::InvalidIrq(vector))
        }
    }
}

impl InterruptControllerTrait for Apic {
    type Line = ApicIrqLine;
    type Session = ApicIrqSession;
    type MessageBlock = ApicMessageBlock;

    fn controller_name(&self) -> &'static str {
        "APIC"
    }

    fn controller_info(&self) -> InterruptControllerInfo {
        const FEATURE_SHARED_SESSION: usize = 1 << 0;
        const FEATURE_IRQ_FASTPATH: usize = 1 << 1;
        const FEATURE_MESSAGE_IRQ: usize = 1 << 2;
        InterruptControllerInfo {
            line_count: self.io_apic_list.max_gsi() + 1,
            local_irq_base: LAPIC_BASE,
            cpu_count: PerCpu::count(),
            feature_bits: FEATURE_SHARED_SESSION | FEATURE_IRQ_FASTPATH | FEATURE_MESSAGE_IRQ,
        }
    }

    fn line(&'static self, irq: usize) -> IrqResult<Self::Line> {
        if irq >= LAPIC_BASE {
            return Err(IrqError::InvalidIrq(irq));
        }
        self.with_ioapic(irq as u32, |_| Ok(()))?;
        Ok(ApicIrqLine::new(self, irq))
    }

    fn allocate_message_block(
        &'static self,
        request: MessageIrqRequest,
    ) -> IrqResult<Self::MessageBlock> {
        if request.count == 0 {
            return Err(IrqError::InvalidParameter);
        }

        match request.kind {
            MessageIrqKind::Msi => {
                let cpu_id = request.target_cpu.unwrap_or(PerCpu::id());
                let flags = MessageIrqFlags::CONTIGUOUS;
                let entries = self.allocate_message_entries_contiguous(
                    MessageIrqKind::Msi,
                    request.count,
                    cpu_id,
                    flags,
                )?;
                Ok(ApicMessageBlock::new(
                    self,
                    MessageIrqKind::Msi,
                    flags,
                    entries,
                ))
            }
            MessageIrqKind::Msix => {
                let flags = MessageIrqFlags::PER_ENTRY_ROUTE;
                let entries = self.allocate_message_entries_scattered(request, flags)?;
                Ok(ApicMessageBlock::new(
                    self,
                    MessageIrqKind::Msix,
                    flags,
                    entries,
                ))
            }
        }
    }

    fn wait_for_interrupt(&self) {
        unsafe { asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }

    fn is_valid_irq(&self, irq: usize) -> bool {
        if irq >= LAPIC_BASE {
            return true;
        }
        self.io_apic_list.find(irq as u32).is_some()
    }

    fn enable_ic(&self) -> IrqResult {
        x86_64::instructions::interrupts::enable();
        Ok(())
    }

    fn disable_ic(&self) -> IrqResult {
        x86_64::instructions::interrupts::disable();
        Ok(())
    }

    fn end_of_interrupt(&self) -> IrqResult {
        Self::lapic().eoi();
        Ok(())
    }

    fn is_interrupt_enabled(&self) -> bool {
        let rflags: usize;
        unsafe {
            asm!("pushfq; pop {}", out(reg) rflags);
        }
        rflags & (1 << 9) != 0
    }

    fn mask_irq(&self, irq: usize) -> IrqResult {
        let gsi = irq as u32;
        self.with_ioapic(gsi, |ioapic| {
            ioapic.toggle(gsi, false);
            Ok(())
        })
    }

    fn unmask_irq(&self, irq: usize) -> IrqResult {
        let gsi = irq as u32;
        self.with_ioapic(gsi, |ioapic| {
            ioapic.toggle(gsi, true);
            Ok(())
        })
    }

    fn register_irq_handler(&self, irq: usize, handler: IrqHandler) -> IrqResult {
        if irq >= LAPIC_BASE {
            self.register_lapic_handler(irq, handler)
        } else {
            self.register_ioapic_handler(irq as u32, handler)
        }
    }

    fn unregister_irq_handler(&self, irq: usize) -> IrqResult {
        if irq >= LAPIC_BASE {
            self.unregister_lapic_handler(irq)
        } else {
            self.unregister_ioapic_handler(irq as u32)
        }
    }

    fn set_timer_isr(&self, handler: IrqHandler) -> IrqResult {
        let cpu_id = PerCpu::id();
        Self::percpu_irq_managers()
            .get(cpu_id)
            .ok_or(IrqError::InvalidParameter)?
            .lock()
            .set_timer_isr(handler)?;
        Ok(())
    }

    fn clear_timer_isr(&self) -> IrqResult {
        let cpu_id = PerCpu::id();
        Self::percpu_irq_managers()
            .get(cpu_id)
            .ok_or(IrqError::InvalidParameter)?
            .lock()
            .clear_timer_isr()
    }

    fn handle_irq(&self, irq: usize) -> IrqResult {
        // Acknowledge the local APIC first so the rest of the Rust dispatch
        // path can focus on logical shared-line state transitions.
        Self::lapic().eoi();
        let result = if irq == APIC_TIMER_INTERRUPT {
            let cpu_id = PerCpu::id();
            Self::percpu_irq_managers()
                .get(cpu_id)
                .ok_or(IrqError::InvalidParameter)?
                .lock()
                .handle_irq(irq)
        } else if irq >= LAPIC_BASE {
            self.manager_lapic.lock().handle_irq(irq).map(|_| ())
        } else {
            let cpu_id = PerCpu::id();
            Self::percpu_irq_managers()
                .get(cpu_id)
                .ok_or(IrqError::InvalidParameter)?
                .lock()
                .handle_irq(irq)
        };
        match result {
            Err(IrqError::InvalidIrq(_)) => Err(IrqError::InvalidIrq(irq)),
            Err(IrqError::NotSupported) => Ok(()),
            Err(IrqError::OutOfResources) => Ok(()),
            Err(IrqError::InvalidParameter) => Ok(()),
            Err(IrqError::WouldBlock) => Ok(()),
            Err(IrqError::Closed) => Ok(()),
            Err(IrqError::DeadlineUnsupported) => Ok(()),
            Ok(_) => Ok(()),
        }
    }

    fn send_ipi(&self, reason: IpiReason, dest: IpiTarget) -> IrqResult {
        let vector = match reason {
            IpiReason::Reschedule => APIC_IPI_RESCHEDULE as u8,
            IpiReason::FlushTlb => APIC_IPI_FLUSH_TLB as u8,
            IpiReason::Panic => APIC_IPI_PANIC as u8,
            IpiReason::Mailbox => APIC_IPI_MAILBOX as u8,
            IpiReason::Custom(vector) => {
                if vector > 0xff {
                    APIC_IPI_CUSTOM_BASE as u8
                } else {
                    vector as u8
                }
            }
        };
        Self::lapic().send_ipi(vector, dest);
        Ok(())
    }
}
