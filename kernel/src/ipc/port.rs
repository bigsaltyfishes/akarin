use alloc::{boxed::Box, vec::Vec};
use core::{
    ops::Deref,
    sync::atomic::{AtomicU8, AtomicU64, Ordering},
};

use async_trait::async_trait;
use hashbrown::HashMap;
use libakarin_machine_core::sync::{NoOp, ScopedGuard};
use libakarin_object::{ControlPlane, Handle, ObjectError, ObjectSyscallContext, SyscallDispatch};
use libakarin_sync::{
    asynchronous::SendError,
    collections::IdAllocator,
    spin::{Once, SpinRwLock},
};
use libakarin_syscall::{IpcError, PortMethod, PortUserMessage, SyscallResult};
use thiserror::Error;

use super::{
    IpcInvokeFrame, Message, MessageValidationError, P_BIND_RECV, P_CLOSE_PORT, P_LISTEN,
    P_PUBLISH, P_QUERY_STATE, P_REBIND_RECV, P_RECV_MSG, P_SEND_MSG, P_SUBSCRIBE, P_UNSUBSCRIBE,
    PortUserMessageError, PortUserMessageExt, ProcessInboxSender, QueuedMessage,
};
use crate::{
    arch::guards::IrqSaveGuard, error::ObjectOrUnderlyingError, sched::process::ProcessControl,
};

type ProcessId = crate::sched::ProcessId;

fn unicast_port_ids() -> &'static IdAllocator<u64> {
    static IDS: Once<IdAllocator<u64>, ScopedGuard<NoOp>> = Once::new();
    IDS.get_or_else(|| IdAllocator::new(1, 1))
}

fn fanout_port_ids() -> &'static IdAllocator<u64> {
    static IDS: Once<IdAllocator<u64>, ScopedGuard<NoOp>> = Once::new();
    IDS.get_or_else(|| IdAllocator::new(1, 1))
}

type PortDispatchError = ObjectOrUnderlyingError<IpcError>;

/// Port distribution mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortKind {
    Unicast,
    Broadcast,
    Bus,
}

/// Runtime lifecycle state of one port object.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortState {
    Open = 0,
    Frozen = 1,
    Closed = 2,
}

impl PortState {
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Open,
            1 => Self::Frozen,
            2 => Self::Closed,
            _ => Self::Closed,
        }
    }
}

/// Snapshot of one port's delivery counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PortStats {
    pub accepted: u64,
    pub failed: u64,
}

/// Binding failure when wiring one port to one process inbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PortBindError {
    #[error("port already has a bound receiver")]
    AlreadyBound,
    #[error("process is already subscribed")]
    AlreadySubscribed,
    #[error("process is not subscribed")]
    NotSubscribed,
    #[error("port is not open")]
    Closed,
}

/// Message delivery failure for one port send attempt.
#[derive(Debug, Error)]
pub enum PortSendError {
    #[error("port has no bound receiver")]
    Unbound(Message),
    #[error("port is not open")]
    Closed(Message),
    #[error("receiver inbox closed")]
    ReceiverClosed(Message),
    #[error("fan-out clone failed")]
    CloneFailed {
        message: Message,
        error: ObjectError,
    },
    #[error("message is not transferable")]
    InvalidMessage {
        message: Message,
        reason: MessageValidationError,
    },
}

impl PortSendError {
    pub fn ipc_error(self) -> IpcError {
        match self {
            Self::Unbound(_) => IpcError::Unbound,
            Self::Closed(_) => IpcError::PortClosed,
            Self::ReceiverClosed(_) => IpcError::ReceiverClosed,
            Self::CloneFailed { .. } | Self::InvalidMessage { .. } => IpcError::InvalidMessage,
        }
    }
}

impl PortBindError {
    fn ipc_error(self) -> IpcError {
        match self {
            Self::AlreadyBound => IpcError::AlreadyBound,
            Self::AlreadySubscribed => IpcError::AlreadySubscribed,
            Self::NotSubscribed => IpcError::NotSubscribed,
            Self::Closed => IpcError::PortClosed,
        }
    }
}

#[derive(Clone)]
struct Route {
    pid: ProcessId,
    inbox: ProcessInboxSender,
}

/// A many-sender, one-receiver IPC port.
pub struct UnicastPort {
    id: u64,
    state: AtomicU8,
    accepted: AtomicU64,
    failed: AtomicU64,
    receiver: SpinRwLock<Option<Route>, IrqSaveGuard>,
}

impl UnicastPort {
    /// Create one unbound unicast port.
    pub fn new() -> Self {
        Self {
            id: unicast_port_ids().allocate(),
            state: AtomicU8::new(PortState::Open as u8),
            accepted: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            receiver: SpinRwLock::new(None),
        }
    }

    /// Return the stable route identifier used inside destination inboxes.
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Return this object's port kind.
    pub const fn kind(&self) -> PortKind {
        PortKind::Unicast
    }

    /// Return the current lifecycle state.
    pub fn state(&self) -> PortState {
        PortState::from_u8(self.state.load(Ordering::Acquire))
    }

    /// Return one snapshot of delivery counters.
    pub fn stats(&self) -> PortStats {
        PortStats {
            accepted: self.accepted.load(Ordering::Acquire),
            failed: self.failed.load(Ordering::Acquire),
        }
    }

