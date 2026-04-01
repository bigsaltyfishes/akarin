use alloc::{boxed::Box, vec};
use core::fmt::{self, Write};

use async_trait::async_trait;
use libakarin_boot_proto::display::DisplayMode;
use libakarin_core::{
    bootloader::BootFramebufferData,
    memory::{VmFlags, Vmo},
};
use libakarin_machine_core::memory::{AddressSpaceTrait, VirtAddr};
use libakarin_object::{ControlPlane, Handle, ObjectError, SyscallDispatch};
use libakarin_sync::spin::SpinLock;
use libakarin_syscall::SyscallResult;
use linkme::distributed_slice;
use log::error;

use super::{
    DeviceProbeError,
    manager::{DEVICE_PROBES, DeviceProbe, DeviceProbeContext},
};
use crate::{
    arch::{Machine, guards::IrqSaveGuard, vm::UNIT_PAGE_SIZE},
    logger::unregister_output,
};

const EFI_FRAMEBUFFER_NAME: &str = "EFIFramebuffer";
const CONSOLE_CHAR_HEIGHT: usize = 16;
const CONSOLE_BACKUP_CHAR: char = '?';

struct FramebufferConsole {
    framebuffer: Vmo,
    mode: DisplayMode,
    x_pos: usize,
    y_pos: usize,
    line_spacing: usize,
    letter_spacing: usize,
    border_padding: usize,
}

impl FramebufferConsole {
    fn new(framebuffer: BootFramebufferData) -> Result<Self, ObjectError> {
        let framebuffer_size = framebuffer.buffer.len();
        let framebuffer_phys =
            Machine::virt_to_phys(VirtAddr::new(framebuffer.buffer.as_ptr() as usize))
                .ok_or(ObjectError::InvalidArgument)?;
        let mut console = Self {
            framebuffer: Vmo::new_physical::<Machine>(
                EFI_FRAMEBUFFER_NAME,
                framebuffer_size,
                UNIT_PAGE_SIZE,
                VmFlags::READ | VmFlags::WRITE | VmFlags::MAP,
                framebuffer_phys,
            ),
            mode: framebuffer.mode,
            x_pos: 1,
            y_pos: 1,
            line_spacing: 2,
            letter_spacing: 0,
            border_padding: 1,
        };
        console.clear()?;
        Ok(console)
    }

    fn bytes_per_pixel(&self) -> usize {
        (self.mode.depth / 8) as usize
    }

    fn clear(&mut self) -> Result<(), ObjectError> {
        let framebuffer_size = self.framebuffer.size();
        let zero_fill = vec![0u8; UNIT_PAGE_SIZE];
        let mut offset = 0usize;
        while offset < framebuffer_size {
            let chunk = zero_fill.len().min(framebuffer_size - offset);
            if !self.framebuffer.write(offset, &zero_fill[..chunk]) {
                return Err(ObjectError::InvalidArgument);
            }
            offset += chunk;
        }
        self.x_pos = self.border_padding;
        self.y_pos = self.border_padding;
        Ok(())
    }

    fn new_line(&mut self) {
        self.x_pos = self.border_padding;
        self.y_pos += self.line_spacing + CONSOLE_CHAR_HEIGHT;
    }

    fn move_up(&mut self) -> Result<(), ObjectError> {
        let row_bytes = self.mode.pitch as usize;
        let dy = self.y_pos + self.line_spacing + CONSOLE_CHAR_HEIGHT - self.mode.height as usize;
        let byte_offset = dy * row_bytes;
        let framebuffer_size = self.framebuffer.size();
        let mut row = vec![0u8; row_bytes];
        let mut src = byte_offset;
        let mut dst = 0usize;
        while src < framebuffer_size {
            let chunk = row.len().min(framebuffer_size - src);
            if !self.framebuffer.read(src, &mut row[..chunk]) {
                return Err(ObjectError::InvalidArgument);
            }
            if !self.framebuffer.write(dst, &row[..chunk]) {
                return Err(ObjectError::InvalidArgument);
            }
            src += chunk;
            dst += chunk;
        }

        let zero_fill = vec![0u8; row_bytes.max(UNIT_PAGE_SIZE)];
        let mut clear = framebuffer_size.saturating_sub(byte_offset);
        while clear < framebuffer_size {
            let chunk = zero_fill.len().min(framebuffer_size - clear);
            if !self.framebuffer.write(clear, &zero_fill[..chunk]) {
                return Err(ObjectError::InvalidArgument);
            }
            clear += chunk;
        }
        self.y_pos = self.y_pos.saturating_sub(dy);
        Ok(())
    }

