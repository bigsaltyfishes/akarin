use core::{
    cmp::max,
    ops::{Deref, DerefMut},
};

use uefi::proto::console::gop::{GraphicsOutput, PixelFormat};

use crate::{
    protocol::display::{ColorMode, DisplayMode},
    resources::UefiResource,
};

static mut FRAMEBUFFER: Option<Framebuffer> = None;

#[derive(Debug)]
pub struct Framebuffer {
    buffer: &'static mut [u8],
    mode: DisplayMode,
}

impl Framebuffer {
    pub fn buffer(&mut self) -> &mut [u8] {
        self.buffer
    }

    pub fn mode(&self) -> &DisplayMode {
        &self.mode
    }
}

impl Deref for Framebuffer {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.buffer
    }
}

impl DerefMut for Framebuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.buffer
    }
}

impl UefiResource for Framebuffer {
    fn probe() -> Option<()> {
        let gop_handler = uefi::boot::get_handle_for_protocol::<GraphicsOutput>().ok()?;
        let mut gop = uefi::boot::open_protocol_exclusive::<GraphicsOutput>(gop_handler).ok()?;

        let fb_slice = unsafe {
            core::slice::from_raw_parts_mut(
                gop.frame_buffer().as_mut_ptr(),
                gop.frame_buffer().size(),
            )
        };

        let info = gop.current_mode_info();
        unsafe {
            FRAMEBUFFER = Some(Self {
                buffer: fb_slice,
                mode: DisplayMode {
                    width: info.resolution().0 as _,
                    height: info.resolution().1 as _,
                    depth: 32,
                    pitch: (max(info.stride(), info.resolution().0) * 4) as _,
                    color_mode: match info.pixel_format() {
                        PixelFormat::Bgr => ColorMode::BGRA,
                        PixelFormat::Rgb => ColorMode::RGBA,
                        _ => return None,
                    },
                },
            })
        }

        Some(())
    }

    fn resource() -> Option<&'static mut Self> {
        unsafe {
            let fb = &raw mut FRAMEBUFFER;
            (*fb).as_mut()
        }
    }
}