    /// Return the currently bound receiver pid, if any.
    pub fn receiver_pid(&self) -> Option<ProcessId> {
        self.receiver.read().as_ref().map(|route| route.pid)
    }

    /// Bind this port to one process inbox for the first time.
    pub fn bind_receiver(
        &self,
        pid: ProcessId,
        inbox: ProcessInboxSender,
    ) -> Result<(), PortBindError> {
        if self.state() != PortState::Open {
            return Err(PortBindError::Closed);
        }

        let mut receiver = self.receiver.write();
        if receiver.is_some() {
            return Err(PortBindError::AlreadyBound);
        }

        *receiver = Some(Route { pid, inbox });
        Ok(())
    }

    /// Replace the currently bound receiver.
    pub fn rebind_receiver(
        &self,
        pid: ProcessId,
        inbox: ProcessInboxSender,
    ) -> Result<(), PortBindError> {
        if self.state() != PortState::Open {
            return Err(PortBindError::Closed);
        }

        *self.receiver.write() = Some(Route { pid, inbox });
        Ok(())
    }

    /// Freeze this port and reject subsequent sends.
    pub fn freeze(&self) {
        self.state.store(PortState::Frozen as u8, Ordering::Release);
    }

    /// Close this port and detach its receiver.
    pub fn close(&self) {
        self.state.store(PortState::Closed as u8, Ordering::Release);
        self.receiver.write().take();
    }

    /// Send one message through the currently bound route.
    pub async fn send(&self, message: Message) -> Result<(), PortSendError> {
        let RouteMessage { route, message } = self.prepare_route(message)?;
        match route.inbox.send(QueuedMessage::new(self.id, message)).await {
            Ok(()) => {
                self.accepted.fetch_add(1, Ordering::AcqRel);
                Ok(())
            }
            Err(SendError::Closed(queued)) => {
                self.failed.fetch_add(1, Ordering::AcqRel);
                Err(PortSendError::ReceiverClosed(queued.into_message()))
            }
        }
    }

    /// Send one message without using the async runtime.
    pub fn send_blocking(&self, message: Message) -> Result<(), PortSendError> {
        let RouteMessage { route, message } = self.prepare_route(message)?;
        match route
            .inbox
            .send_blocking(QueuedMessage::new(self.id, message))
        {
            Ok(()) => {
                self.accepted.fetch_add(1, Ordering::AcqRel);
                Ok(())
            }
            Err(SendError::Closed(queued)) => {
                self.failed.fetch_add(1, Ordering::AcqRel);
                Err(PortSendError::ReceiverClosed(queued.into_message()))
            }
        }
    }

    fn prepare_route(&self, message: Message) -> Result<RouteMessage, PortSendError> {
        if self.state() != PortState::Open {
            self.failed.fetch_add(1, Ordering::AcqRel);
            return Err(PortSendError::Closed(message));
        }

        if let Err(reason) = message.validate() {
            self.failed.fetch_add(1, Ordering::AcqRel);
            return Err(PortSendError::InvalidMessage { message, reason });
        }

        let Some(route) = self.receiver.read().as_ref().cloned() else {
            self.failed.fetch_add(1, Ordering::AcqRel);
            return Err(PortSendError::Unbound(message));
        };

        Ok(RouteMessage { route, message })
    }
}

impl Drop for UnicastPort {
    fn drop(&mut self) {
        unicast_port_ids().recycle(self.id);
    }
}

struct RouteMessage {
    route: Route,
    message: Message,
}

struct FanoutPort {
    id: u64,
    state: AtomicU8,
    accepted: AtomicU64,
    failed: AtomicU64,
    subscribers: SpinRwLock<HashMap<ProcessId, ProcessInboxSender>, IrqSaveGuard>,
}

impl FanoutPort {
    fn new() -> Self {
        Self {
            id: fanout_port_ids().allocate(),
            state: AtomicU8::new(PortState::Open as u8),
            accepted: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            subscribers: SpinRwLock::new(HashMap::new()),
        }
    }

    fn id(&self) -> u64 {
        self.id
    }

    fn state(&self) -> PortState {
        PortState::from_u8(self.state.load(Ordering::Acquire))
    }

    fn stats(&self) -> PortStats {
        PortStats {
            accepted: self.accepted.load(Ordering::Acquire),
            failed: self.failed.load(Ordering::Acquire),
        }
    }

    fn subscriber_count(&self) -> usize {
        self.subscribers.read().len()
    }

    fn is_subscribed(&self, pid: ProcessId) -> bool {
        self.subscribers.read().contains_key(&pid)
    }

    fn subscribe(&self, pid: ProcessId, inbox: ProcessInboxSender) -> Result<(), PortBindError> {
        if self.state() != PortState::Open {
            return Err(PortBindError::Closed);
        }

        let previous = self.subscribers.write().insert(pid, inbox);
        if previous.is_some() {
            return Err(PortBindError::AlreadySubscribed);
        }

        Ok(())
    }

    fn unsubscribe(&self, pid: ProcessId) -> Result<(), PortBindError> {
        match self.subscribers.write().remove(&pid) {
            Some(_) => Ok(()),
            None => Err(PortBindError::NotSubscribed),
        }
    }

    fn freeze(&self) {
        self.state.store(PortState::Frozen as u8, Ordering::Release);
    }

    fn close(&self) {
        self.state.store(PortState::Closed as u8, Ordering::Release);
        self.subscribers.write().clear();
    }

