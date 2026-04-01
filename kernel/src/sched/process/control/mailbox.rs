use alloc::sync::Arc;

use super::super::Process;
use crate::ipc::{Message, ProcessInboxReceiver, ProcessInboxSender, QueuedMessage};

/// Capability-scoped mailbox controller for one process.
#[derive(Clone)]
pub struct MailboxControl {
    process: Arc<Process>,
    supervisor_only: bool,
}

impl MailboxControl {
    /// Build one mailbox controller for the supplied process.
    pub fn new(process: Arc<Process>, supervisor_only: bool) -> Self {
        Self {
            process,
            supervisor_only,
        }
    }

    /// Return the wrapped process runtime object.
    pub fn process(&self) -> &Arc<Process> {
        &self.process
    }

    /// Return whether this controller is restricted to supervisor-only
    /// lifecycle methods.
    pub fn supervisor_only(&self) -> bool {
        self.supervisor_only
    }

    /// Return one sender for the process inbox.
    pub fn inbox_sender(&self) -> ProcessInboxSender {
        self.process.inbox_sender()
    }

    /// Return one receiver for the process inbox.
    pub fn inbox_receiver(&self) -> ProcessInboxReceiver {
        self.process.inbox_receiver()
    }

    /// Remove one pending message for the supplied logical port id.
    pub fn pop_pending_message(&self, port_id: u64) -> Option<Message> {
        self.process.pop_pending_message(port_id)
    }

    /// Requeue one message that does not belong to the current logical port.
    pub fn push_pending_message(&self, queued: QueuedMessage) {
        self.process.push_pending_message(queued);
    }
}
