use libakarin_sync::asynchronous::{Receiver, RecvError, Sender, unbounded};

use super::Message;

/// One message already routed to a specific port within a destination process.
#[derive(Debug)]
pub struct QueuedMessage {
    port_id: u64,
    message: Message,
}

impl QueuedMessage {
    /// Create one queued message for the supplied route id.
    pub fn new(port_id: u64, message: Message) -> Self {
        Self { port_id, message }
    }

    /// Return the destination port identifier inside the receiver process.
    pub const fn port_id(&self) -> u64 {
        self.port_id
    }

    /// Consume the envelope and return the underlying message.
    pub fn into_message(self) -> Message {
        self.message
    }
}

pub type ProcessInboxSender = Sender<QueuedMessage>;
pub type ProcessInboxReceiver = Receiver<QueuedMessage>;

/// One process-local IPC inbox.
///
/// Ports route messages into this inbox. The process owns the receive side,
/// while ports and other kernel subsystems retain cloned senders.
pub struct ProcessInbox {
    sender: ProcessInboxSender,
    receiver: ProcessInboxReceiver,
}

impl ProcessInbox {
    /// Create one empty process inbox backed by one unbounded kernel queue.
    pub fn new() -> Self {
        let (sender, receiver) = unbounded::<QueuedMessage>();
        Self { sender, receiver }
    }

    /// Clone one sending endpoint for routing into this inbox.
    pub fn sender(&self) -> ProcessInboxSender {
        self.sender.clone()
    }

    /// Clone one receiving endpoint for deferred per-process dispatch.
    pub fn receiver(&self) -> ProcessInboxReceiver {
        self.receiver.clone()
    }

    /// Receive one message asynchronously.
    pub async fn recv(&self) -> Result<QueuedMessage, RecvError> {
        self.receiver.recv().await
    }

    /// Receive one message without involving the async runtime.
    pub fn recv_blocking(&self) -> Result<QueuedMessage, RecvError> {
        self.receiver.recv_blocking()
    }

    /// Try to receive one message immediately.
    pub fn try_recv(&self) -> Result<QueuedMessage, RecvError> {
        self.receiver.try_recv()
    }
}