    fn validate_fanout(&self, message: Message) -> Result<Vec<RouteMessage>, PortSendError> {
        if self.state() != PortState::Open {
            self.failed.fetch_add(1, Ordering::AcqRel);
            return Err(PortSendError::Closed(message));
        }

        if let Err(reason) = message.validate() {
            self.failed.fetch_add(1, Ordering::AcqRel);
            return Err(PortSendError::InvalidMessage { message, reason });
        }

        let subscribers = self
            .subscribers
            .read()
            .iter()
            .map(|(pid, inbox)| Route {
                pid: *pid,
                inbox: inbox.clone(),
            })
            .collect::<Vec<_>>();

        if subscribers.is_empty() {
            return Ok(Vec::new());
        }

        let mut deliveries = Vec::with_capacity(subscribers.len());
        for route in subscribers.iter().take(subscribers.len() - 1) {
            let cloned = match message.try_clone_for_fanout() {
                Ok(clone) => clone,
                Err(error) => {
                    self.failed.fetch_add(1, Ordering::AcqRel);
                    return Err(PortSendError::CloneFailed { message, error });
                }
            };
            deliveries.push(RouteMessage {
                route: (*route).clone(),
                message: cloned,
            });
        }

        deliveries.push(RouteMessage {
            route: subscribers
                .last()
                .cloned()
                .expect("non-empty subscriber list lost tail route"),
            message,
        });

        Ok(deliveries)
    }

    async fn send(&self, message: Message) -> Result<(), PortSendError> {
        let deliveries = self.validate_fanout(message)?;
        if deliveries.is_empty() {
            self.accepted.fetch_add(1, Ordering::AcqRel);
            return Ok(());
        }

        for delivery in deliveries {
            match delivery
                .route
                .inbox
                .send(QueuedMessage::new(self.id, delivery.message))
                .await
            {
                Ok(()) => {}
                Err(SendError::Closed(queued)) => {
                    let _ = self.unsubscribe(delivery.route.pid);
                    self.failed.fetch_add(1, Ordering::AcqRel);
                    return Err(PortSendError::ReceiverClosed(queued.into_message()));
                }
            }
        }

        self.accepted.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn send_blocking(&self, message: Message) -> Result<(), PortSendError> {
        let deliveries = self.validate_fanout(message)?;
        if deliveries.is_empty() {
            self.accepted.fetch_add(1, Ordering::AcqRel);
            return Ok(());
        }

        for delivery in deliveries {
            match delivery
                .route
                .inbox
                .send_blocking(QueuedMessage::new(self.id, delivery.message))
            {
                Ok(()) => {}
                Err(SendError::Closed(queued)) => {
                    let _ = self.unsubscribe(delivery.route.pid);
                    self.failed.fetch_add(1, Ordering::AcqRel);
                    return Err(PortSendError::ReceiverClosed(queued.into_message()));
                }
            }
        }

        self.accepted.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

impl Drop for FanoutPort {
    fn drop(&mut self) {
        fanout_port_ids().recycle(self.id);
    }
}

/// Fan-out broadcast port.
pub struct BroadcastPort {
    inner: FanoutPort,
}

impl BroadcastPort {
    /// Create one empty broadcast port.
    pub fn new() -> Self {
        Self {
            inner: FanoutPort::new(),
        }
    }

    /// Return the stable route identifier used inside destination inboxes.
    pub fn id(&self) -> u64 {
        self.inner.id()
    }

    /// Return this object's port kind.
    pub const fn kind(&self) -> PortKind {
        PortKind::Broadcast
    }

    /// Return the current lifecycle state.
    pub fn state(&self) -> PortState {
        self.inner.state()
    }

    /// Return one snapshot of delivery counters.
    pub fn stats(&self) -> PortStats {
        self.inner.stats()
    }

    /// Return the number of subscribed receivers.
    pub fn subscriber_count(&self) -> usize {
        self.inner.subscriber_count()
    }

    /// Return whether `pid` is subscribed.
    pub fn is_subscribed(&self, pid: ProcessId) -> bool {
        self.inner.is_subscribed(pid)
    }

    /// Add one process subscriber.
    pub fn subscribe(
        &self,
        pid: ProcessId,
        inbox: ProcessInboxSender,
    ) -> Result<(), PortBindError> {
        self.inner.subscribe(pid, inbox)
    }

    /// Remove one process subscriber.
    pub fn unsubscribe(&self, pid: ProcessId) -> Result<(), PortBindError> {
        self.inner.unsubscribe(pid)
    }

    /// Freeze this port and reject subsequent sends.
    pub fn freeze(&self) {
        self.inner.freeze();
    }

    /// Close this port and clear the subscriber set.
    pub fn close(&self) {
        self.inner.close();
    }

    /// Send one message to every subscriber.
    pub async fn send(&self, message: Message) -> Result<(), PortSendError> {
        self.inner.send(message).await
    }

    /// Send one message to every subscriber without using the async runtime.
    pub fn send_blocking(&self, message: Message) -> Result<(), PortSendError> {
        self.inner.send_blocking(message)
    }
}

/// Multi-publisher fan-out bus port.
pub struct BusPort {
    inner: FanoutPort,
}

impl BusPort {
    /// Create one empty bus port.
    pub fn new() -> Self {
        Self {
            inner: FanoutPort::new(),
        }
    }

    /// Return the stable route identifier used inside destination inboxes.
    pub fn id(&self) -> u64 {
        self.inner.id()
    }

    /// Return this object's port kind.
    pub const fn kind(&self) -> PortKind {
        PortKind::Bus
    }

    /// Return the current lifecycle state.
    pub fn state(&self) -> PortState {
        self.inner.state()
    }

    /// Return one snapshot of delivery counters.
    pub fn stats(&self) -> PortStats {
        self.inner.stats()
    }

    /// Return the number of subscribed listeners.
    pub fn subscriber_count(&self) -> usize {
        self.inner.subscriber_count()
    }

    /// Return whether `pid` is listening on this bus.
    pub fn is_subscribed(&self, pid: ProcessId) -> bool {
        self.inner.is_subscribed(pid)
    }

    /// Add one listener.
    pub fn subscribe(
        &self,
        pid: ProcessId,
        inbox: ProcessInboxSender,
    ) -> Result<(), PortBindError> {
        self.inner.subscribe(pid, inbox)
    }

    /// Remove one listener.
    pub fn unsubscribe(&self, pid: ProcessId) -> Result<(), PortBindError> {
        self.inner.unsubscribe(pid)
    }

    /// Freeze this port and reject subsequent publishes.
    pub fn freeze(&self) {
        self.inner.freeze();
    }

    /// Close this port and clear the listener set.
    pub fn close(&self) {
        self.inner.close();
    }

    /// Publish one message to every listener.
    pub async fn publish(&self, message: Message) -> Result<(), PortSendError> {
        self.inner.send(message).await
    }

    /// Publish one message to every listener without using the async runtime.
    pub fn publish_blocking(&self, message: Message) -> Result<(), PortSendError> {
        self.inner.send_blocking(message)
    }
}

pub struct UnicastPortGuard<'a> {
    port: &'a UnicastPort,
    interface_caps: u32,
    mode: GuardMode,
}

pub struct BroadcastPortGuard<'a> {
    port: &'a BroadcastPort,
    interface_caps: u32,
    mode: GuardMode,
}

pub struct BusPortGuard<'a> {
    port: &'a BusPort,
    interface_caps: u32,
    mode: GuardMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuardMode {
    Read,
    Write,
    Execute,
    Agent,
    Admin,
}

impl GuardMode {
    /// Verify that one guard mode and one interface-capability set permit the
    /// requested object-invoke method.
    ///
    /// This keeps the generic access-mode matrix on the mode type itself so
    /// individual guard implementations only need to supply the object-local
    /// interface capability bit they require.
    fn require_method(
        self,
        interface_caps: u32,
        method: PortMethod,
        caps: u32,
    ) -> Result<(), ObjectError> {
        let mode_allows_method = match self {
            Self::Admin => true,
            Self::Agent => !matches!(method, PortMethod::RebindReceiver | PortMethod::Close),
            Self::Read => matches!(method, PortMethod::QueryState),
            Self::Write => matches!(
                method,
                PortMethod::BindReceiver | PortMethod::Subscribe | PortMethod::Unsubscribe
            ),
            Self::Execute => matches!(method, PortMethod::Send | PortMethod::Recv),
        };
        if !mode_allows_method {
            return Err(ObjectError::InsufficientCapabilities);
        }
        if interface_caps == u32::MAX || (interface_caps & caps) == caps {
            return Ok(());
        }
        Err(ObjectError::InsufficientCapabilities)
    }
}

/// Per-dispatch helper bound to one object-syscall caller context.
///
/// Route resolution and inbox receive logic fundamentally depend on the
/// calling process, so they live on this typed view instead of as free
/// functions at file scope.
struct PortCallContext<'a> {
    caller: &'a ObjectSyscallContext,
}

