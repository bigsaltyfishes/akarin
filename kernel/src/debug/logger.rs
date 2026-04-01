//! Kernel logger and log sink registration.
//!
//! The logger keeps a bounded in-memory replay buffer and mirrors each record
//! to every registered output sink, such as the serial console or the EFI
//! framebuffer logger.

use core::{
    fmt::{self, Write},
    sync::atomic::{AtomicBool, Ordering},
};

use libakarin_object::{Capability, Handle, ObjectError};
use libakarin_sync::spin::SpinLock;
use log::{LevelFilter, Metadata, Record, SetLoggerError};

use crate::{arch::guards::IrqSaveGuard, device::efifb::EfiFramebuffer};

const MAX_OUTPUTS: usize = 2;
const LOG_BUFFER_CAPACITY: usize = 128 * 1024;
const LOG_LINE_CAPACITY: usize = 2048;

/// One runtime-installed kernel log sink.
pub type LogWriter = dyn Write + Send;

/// Failure returned when the fixed logger sink table has no free slot left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterError {
    /// Every output slot is already occupied.
    NoFreeSlot,
}

struct LogBuffer {
    bytes: [u8; LOG_BUFFER_CAPACITY],
    head: usize,
    len: usize,
}

impl LogBuffer {
    const fn new() -> Self {
        Self {
            bytes: [0; LOG_BUFFER_CAPACITY],
            head: 0,
            len: 0,
        }
    }

    fn push_bytes(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }

        if bytes.len() >= LOG_BUFFER_CAPACITY {
            let tail = &bytes[bytes.len() - LOG_BUFFER_CAPACITY..];
            self.bytes.copy_from_slice(tail);
            self.head = 0;
            self.len = LOG_BUFFER_CAPACITY;
            return;
        }

        let overflow = self
            .len
            .saturating_add(bytes.len())
            .saturating_sub(LOG_BUFFER_CAPACITY);
        if overflow > 0 {
            self.head = (self.head + overflow) % LOG_BUFFER_CAPACITY;
            self.len -= overflow;
        }

        let tail = (self.head + self.len) % LOG_BUFFER_CAPACITY;
        let first = bytes.len().min(LOG_BUFFER_CAPACITY - tail);
        self.bytes[tail..tail + first].copy_from_slice(&bytes[..first]);
        if first < bytes.len() {
            self.bytes[..bytes.len() - first].copy_from_slice(&bytes[first..]);
        }
        self.len += bytes.len();
    }

    fn replay_into(&self, writer: &mut LogWriter) {
        let first = self.len.min(LOG_BUFFER_CAPACITY - self.head);
        self.write_slice(writer, &self.bytes[self.head..self.head + first]);
        if first < self.len {
            self.write_slice(writer, &self.bytes[..self.len - first]);
        }
    }

    fn write_slice(&self, writer: &mut LogWriter, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let text = unsafe { core::str::from_utf8_unchecked(bytes) };
        let _ = writer.write_str(text);
    }
}

struct LineBuffer {
    bytes: [u8; LOG_LINE_CAPACITY],
    len: usize,
}

impl LineBuffer {
    fn new() -> Self {
        Self {
            bytes: [0; LOG_LINE_CAPACITY],
            len: 0,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl Write for LineBuffer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.len >= self.bytes.len() {
            return Ok(());
        }
        let available = self.bytes.len() - self.len;
        let copy_len = s.len().min(available);
        self.bytes[self.len..self.len + copy_len].copy_from_slice(&s.as_bytes()[..copy_len]);
        self.len += copy_len;
        Ok(())
    }
}

struct LoggerState {
    buffer: LogBuffer,
    outputs: [Option<&'static mut LogWriter>; MAX_OUTPUTS],
}

impl LoggerState {
    const fn new() -> Self {
        Self {
            buffer: LogBuffer::new(),
            outputs: [None, None],
        }
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        self.buffer.push_bytes(bytes);
        for slot in &mut self.outputs {
            if let Some(writer) = slot.as_mut() {
                let text = unsafe { core::str::from_utf8_unchecked(bytes) };
                let _ = (*writer).write_str(text);
            }
        }
    }

    fn register_output(&mut self, writer: &'static mut LogWriter) -> Result<usize, RegisterError> {
        for (slot_id, slot) in self.outputs.iter_mut().enumerate() {
            if slot.is_none() {
                self.buffer.replay_into(writer);
                *slot = Some(writer);
                return Ok(slot_id);
            }
        }
        Err(RegisterError::NoFreeSlot)
    }

