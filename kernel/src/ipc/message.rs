use alloc::vec::Vec;

use bitflags::bitflags;
use libakarin_object::{Capability, Handle, ObjectError};

bitflags! {
    /// Message shape flags derived from the currently attached payload pieces.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct MessageFlags: u32 {
        const INLINE_DATA = 1 << 0;
        const ATTACHMENTS = 1 << 1;
        const BUFFER = 1 << 2;
        const REPLY_PORT = 1 << 3;
    }
}

/// One transport-neutral IPC message header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageHeader {
    txid: u64,
    proto_id: u32,
    opcode: u32,
    flags: MessageFlags,
}

impl MessageHeader {
    /// Build one header with empty shape flags.
    pub const fn new(txid: u64, proto_id: u32, opcode: u32) -> Self {
        Self {
            txid,
            proto_id,
            opcode,
            flags: MessageFlags::empty(),
        }
    }

    /// Return the transport transaction identifier.
    pub const fn txid(&self) -> u64 {
        self.txid
    }

    /// Return the application protocol identifier.
    pub const fn proto_id(&self) -> u32 {
        self.proto_id
    }

    /// Return the protocol-defined operation identifier.
    pub const fn opcode(&self) -> u32 {
        self.opcode
    }

    /// Return the derived message flags.
    pub const fn flags(&self) -> MessageFlags {
        self.flags
    }
}

/// Validation failure for one IPC message capability payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageValidationError {
    /// One transferred attachment does not carry `SEND`.
    AttachmentMissingSend { index: usize },
    /// The attached buffer descriptor does not carry `SEND`.
    BufferMissingSend,
    /// The attached reply port does not carry `SEND`.
    ReplyPortMissingSend,
}

/// One routed IPC message.
#[derive(Debug)]
pub struct Message {
    header: MessageHeader,
    inline_data: Vec<usize>,
    attachments: Vec<Handle>,
    buffer: Option<Handle>,
    reply_port: Option<Handle>,
}

impl Message {
    /// Create one empty message with the supplied header.
    pub fn new(header: MessageHeader) -> Self {
        Self {
            header,
            inline_data: Vec::new(),
            attachments: Vec::new(),
            buffer: None,
            reply_port: None,
        }
    }

    /// Return the immutable message header.
    pub const fn header(&self) -> MessageHeader {
        self.header
    }

    /// Return the inline word payload.
    pub fn inline_data(&self) -> &[usize] {
        &self.inline_data
    }

    /// Return the attached generic handles.
    pub fn attachments(&self) -> &[Handle] {
        &self.attachments
    }

    /// Return the optional buffer descriptor handle.
    pub fn buffer(&self) -> Option<&Handle> {
        self.buffer.as_ref()
    }

    /// Return the optional reply-port handle.
    pub fn reply_port(&self) -> Option<&Handle> {
        self.reply_port.as_ref()
    }

    /// Append one inline word.
    pub fn push_inline_word(&mut self, word: usize) {
        self.inline_data.push(word);
        self.sync_flags();
    }

    /// Replace the inline payload with the supplied words.
    pub fn set_inline_data(&mut self, words: Vec<usize>) {
        self.inline_data = words;
        self.sync_flags();
    }

    /// Attach one handle to this message.
    pub fn push_attachment(&mut self, handle: Handle) {
        self.attachments.push(handle);
        self.sync_flags();
    }

    /// Install one optional VMO buffer descriptor handle.
    pub fn set_buffer(&mut self, handle: Option<Handle>) {
        self.buffer = handle;
        self.sync_flags();
    }

    /// Install one optional reply-port handle.
    pub fn set_reply_port(&mut self, handle: Option<Handle>) {
        self.reply_port = handle;
        self.sync_flags();
    }

    /// Remove and return all attached generic handles.
    pub fn take_attachments(&mut self) -> Vec<Handle> {
        let handles = core::mem::take(&mut self.attachments);
        self.sync_flags();
        handles
    }

    /// Remove and return the optional buffer descriptor.
    pub fn take_buffer(&mut self) -> Option<Handle> {
        let handle = self.buffer.take();
        self.sync_flags();
        handle
    }

    /// Remove and return the optional reply port.
    pub fn take_reply_port(&mut self) -> Option<Handle> {
        let handle = self.reply_port.take();
        self.sync_flags();
        handle
    }

    /// Validate that every transferred capability can legally cross one IPC
    /// message boundary.
    pub fn validate(&self) -> Result<(), MessageValidationError> {
        for (index, handle) in self.attachments.iter().enumerate() {
            if !handle.capabilities().contains(Capability::SEND) {
                return Err(MessageValidationError::AttachmentMissingSend { index });
            }
        }

        if self
            .buffer
            .as_ref()
            .is_some_and(|handle| !handle.capabilities().contains(Capability::SEND))
        {
            return Err(MessageValidationError::BufferMissingSend);
        }

        if self
            .reply_port
            .as_ref()
            .is_some_and(|handle| !handle.capabilities().contains(Capability::SEND))
        {
            return Err(MessageValidationError::ReplyPortMissingSend);
        }

        Ok(())
    }

    /// Clone this message for fan-out delivery.
    ///
    /// Inline words are copied directly. Capability-bearing payloads are
    /// cloned handle-by-handle so each receiver gets its own handle value.
    /// This currently requires those handles to be clonable.
    pub fn try_clone_for_fanout(&self) -> Result<Self, ObjectError> {
        let mut attachments = Vec::with_capacity(self.attachments.len());
        for handle in &self.attachments {
            attachments.push(handle.try_clone()?);
        }

        let buffer = match self.buffer.as_ref() {
            Some(handle) => Some(handle.try_clone()?),
            None => None,
        };

        let reply_port = match self.reply_port.as_ref() {
            Some(handle) => Some(handle.try_clone()?),
            None => None,
        };

        Ok(Self {
            header: self.header,
            inline_data: self.inline_data.clone(),
            attachments,
            buffer,
            reply_port,
        })
    }

    fn sync_flags(&mut self) {
        let mut flags = MessageFlags::empty();
        if !self.inline_data.is_empty() {
            flags.insert(MessageFlags::INLINE_DATA);
        }
        if !self.attachments.is_empty() {
            flags.insert(MessageFlags::ATTACHMENTS);
        }
        if self.buffer.is_some() {
            flags.insert(MessageFlags::BUFFER);
        }
        if self.reply_port.is_some() {
            flags.insert(MessageFlags::REPLY_PORT);
        }
        self.header.flags = flags;
    }
}