impl PortCallContext<'_> {
    /// Resolve one process handle operand, treating `u32::MAX` as the current
    /// calling process just like the port ABI requires.
    fn resolve_process(&self, slot: u32) -> Result<Handle, ObjectError> {
        if slot == u32::MAX {
            return self.caller.current_process();
        }
        self.caller.acquire_handle(slot)
    }

    /// Resolve one process route tuple that can be installed into a port.
    ///
    /// This reads the destination process metadata through the object system
    /// instead of reaching into scheduler globals directly.
    fn resolve_route(&self, slot: u32) -> Result<(ProcessId, ProcessInboxSender), ObjectError> {
        let process = self.resolve_process(slot)?;
        process.read_cp_with::<ProcessControl, _, _>(|process| {
            Ok::<_, ObjectError>((
                process.process_id(),
                process.mailbox_control().inbox_sender(),
            ))
        })?
    }

    /// Resolve one process operand to its stable pid through the control
    /// plane, accepting both published process objects and the task-private
    /// current-process handle.
    fn resolve_process_id(&self, slot: u32) -> Result<ProcessId, ObjectError> {
        let process = self.resolve_process(slot)?;
        process.read_cp_with::<ProcessControl, _, _>(|process| {
            Ok::<_, ObjectError>(process.process_id())
        })?
    }

    /// Return the current caller pid as seen through the process object.
    ///
    /// Fan-out ports use this for subscribe/listen authorization so the check
    /// stays coupled to the caller context that originated the invoke.
    fn current_process_id(&self) -> Result<ProcessId, ObjectError> {
        self.caller
            .current_process()?
            .read_cp_with::<ProcessControl, _, _>(|process| {
                Ok::<_, ObjectError>(process.process_id())
            })?
    }

    /// Receive one message for one logical port id using the process inbox's
    /// async receive path.
    ///
    /// Messages for other ports are pushed back into the per-process pending
    /// map so the caller observes FIFO order per logical port without losing
    /// unrelated traffic.
    async fn recv_async_for_port(&self, port_id: u64) -> Result<Message, PortDispatchError> {
        let process = self.caller.current_process()?;
        if let Some(message) = process.write_cp_with::<ProcessControl, _, _>(|process| {
            Ok::<_, ObjectError>(process.mailbox_control().pop_pending_message(port_id))
        })?? {
            return Ok(message);
        }

        let receiver = process.read_cp_with::<ProcessControl, _, _>(|process| {
            Ok::<_, ObjectError>(process.mailbox_control().inbox_receiver())
        })??;

        loop {
            let queued = receiver.recv().await.map_err(|err| match err {
                libakarin_sync::asynchronous::RecvError::Empty => {
                    PortDispatchError::Underlying(IpcError::WouldBlock)
                }
                libakarin_sync::asynchronous::RecvError::Closed => {
                    PortDispatchError::Underlying(IpcError::ReceiverClosed)
                }
            })?;

            if queued.port_id() == port_id {
                return Ok(queued.into_message());
            }

            process.write_cp_with::<ProcessControl, _, _>(|process| {
                process.mailbox_control().push_pending_message(queued);
            })?;
        }
    }

    /// Receive one message for one logical port id using the blocking inbox
    /// path used by kernel-internal callers.
    ///
    /// The requeue behavior mirrors the async path so both APIs preserve the
    /// same pending-message invariants.
    fn recv_blocking_for_port(&self, port_id: u64) -> Result<Message, ObjectError> {
        let process = self.caller.current_process()?;
        if let Some(message) = process.write_cp_with::<ProcessControl, _, _>(|process| {
            Ok(process.mailbox_control().pop_pending_message(port_id))
        })?? {
            return Ok(message);
        }

        let receiver = process.read_cp_with::<ProcessControl, _, _>(|process| {
            Ok(process.mailbox_control().inbox_receiver())
        })??;

        loop {
            let queued = receiver.recv_blocking().map_err(|err| match err {
                libakarin_sync::asynchronous::RecvError::Empty => ObjectError::ObjectNotFound,
                libakarin_sync::asynchronous::RecvError::Closed => ObjectError::ObjectDestroyed,
            })?;

            if queued.port_id() == port_id {
                return Ok(queued.into_message());
            }

            process.write_cp_with::<ProcessControl, _, _>(|process| {
                process.mailbox_control().push_pending_message(queued);
            })?;
        }
    }
}