    fn unregister_output(&mut self, slot_id: usize) -> bool {
        if slot_id >= self.outputs.len() {
            return false;
        }
        self.outputs[slot_id].take().is_some()
    }
}

/// Global kernel logger with a bounded replay buffer and a small sink table.
pub struct KernelLogger {
    state: SpinLock<LoggerState, IrqSaveGuard>,
}

static PANIC_MODE: AtomicBool = AtomicBool::new(false);

impl KernelLogger {
    /// Create one empty logger instance.
    pub const fn new() -> Self {
        Self {
            state: SpinLock::new(LoggerState::new()),
        }
    }

    fn format_record(&self, record: &Record<'_>) -> LineBuffer {
        let mut line = LineBuffer::new();
        let _ = write!(&mut line, "[{}] - {}\n", record.level(), record.args());
        line
    }

    fn write_record(&self, record: &Record<'_>) {
        let line = self.format_record(record);
        self.state.lock().write_bytes(line.as_bytes());
    }

    /// Register one output sink and replay buffered log lines into it.
    pub fn register_output(&self, writer: &'static mut LogWriter) -> Result<usize, RegisterError> {
        self.state.lock().register_output(writer)
    }

    /// Unregister one previously installed sink by slot id.
    pub fn unregister_output(&self, slot_id: usize) -> bool {
        self.state.lock().unregister_output(slot_id)
    }

    /// Force-unlock the logger state after panic-mode takeover.
    ///
    /// # Safety
    ///
    /// The caller must ensure no other CPU still relies on the current lock
    /// owner; this is only intended for panic recovery.
    pub unsafe fn force_unlock(&self) {
        unsafe { self.state.force_unlock() };
    }
}

struct LoggerBackend;

impl log::Log for LoggerBackend {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &Record<'_>) {
        if let Some(false) = record.module_path().map(|p| p.contains("akarin")) {
            if record.level() == LevelFilter::Debug {
                return;
            }
        }
        LOGGER.write_record(record);
    }

    fn flush(&self) {
        let _ = &LOGGER;
    }
}

static LOGGER: KernelLogger = KernelLogger::new();
static BACKEND: LoggerBackend = LoggerBackend;

/// Install the global kernel logger backend and set the runtime log level.
pub fn install(max_level: LevelFilter) -> Result<(), SetLoggerError> {
    log::set_logger(&BACKEND)?;
    log::set_max_level(max_level);
    Ok(())
}

/// Return the process-wide global kernel logger.
pub fn global() -> &'static KernelLogger {
    &LOGGER
}

/// Register one additional output sink on the global logger.
pub fn register_output(writer: &'static mut LogWriter) -> Result<usize, RegisterError> {
    global().register_output(writer)
}

/// Remove one output sink from the global logger.
pub fn unregister_output(slot_id: usize) -> bool {
    global().unregister_output(slot_id)
}

/// Switch the logger into panic mode and unlock any abandoned state.
pub fn enter_panic_mode() {
    if PANIC_MODE.swap(true, Ordering::Acquire) {
        return;
    }
    unsafe { global().force_unlock() };
}

/// Force-unlock the global logger directly.
///
/// # Safety
///
/// The caller must guarantee the current lock owner can no longer make
/// progress. This is reserved for fatal-panic recovery paths.
pub unsafe fn force_unlock() {
    unsafe { global().force_unlock() };
}

struct EfiFramebufferWriter {
    handle: Option<Handle>,
}

impl EfiFramebufferWriter {
    const fn new() -> Self {
        Self { handle: None }
    }

    fn attach(&mut self, handle: Handle) -> Result<(), ObjectError> {
        if self.handle.is_some() {
            return Err(ObjectError::InvalidArgument);
        }
        self.handle = Some(handle);
        Ok(())
    }
}

impl Write for EfiFramebufferWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if let Some(handle) = self.handle.as_ref() {
            match handle.write_cp_with::<EfiFramebuffer, _, _>(|fb| fb.write_str(s)) {
                Ok(Ok(())) => {}
                _ => return Err(fmt::Error),
            }
        }
        Ok(())
    }
}

unsafe impl Send for EfiFramebufferWriter {}

static mut EFI_FRAMEBUFFER_WRITER: EfiFramebufferWriter = EfiFramebufferWriter::new();

/// Attach the EFI framebuffer device as one additional kernel log sink.
pub fn attach_efi_framebuffer_output(handle: Handle) -> Result<usize, ObjectError> {
    let device_handle = handle.derive_handle(Capability::WRITE, 0)?;
    let writer = unsafe { &mut *(&raw mut EFI_FRAMEBUFFER_WRITER) };
    writer.attach(handle)?;

    let slot = match register_output(writer) {
        Ok(slot) => slot,
        Err(RegisterError::NoFreeSlot) => return Err(ObjectError::InvalidArgument),
    };

    if let Err(err) =
        device_handle.write_cp_with::<EfiFramebuffer, _, _>(|fb| fb.attach_logger_output(slot))?
    {
        let _ = unregister_output(slot);
        return Err(err);
    }

    Ok(slot)
}