    fn draw_pixel(&mut self, x: usize, y: usize, rgb: [u8; 3]) -> Result<(), ObjectError> {
        if x >= self.mode.width as usize || y >= self.mode.height as usize {
            return Ok(());
        }
        let bytes_per_pixel = self.bytes_per_pixel();
        let offset = y * self.mode.pitch as usize + x * bytes_per_pixel;
        if offset + bytes_per_pixel > self.framebuffer.size() {
            return Ok(());
        }

        let mut pixel = [0u8; 4];
        if bytes_per_pixel >= 3 {
            pixel[self.mode.color_mode.red as usize] = rgb[0];
            pixel[self.mode.color_mode.green as usize] = rgb[1];
            pixel[self.mode.color_mode.blue as usize] = rgb[2];
        }
        if let Some(alpha) = self.mode.color_mode.alpha {
            let alpha = alpha as usize;
            if alpha < bytes_per_pixel {
                pixel[alpha] = 0xff;
            }
        }
        if !self.framebuffer.write(offset, &pixel[..bytes_per_pixel]) {
            return Err(ObjectError::InvalidArgument);
        }
        Ok(())
    }

    fn draw_char(&mut self, ch: char) -> Result<(), ObjectError> {
        if ch == '\n' {
            self.new_line();
            return Ok(());
        }

        let glyph = unifont::get_glyph(ch).unwrap_or_else(|| {
            unifont::get_glyph(CONSOLE_BACKUP_CHAR).expect("backup glyph missing")
        });

        if self.x_pos + glyph.get_width() >= self.mode.width as usize {
            self.new_line();
        }
        if self.y_pos + CONSOLE_CHAR_HEIGHT + self.line_spacing > self.mode.height as usize {
            self.move_up()?;
        }

        for y in 0..CONSOLE_CHAR_HEIGHT {
            for x in 0..glyph.get_width() {
                if glyph.get_pixel(x, y) {
                    self.draw_pixel(self.x_pos + x, self.y_pos + y, [0xff, 0xff, 0xff])?;
                }
            }
        }

        self.x_pos += glyph.get_width() + self.letter_spacing;
        Ok(())
    }
}

impl Write for FramebufferConsole {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for ch in s.chars() {
            self.draw_char(ch).map_err(|_| fmt::Error)?;
        }
        Ok(())
    }
}

struct EfiFramebufferState {
    logger_slot: Option<usize>,
    destroyed: bool,
}

/// EFI framebuffer device published under `/Kernel/Device/EFIFramebuffer`.
pub struct EfiFramebuffer {
    state: SpinLock<EfiFramebufferState, IrqSaveGuard>,
    console: SpinLock<FramebufferConsole, IrqSaveGuard>,
}

impl EfiFramebuffer {
    fn new(framebuffer: BootFramebufferData) -> Result<Self, ObjectError> {
        Ok(Self {
            state: SpinLock::new(EfiFramebufferState {
                logger_slot: None,
                destroyed: false,
            }),
            console: SpinLock::new(FramebufferConsole::new(framebuffer)?),
        })
    }

    /// Bind a logger output slot to this device for later cleanup.
    pub fn attach_logger_output(&self, slot: usize) -> Result<(), ObjectError> {
        let state = &mut *self.state.lock();
        if state.destroyed {
            return Err(ObjectError::ObjectDestroyed);
        }
        if state.logger_slot.is_some() {
            return Err(ObjectError::InvalidArgument);
        }
        state.logger_slot = Some(slot);
        Ok(())
    }