impl UnicastPortGuard<'_> {
    /// Translate one port-subsystem error into the standard object-invoke
    /// result frame.
    ///
    /// All port guards intentionally share this conversion path so subsystem
    /// failures remain encoded identically across unicast, broadcast, and bus
    /// ports. Keeping the frame construction here avoids repeating the ABI
    /// packing details at every call site.
    fn dispatch_ipc_error(error: IpcError) -> Result<SyscallResult, ObjectError> {
        let words = IpcInvokeFrame::ipc_error(error);
        Ok(words.into())
    }

    fn dispatch_user_error(error: PortUserMessageError) -> Result<SyscallResult, ObjectError> {
        match error.into_object_or_underlying() {
            Ok(error) => Err(error),
            Err(error) => Self::dispatch_ipc_error(error),
        }
    }

    fn dispatch_port_error(error: PortDispatchError) -> Result<SyscallResult, ObjectError> {
        match error.into_object_or_underlying() {
            Ok(error) => Err(error),
            Err(error) => Self::dispatch_ipc_error(error),
        }
    }
}

impl Deref for UnicastPortGuard<'_> {
    type Target = UnicastPort;

    fn deref(&self) -> &Self::Target {
        self.port
    }
}

impl Deref for BroadcastPortGuard<'_> {
    type Target = BroadcastPort;

    fn deref(&self) -> &Self::Target {
        self.port
    }
}

impl Deref for BusPortGuard<'_> {
    type Target = BusPort;

    fn deref(&self) -> &Self::Target {
        self.port
    }
}

impl ControlPlane for UnicastPort {
    type ReadGuard<'a>
        = UnicastPortGuard<'a>
    where
        Self: 'a;
    type WriteGuard<'a>
        = UnicastPortGuard<'a>
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = UnicastPortGuard<'a>
    where
        Self: 'a;
    type AgentGuard<'a>
        = UnicastPortGuard<'a>
    where
        Self: 'a;
    type AdminGuard<'a>
        = UnicastPortGuard<'a>
    where
        Self: 'a;

    fn read(&self, interface_caps: u32) -> Self::ReadGuard<'_> {
        UnicastPortGuard {
            port: self,
            interface_caps,
            mode: GuardMode::Read,
        }
    }

    fn write(&self, interface_caps: u32) -> Self::WriteGuard<'_> {
        UnicastPortGuard {
            port: self,
            interface_caps,
            mode: GuardMode::Write,
        }
    }

    fn execute(&self, interface_caps: u32) -> Self::ExecuteGuard<'_> {
        UnicastPortGuard {
            port: self,
            interface_caps,
            mode: GuardMode::Execute,
        }
    }

    fn agent(&self, interface_caps: u32) -> Self::AgentGuard<'_> {
        UnicastPortGuard {
            port: self,
            interface_caps,
            mode: GuardMode::Agent,
        }
    }

    fn admin(&self, interface_caps: u32) -> Self::AdminGuard<'_> {
        UnicastPortGuard {
            port: self,
            interface_caps,
            mode: GuardMode::Admin,
        }
    }
}

