use super::*;

impl Process {
    /// Clone one routing endpoint for this process inbox.
    #[allow(dead_code)]
    pub fn inbox_sender(&self) -> ProcessInboxSender {
        self.inbox.sender()
    }

    #[allow(dead_code)]
    /// Return one receiving endpoint for the process inbox.
    pub fn inbox_receiver(&self) -> ProcessInboxReceiver {
        self.inbox.receiver()
    }

    /// Pop the oldest deferred message for `port_id`, if one exists.
    pub fn pop_pending_message(&self, port_id: u64) -> Option<Message> {
        let mut pending = self.pending_messages.write();
        let queue = pending.get_mut(&port_id)?;
        let message = queue.pop_front();
        if queue.is_empty() {
            pending.remove(&port_id);
        }
        message
    }

    /// Stash one unmatched queued message until its destination port is polled.
    pub fn push_pending_message(&self, queued: QueuedMessage) {
        self.pending_messages
            .write()
            .entry(queued.port_id())
            .or_insert_with(VecDeque::new)
            .push_back(queued.into_message());
    }

    /// Receive one IPC message for the supplied port asynchronously.
    #[allow(dead_code)]
    pub async fn recv_message(&self, port_id: u64) -> Result<crate::ipc::Message, RecvError> {
        if let Some(message) = self.pop_pending_message(port_id) {
            return Ok(message);
        }

        loop {
            let queued = self.inbox.recv().await?;
            if queued.port_id() == port_id {
                return Ok(queued.into_message());
            }
            self.push_pending_message(queued);
        }
    }

    /// Receive one IPC message for the supplied port without using the async
    /// runtime.
    #[allow(dead_code)]
    pub fn recv_message_blocking(&self, port_id: u64) -> Result<crate::ipc::Message, RecvError> {
        if let Some(message) = self.pop_pending_message(port_id) {
            return Ok(message);
        }

        loop {
            let queued = self.inbox.recv_blocking()?;
            if queued.port_id() == port_id {
                return Ok(queued.into_message());
            }
            self.push_pending_message(queued);
        }
    }

    /// Try to receive one IPC message for the supplied port immediately.
    #[allow(dead_code)]
    pub fn try_recv_message(&self, port_id: u64) -> Result<crate::ipc::Message, RecvError> {
        if let Some(message) = self.pop_pending_message(port_id) {
            return Ok(message);
        }

        loop {
            let queued = self.inbox.try_recv()?;
            if queued.port_id() == port_id {
                return Ok(queued.into_message());
            }
            self.push_pending_message(queued);
        }
    }

    /// Create one unicast IPC port bound to this process inbox.
    #[allow(dead_code)]
    pub fn create_unicast_port(&self) -> Result<Handle, ObjectError> {
        let port = UnicastPort::new();
        port.bind_receiver(self.pid(), self.inbox_sender())
            .map_err(|_| ObjectError::InvalidArgument)?;
        self.create_anonymous_object(
            Payload::new(port),
            Capability::SEND | Capability::READ | Capability::WRITE | Capability::EXECUTE,
            P_SEND_MSG | P_RECV_MSG | P_BIND_RECV | P_QUERY_STATE,
        )
    }

    /// Create one anonymous broadcast IPC port.
    #[allow(dead_code)]
    pub fn create_broadcast_port(&self) -> Result<Handle, ObjectError> {
        self.create_anonymous_object(
            Payload::new(BroadcastPort::new()),
            Capability::SEND | Capability::READ | Capability::WRITE | Capability::EXECUTE,
            P_SEND_MSG | P_RECV_MSG | P_SUBSCRIBE | P_UNSUBSCRIBE | P_QUERY_STATE,
        )
    }

    /// Create one anonymous bus IPC port.
    #[allow(dead_code)]
    pub fn create_bus_port(&self) -> Result<Handle, ObjectError> {
        self.create_anonymous_object(
            Payload::new(BusPort::new()),
            Capability::SEND | Capability::READ | Capability::WRITE | Capability::EXECUTE,
            P_PUBLISH | P_LISTEN | P_SUBSCRIBE | P_UNSUBSCRIBE | P_QUERY_STATE,
        )
    }

    /// Create one anonymous reply port.
    #[allow(dead_code)]
    pub fn create_reply_port(&self) -> Result<Handle, ObjectError> {
        self.create_anonymous_object(
            Payload::new(ReplyPort::new()),
            Capability::SEND | Capability::READ | Capability::EXECUTE,
            P_SEND_MSG | P_RECV_MSG | P_QUERY_STATE,
        )
    }
}