    /// Write UTF-8 text to the framebuffer console.
    pub fn write_str(&self, text: &str) -> Result<(), ObjectError> {
        if self.state.lock().destroyed {
            return Err(ObjectError::ObjectDestroyed);
        }

        self.console
            .lock()
            .write_str(text)
            .map_err(|_| ObjectError::InvalidArgument)
    }

    /// Permanently unregister this framebuffer device.
    pub fn unregister(&self) -> Result<(), ObjectError> {
        let logger_slot = {
            let state = &mut *self.state.lock();
            if state.destroyed {
                return Err(ObjectError::ObjectDestroyed);
            }
            state.destroyed = true;
            state.logger_slot.take()
        };

        if let Some(slot) = logger_slot {
            let _ = unregister_output(slot);
        }

        crate::RuntimeServices::global()
            .namespaces()
            .device_manager()
            .unregister_driver(EFI_FRAMEBUFFER_NAME)?;
        Ok(())
    }
}

impl ControlPlane for EfiFramebuffer {
    type ReadGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type WriteGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type AgentGuard<'a>
        = &'a Self
    where
        Self: 'a;
    type AdminGuard<'a>
        = &'a Self
    where
        Self: 'a;

    fn read(&self, _interface_caps: u32) -> Self::ReadGuard<'_> {
        self
    }

    fn write(&self, _interface_caps: u32) -> Self::WriteGuard<'_> {
        self
    }

    fn execute(&self, _interface_caps: u32) -> Self::ExecuteGuard<'_> {
        self
    }

    fn agent(&self, _interface_caps: u32) -> Self::AgentGuard<'_> {
        self
    }

    fn admin(&self, _interface_caps: u32) -> Self::AdminGuard<'_> {
        self
    }
}

#[async_trait]
impl SyscallDispatch<libakarin_object::ObjectSyscallContext> for EfiFramebuffer {
    async fn dispatch(
        &self,
        _caller: &libakarin_object::ObjectSyscallContext,
        method_id: usize,
        _arg1: usize,
        _arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        match method_id {
            // method 0: unregister framebuffer device permanently.
            0 => {
                self.unregister()?;
                Ok([1, 0, 0, 0, 0, 0].into())
            }
            _ => Err(ObjectError::InvalidArgument),
        }
    }
}

fn probe_efi_framebuffer(
    context: &DeviceProbeContext<'_>,
) -> Result<Option<Handle>, DeviceProbeError> {
    let framebuffer = context
        .bootloader_manager()
        .take_framebuffer()
        .map_err(|e| {
            error!("[device/efifb] failed to read boot framebuffer payload: {e:?}");
            DeviceProbeError::InitializationFailed
        })?
        .ok_or(DeviceProbeError::NotPresent)?;

    let device = EfiFramebuffer::new(framebuffer).map_err(|e| {
        error!("[device/efifb] failed to convert framebuffer into VMO-backed device: {e:?}");
        DeviceProbeError::InitializationFailed
    })?;

    let handle = context
        .device_manager()
        .register_driver(EFI_FRAMEBUFFER_NAME, device)
        .map_err(|e| {
            error!("[device/efifb] failed to register EFI framebuffer device: {e:?}");
            DeviceProbeError::InitializationFailed
        })?;

    if let Err(error) = context.bootloader_manager().remove_published("Framebuffer") {
        error!("[device/efifb] failed to remove framebuffer from bootloader manager: {error:?}");
        if let Err(cleanup_error) = context
            .device_manager()
            .unregister_driver(EFI_FRAMEBUFFER_NAME)
        {
            error!(
                "[device/efifb] failed to roll back EFI framebuffer device after bootloader \
                 cleanup failure: {cleanup_error:?}"
            );
        }
        return Err(DeviceProbeError::InitializationFailed);
    }
    Ok(Some(handle))
}

#[distributed_slice(DEVICE_PROBES)]
static EFI_FRAMEBUFFER_DEVICE_PROBE: DeviceProbe = DeviceProbe {
    name: "EFIFramebuffer",
    probe: probe_efi_framebuffer,
};