impl ControlPlane for BroadcastPort {
    type ReadGuard<'a>
        = BroadcastPortGuard<'a>
    where
        Self: 'a;
    type WriteGuard<'a>
        = BroadcastPortGuard<'a>
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = BroadcastPortGuard<'a>
    where
        Self: 'a;
    type AgentGuard<'a>
        = BroadcastPortGuard<'a>
    where
        Self: 'a;
    type AdminGuard<'a>
        = BroadcastPortGuard<'a>
    where
        Self: 'a;

    fn read(&self, interface_caps: u32) -> Self::ReadGuard<'_> {
        BroadcastPortGuard {
            port: self,
            interface_caps,
            mode: GuardMode::Read,
        }
    }

    fn write(&self, interface_caps: u32) -> Self::WriteGuard<'_> {
        BroadcastPortGuard {
            port: self,
            interface_caps,
            mode: GuardMode::Write,
        }
    }

    fn execute(&self, interface_caps: u32) -> Self::ExecuteGuard<'_> {
        BroadcastPortGuard {
            port: self,
            interface_caps,
            mode: GuardMode::Execute,
        }
    }

    fn agent(&self, interface_caps: u32) -> Self::AgentGuard<'_> {
        BroadcastPortGuard {
            port: self,
            interface_caps,
            mode: GuardMode::Agent,
        }
    }

    fn admin(&self, interface_caps: u32) -> Self::AdminGuard<'_> {
        BroadcastPortGuard {
            port: self,
            interface_caps,
            mode: GuardMode::Admin,
        }
    }
}

impl ControlPlane for BusPort {
    type ReadGuard<'a>
        = BusPortGuard<'a>
    where
        Self: 'a;
    type WriteGuard<'a>
        = BusPortGuard<'a>
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = BusPortGuard<'a>
    where
        Self: 'a;
    type AgentGuard<'a>
        = BusPortGuard<'a>
    where
        Self: 'a;
    type AdminGuard<'a>
        = BusPortGuard<'a>
    where
        Self: 'a;

    fn read(&self, interface_caps: u32) -> Self::ReadGuard<'_> {
        BusPortGuard {
            port: self,
            interface_caps,
            mode: GuardMode::Read,
        }
    }

    fn write(&self, interface_caps: u32) -> Self::WriteGuard<'_> {
        BusPortGuard {
            port: self,
            interface_caps,
            mode: GuardMode::Write,
        }
    }

    fn execute(&self, interface_caps: u32) -> Self::ExecuteGuard<'_> {
        BusPortGuard {
            port: self,
            interface_caps,
            mode: GuardMode::Execute,
        }
    }

    fn agent(&self, interface_caps: u32) -> Self::AgentGuard<'_> {
        BusPortGuard {
            port: self,
            interface_caps,
            mode: GuardMode::Agent,
        }
    }

    fn admin(&self, interface_caps: u32) -> Self::AdminGuard<'_> {
        BusPortGuard {
            port: self,
            interface_caps,
            mode: GuardMode::Admin,
        }
    }
}

#[async_trait]
impl SyscallDispatch<ObjectSyscallContext> for UnicastPortGuard<'_> {
    async fn dispatch(
        &self,
        caller: &ObjectSyscallContext,
        method_id: usize,
        arg1: usize,
        _arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let Ok(method) = PortMethod::try_from(method_id) else {
            return Self::dispatch_ipc_error(IpcError::InvalidArgument);
        };
        let context = PortCallContext { caller };
        match method {
            PortMethod::QueryState => {
                self.mode
                    .require_method(self.interface_caps, method, P_QUERY_STATE)?;
                let stats = self.port.stats();
                Ok(IpcInvokeFrame::ok([
                    self.port.kind() as usize,
                    self.port.state() as usize,
                    stats.accepted as usize,
                    stats.failed as usize,
                    self.port.receiver_pid().unwrap_or(usize::MAX as u64) as usize,
                ])
                .into())
            }
            PortMethod::Send => {
                self.mode
                    .require_method(self.interface_caps, method, P_SEND_MSG)?;
                let desc = match PortUserMessage::read(caller, arg1) {
                    Ok(desc) => desc,
                    Err(_) => return Self::dispatch_ipc_error(IpcError::Fault),
                };
                let (message, transfer) = match desc.into_message(caller) {
                    Ok(message) => message,
                    Err(error) => return Self::dispatch_user_error(error),
                };
                if let Err(error) = self
                    .port
                    .send(message)
                    .await
                    .map_err(PortSendError::ipc_error)
                {
                    return Self::dispatch_ipc_error(error);
                }
                if let Err(error) = transfer.commit(caller) {
                    return Self::dispatch_user_error(error);
                }
                Ok(IpcInvokeFrame::empty_ok().into())
            }
            PortMethod::Recv => {
                self.mode
                    .require_method(self.interface_caps, method, P_RECV_MSG)?;
                let mut message = match context.recv_async_for_port(self.port.id()).await {
                    Ok(message) => message,
                    Err(error) => return Self::dispatch_port_error(error),
                };
                let desc = match PortUserMessage::read(caller, arg1) {
                    Ok(desc) => desc,
                    Err(_) => return Self::dispatch_ipc_error(IpcError::Fault),
                };
                match desc.write_message(caller, &mut message, arg1) {
                    Ok(()) => Ok(IpcInvokeFrame::empty_ok().into()),
                    Err(error) => Self::dispatch_user_error(error),
                }
            }
            PortMethod::BindReceiver => {
                self.mode
                    .require_method(self.interface_caps, method, P_BIND_RECV)?;
                let (pid, inbox) = context.resolve_route(arg1 as u32)?;
                if let Err(error) = self
                    .port
                    .bind_receiver(pid, inbox)
                    .map_err(PortBindError::ipc_error)
                {
                    return Self::dispatch_ipc_error(error);
                }
                Ok(IpcInvokeFrame::ok([pid as usize, 0, 0, 0, 0]).into())
            }
            PortMethod::RebindReceiver => {
                self.mode
                    .require_method(self.interface_caps, method, P_REBIND_RECV)?;
                let (pid, inbox) = context.resolve_route(arg1 as u32)?;
                if let Err(error) = self
                    .port
                    .rebind_receiver(pid, inbox)
                    .map_err(PortBindError::ipc_error)
                {
                    return Self::dispatch_ipc_error(error);
                }
                Ok(IpcInvokeFrame::ok([pid as usize, 0, 0, 0, 0]).into())
            }
            PortMethod::Close => {
                self.mode
                    .require_method(self.interface_caps, method, P_CLOSE_PORT)?;
                self.port.close();
                Ok(IpcInvokeFrame::empty_ok().into())
            }
            PortMethod::Freeze => {
                self.mode
                    .require_method(self.interface_caps, method, P_CLOSE_PORT)?;
                self.port.freeze();
                Ok(IpcInvokeFrame::empty_ok().into())
            }
            PortMethod::Subscribe | PortMethod::Unsubscribe => {
                Self::dispatch_ipc_error(IpcError::InvalidArgument)
            }
        }
    }
}

