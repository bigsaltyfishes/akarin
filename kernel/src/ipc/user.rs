use alloc::{vec, vec::Vec};
use core::{mem::size_of, slice};

use libakarin_object::ObjectSyscallContext;
use libakarin_syscall::{INVALID_HANDLE_SLOT, IpcError, PortUserMessage};

use super::{Message, MessageHeader};
use crate::error::ObjectOrUnderlyingError;

pub type PortUserMessageError = ObjectOrUnderlyingError<IpcError>;

pub struct PortCapabilityTransfer {
    attachment_slots: Vec<u32>,
    buffer_slot: Option<u32>,
    reply_port_slot: Option<u32>,
}

impl PortCapabilityTransfer {
    fn empty() -> Self {
        Self {
            attachment_slots: Vec::new(),
            buffer_slot: None,
            reply_port_slot: None,
        }
    }

    pub fn commit(self, caller: &ObjectSyscallContext) -> Result<(), PortUserMessageError> {
        for slot in self.attachment_slots {
            caller
                .close_handle(slot)
                .map_err(PortUserMessageError::Object)?;
        }
        if let Some(slot) = self.buffer_slot {
            caller
                .close_handle(slot)
                .map_err(PortUserMessageError::Object)?;
        }
        if let Some(slot) = self.reply_port_slot {
            caller
                .close_handle(slot)
                .map_err(PortUserMessageError::Object)?;
        }
        Ok(())
    }
}

/// Kernel-side marshaling helpers for the shared IPC user descriptor.
pub trait PortUserMessageExt {
    /// Build one kernel message from this user descriptor.
    fn into_message(
        self,
        caller: &ObjectSyscallContext,
    ) -> Result<(Message, PortCapabilityTransfer), PortUserMessageError>;

    /// Serialize one kernel message back into the caller-provided descriptor.
    fn write_message(
        self,
        caller: &ObjectSyscallContext,
        message: &mut Message,
        ptr: usize,
    ) -> Result<(), PortUserMessageError>;
}

impl PortUserMessageExt for PortUserMessage {
    fn into_message(
        self,
        caller: &ObjectSyscallContext,
    ) -> Result<(Message, PortCapabilityTransfer), PortUserMessageError> {
        let mut message = Message::new(MessageHeader::new(self.txid, self.proto_id, self.opcode));
        let mut transfer = PortCapabilityTransfer::empty();

        if self.inline_words_len != 0 {
            let mut words = vec![0usize; self.inline_words_len];
            let bytes = unsafe {
                slice::from_raw_parts_mut(
                    words.as_mut_ptr().cast::<u8>(),
                    self.inline_words_len * size_of::<usize>(),
                )
            };
            caller
                .copy_from_user(self.inline_words_ptr, bytes)
                .map_err(|_| PortUserMessageError::Underlying(IpcError::Fault))?;
            message.set_inline_data(words);
        }

        if self.handle_slots_len != 0 {
            let mut slots = vec![0u32; self.handle_slots_len];
            let bytes = unsafe {
                slice::from_raw_parts_mut(
                    slots.as_mut_ptr().cast::<u8>(),
                    self.handle_slots_len * size_of::<u32>(),
                )
            };
            caller
                .copy_from_user(self.handle_slots_ptr, bytes)
                .map_err(|_| PortUserMessageError::Underlying(IpcError::Fault))?;
            for slot in slots {
                message.push_attachment(
                    caller
                        .acquire_handle(slot)
                        .map_err(PortUserMessageError::Object)?,
                );
                transfer.attachment_slots.push(slot);
            }
        }

        if self.buffer_slot != INVALID_HANDLE_SLOT {
            message.set_buffer(Some(
                caller
                    .acquire_handle(self.buffer_slot)
                    .map_err(PortUserMessageError::Object)?,
            ));
            transfer.buffer_slot = Some(self.buffer_slot);
        }

        if self.reply_port_slot != INVALID_HANDLE_SLOT {
            message.set_reply_port(Some(
                caller
                    .acquire_handle(self.reply_port_slot)
                    .map_err(PortUserMessageError::Object)?,
            ));
            transfer.reply_port_slot = Some(self.reply_port_slot);
        }

        Ok((message, transfer))
    }

    fn write_message(
        mut self,
        caller: &ObjectSyscallContext,
        message: &mut Message,
        ptr: usize,
    ) -> Result<(), PortUserMessageError> {
        if self.inline_words_len < message.inline_data().len()
            || self.handle_slots_len < message.attachments().len()
        {
            return Err(PortUserMessageError::Underlying(IpcError::BufferTooSmall));
        }

        let inline_len = message.inline_data().len();
        if inline_len != 0 {
            let bytes = unsafe {
                slice::from_raw_parts(
                    message.inline_data().as_ptr().cast::<u8>(),
                    inline_len * size_of::<usize>(),
                )
            };
            caller
                .copy_to_user(self.inline_words_ptr, bytes)
                .map_err(|_| PortUserMessageError::Underlying(IpcError::Fault))?;
        }

        let handle_count = message.attachments().len();
        if handle_count != 0 {
            let mut slots = vec![0u32; handle_count];
            for (slot_out, handle) in slots.iter_mut().zip(message.take_attachments()) {
                *slot_out = caller
                    .install_handle(handle)
                    .map_err(PortUserMessageError::Object)?;
            }
            let bytes = unsafe {
                slice::from_raw_parts(slots.as_ptr().cast::<u8>(), handle_count * size_of::<u32>())
            };
            caller
                .copy_to_user(self.handle_slots_ptr, bytes)
                .map_err(|_| PortUserMessageError::Underlying(IpcError::Fault))?;
        }

        self.txid = message.header().txid();
        self.proto_id = message.header().proto_id();
        self.opcode = message.header().opcode();
        self.inline_words_len = inline_len;
        self.handle_slots_len = handle_count;
        self.buffer_slot = match message.take_buffer() {
            Some(handle) => caller
                .install_handle(handle)
                .map_err(PortUserMessageError::Object)?,
            None => INVALID_HANDLE_SLOT,
        };
        self.reply_port_slot = match message.take_reply_port() {
            Some(handle) => caller
                .install_handle(handle)
                .map_err(PortUserMessageError::Object)?,
            None => INVALID_HANDLE_SLOT,
        };
        PortUserMessage::write(caller, ptr, &self)
            .map_err(|_| PortUserMessageError::Underlying(IpcError::Fault))
    }
}