#[async_trait]
impl SyscallDispatch<ObjectSyscallContext> for BroadcastPortGuard<'_> {
    async fn dispatch(
        &self,
        caller: &ObjectSyscallContext,
        method_id: usize,
        arg1: usize,
        _arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let Ok(method) = PortMethod::try_from(method_id) else {
            return UnicastPortGuard::dispatch_ipc_error(IpcError::InvalidArgument);
        };
        let context = PortCallContext { caller };
        match method {
            PortMethod::QueryState => {
                self.mode
                    .require_method(self.interface_caps, method, P_QUERY_STATE)?;
                let stats = self.port.stats();
                Ok(IpcInvokeFrame::ok([
                    self.port.kind() as usize,
                    self.port.state() as usize,
                    stats.accepted as usize,
                    stats.failed as usize,
                    self.port.subscriber_count(),
                ])
                .into())
            }
            PortMethod::Send => {
                self.mode
                    .require_method(self.interface_caps, method, P_SEND_MSG)?;
                let desc = match PortUserMessage::read(caller, arg1) {
                    Ok(desc) => desc,
                    Err(_) => return UnicastPortGuard::dispatch_ipc_error(IpcError::Fault),
                };
                let (message, transfer) = match desc.into_message(caller) {
                    Ok(message) => message,
                    Err(error) => return UnicastPortGuard::dispatch_user_error(error),
                };
                if let Err(error) = self
                    .port
                    .send(message)
                    .await
                    .map_err(PortSendError::ipc_error)
                {
                    return UnicastPortGuard::dispatch_ipc_error(error);
                }
                if let Err(error) = transfer.commit(caller) {
                    return UnicastPortGuard::dispatch_user_error(error);
                }
                Ok(IpcInvokeFrame::empty_ok().into())
            }
            PortMethod::Recv => {
                self.mode
                    .require_method(self.interface_caps, method, P_RECV_MSG)?;
                let pid = context.current_process_id()?;
                if !self.port.is_subscribed(pid) {
                    return UnicastPortGuard::dispatch_ipc_error(IpcError::NotSubscribed);
                }
                let mut message = match context.recv_async_for_port(self.port.id()).await {
                    Ok(message) => message,
                    Err(error) => return UnicastPortGuard::dispatch_port_error(error),
                };
                let desc = match PortUserMessage::read(caller, arg1) {
                    Ok(desc) => desc,
                    Err(_) => return UnicastPortGuard::dispatch_ipc_error(IpcError::Fault),
                };
                match desc.write_message(caller, &mut message, arg1) {
                    Ok(()) => Ok(IpcInvokeFrame::empty_ok().into()),
                    Err(error) => UnicastPortGuard::dispatch_user_error(error),
                }
            }
            PortMethod::Subscribe => {
                self.mode
                    .require_method(self.interface_caps, method, P_SUBSCRIBE)?;
                let (pid, inbox) = context.resolve_route(arg1 as u32)?;
                if let Err(error) = self
                    .port
                    .subscribe(pid, inbox)
                    .map_err(PortBindError::ipc_error)
                {
                    return UnicastPortGuard::dispatch_ipc_error(error);
                }
                Ok(IpcInvokeFrame::ok([pid as usize, 0, 0, 0, 0]).into())
            }
            PortMethod::Unsubscribe => {
                self.mode
                    .require_method(self.interface_caps, method, P_UNSUBSCRIBE)?;
                let pid = context.resolve_process_id(arg1 as u32)?;
                if let Err(error) = self.port.unsubscribe(pid).map_err(PortBindError::ipc_error) {
                    return UnicastPortGuard::dispatch_ipc_error(error);
                }
                Ok(IpcInvokeFrame::ok([pid as usize, 0, 0, 0, 0]).into())
            }
            PortMethod::Close => {
                self.mode
                    .require_method(self.interface_caps, method, P_CLOSE_PORT)?;
                self.port.close();
                Ok(IpcInvokeFrame::empty_ok().into())
            }
            PortMethod::Freeze => {
                self.mode
                    .require_method(self.interface_caps, method, P_CLOSE_PORT)?;
                self.port.freeze();
                Ok(IpcInvokeFrame::empty_ok().into())
            }
            PortMethod::BindReceiver | PortMethod::RebindReceiver => {
                UnicastPortGuard::dispatch_ipc_error(IpcError::InvalidArgument)
            }
        }
    }
}

#[async_trait]
impl SyscallDispatch<ObjectSyscallContext> for BusPortGuard<'_> {
    async fn dispatch(
        &self,
        caller: &ObjectSyscallContext,
        method_id: usize,
        arg1: usize,
        _arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let Ok(method) = PortMethod::try_from(method_id) else {
            return UnicastPortGuard::dispatch_ipc_error(IpcError::InvalidArgument);
        };
        let context = PortCallContext { caller };
        match method {
            PortMethod::QueryState => {
                self.mode
                    .require_method(self.interface_caps, method, P_QUERY_STATE)?;
                let stats = self.port.stats();
                Ok(IpcInvokeFrame::ok([
                    self.port.kind() as usize,
                    self.port.state() as usize,
                    stats.accepted as usize,
                    stats.failed as usize,
                    self.port.subscriber_count(),
                ])
                .into())
            }
            PortMethod::Send => {
                self.mode
                    .require_method(self.interface_caps, method, P_PUBLISH)?;
                let desc = match PortUserMessage::read(caller, arg1) {
                    Ok(desc) => desc,
                    Err(_) => return UnicastPortGuard::dispatch_ipc_error(IpcError::Fault),
                };
                let (message, transfer) = match desc.into_message(caller) {
                    Ok(message) => message,
                    Err(error) => return UnicastPortGuard::dispatch_user_error(error),
                };
                if let Err(error) = self
                    .port
                    .publish(message)
                    .await
                    .map_err(PortSendError::ipc_error)
                {
                    return UnicastPortGuard::dispatch_ipc_error(error);
                }
                if let Err(error) = transfer.commit(caller) {
                    return UnicastPortGuard::dispatch_user_error(error);
                }
                Ok(IpcInvokeFrame::empty_ok().into())
            }
            PortMethod::Recv => {
                self.mode
                    .require_method(self.interface_caps, method, P_LISTEN)?;
                let pid = context.current_process_id()?;
                if !self.port.is_subscribed(pid) {
                    return UnicastPortGuard::dispatch_ipc_error(IpcError::NotSubscribed);
                }
                let mut message = match context.recv_async_for_port(self.port.id()).await {
                    Ok(message) => message,
                    Err(error) => return UnicastPortGuard::dispatch_port_error(error),
                };
                let desc = match PortUserMessage::read(caller, arg1) {
                    Ok(desc) => desc,
                    Err(_) => return UnicastPortGuard::dispatch_ipc_error(IpcError::Fault),
                };
                match desc.write_message(caller, &mut message, arg1) {
                    Ok(()) => Ok(IpcInvokeFrame::empty_ok().into()),
                    Err(error) => UnicastPortGuard::dispatch_user_error(error),
                }
            }
            PortMethod::Subscribe => {
                self.mode
                    .require_method(self.interface_caps, method, P_SUBSCRIBE)?;
                let (pid, inbox) = context.resolve_route(arg1 as u32)?;
                if let Err(error) = self
                    .port
                    .subscribe(pid, inbox)
                    .map_err(PortBindError::ipc_error)
                {
                    return UnicastPortGuard::dispatch_ipc_error(error);
                }
                Ok(IpcInvokeFrame::ok([pid as usize, 0, 0, 0, 0]).into())
            }
            PortMethod::Unsubscribe => {
                self.mode
                    .require_method(self.interface_caps, method, P_UNSUBSCRIBE)?;
                let pid = context.resolve_process_id(arg1 as u32)?;
                if let Err(error) = self.port.unsubscribe(pid).map_err(PortBindError::ipc_error) {
                    return UnicastPortGuard::dispatch_ipc_error(error);
                }
                Ok(IpcInvokeFrame::ok([pid as usize, 0, 0, 0, 0]).into())
            }
            PortMethod::Close => {
                self.mode
                    .require_method(self.interface_caps, method, P_CLOSE_PORT)?;
                self.port.close();
                Ok(IpcInvokeFrame::empty_ok().into())
            }
            PortMethod::Freeze => {
                self.mode
                    .require_method(self.interface_caps, method, P_CLOSE_PORT)?;
                self.port.freeze();
                Ok(IpcInvokeFrame::empty_ok().into())
            }
            PortMethod::BindReceiver | PortMethod::RebindReceiver => {
                UnicastPortGuard::dispatch_ipc_error(IpcError::InvalidArgument)
            }
        }
    }
}
