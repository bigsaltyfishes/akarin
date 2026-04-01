use alloc::{boxed::Box, vec};
use core::{
    cell::{Ref, RefCell, RefMut},
    fmt::Debug,
};

use super::canvas::{Color, SimpleCanvas};
use crate::{protocol::display::DisplayMode, resources::framebuffer::Framebuffer};

// TODO: Leak the framebuffer to prevent freeing it
pub struct Screen {
    pub framebuffer: RefCell<Box<[u8]>>,
    pub front_buffer: Option<RefCell<Box<[u8]>>>,
    pub mode: DisplayMode,
}

impl Screen {
    pub fn new(framebuffer: &'static mut Framebuffer) -> Self {
        Self {
            framebuffer: unsafe { RefCell::new(Box::from_raw(framebuffer.buffer())) },
            front_buffer: None,
            mode: *framebuffer.mode(),
        }
    }

    pub fn enable_double_buffering(&mut self) {
        if self.front_buffer.is_some() {
            return;
        }
        let len = self.framebuffer.borrow().len();
        let front_buffer = RefCell::new(vec![0; len].into_boxed_slice());

        // Copy the current framebuffer to the front buffer
        front_buffer
            .borrow_mut()
            .copy_from_slice(&self.framebuffer.borrow());
        self.front_buffer = Some(front_buffer);
    }

    pub fn swap_buffers(&mut self) {
        if let Some(front_buffer) = &self.front_buffer {
            let mut framebuffer = self.framebuffer.borrow_mut();
            let front_buffer = front_buffer.borrow();
            framebuffer.copy_from_slice(&front_buffer);
        }
    }

    pub fn front_buffer(&self) -> Ref<'_, Box<[u8]>> {
        if let Some(front_buffer) = &self.front_buffer {
            front_buffer.borrow()
        } else {
            self.framebuffer.borrow()
        }
    }

    pub fn front_buffer_mut(&mut self) -> RefMut<'_, Box<[u8]>> {
        if let Some(front_buffer) = &self.front_buffer {
            front_buffer.borrow_mut()
        } else {
            self.framebuffer.borrow_mut()
        }
    }

    pub fn get_mode(&self) -> &DisplayMode {
        &self.mode
    }
}

impl SimpleCanvas for Screen {
    fn get_pixel(&self, x: u32, y: u32) -> Color {
        let bytes_per_pixel = self.mode.depth / 8;
        let framebuffer = self.front_buffer();
        if x < self.mode.width && y < self.mode.height {
            let index = ((y * self.mode.width + x) * bytes_per_pixel) as usize;
            let mut color = Color::BLACK;
            for i in 0..bytes_per_pixel as u8 {
                if i == self.mode.color_mode.red {
                    color.red = framebuffer[index + i as usize];
                } else if i == self.mode.color_mode.green {
                    color.green = framebuffer[index + i as usize];
                } else if i == self.mode.color_mode.blue {
                    color.blue = framebuffer[index + i as usize];
                } else if Some(i) == self.mode.color_mode.alpha {
                    color.alpha = framebuffer[index + i as usize];
                }
            }
            color
        } else {
            Color::BLACK
        }
    }

    fn draw_pixel(&mut self, x: u32, y: u32, color: &Color) {
        let mode = self.mode;
        if x >= mode.width || y >= mode.height {
            return;
        }
        let mut framebuffer = self.front_buffer_mut();
        let bytes_per_pixel = mode.depth / 8;
        let offset = (y * mode.pitch + x * bytes_per_pixel) as usize;
        let mut pixel: [u8; 4] = [0; 4];
        if bytes_per_pixel == 1 {
            pixel[0] = (color.red + color.green + color.blue) / 3;
        } else {
            for i in 0..bytes_per_pixel as u8 {
                if i == mode.color_mode.red {
                    pixel[i as usize] = color.red;
                } else if i == mode.color_mode.green {
                    pixel[i as usize] = color.green;
                } else if i == mode.color_mode.blue {
                    pixel[i as usize] = color.blue;
                } else if Some(i) == mode.color_mode.alpha {
                    pixel[i as usize] = color.alpha;
                } else {
                    pixel[i as usize] = 0;
                }
            }
        }
        framebuffer[offset..offset + bytes_per_pixel as usize]
            .copy_from_slice(&pixel.as_slice()[0..bytes_per_pixel as usize]);
    }

    fn move_up(&mut self, dy: u32) {
        let mode = self.mode;
        let mut framebuffer = self.front_buffer_mut();
        let offset = (dy * mode.pitch) as usize;
        let size = framebuffer.len() - offset;
        framebuffer.copy_within(offset.., 0);
        framebuffer[size..].fill(0);
    }

    fn clear(&mut self) {
        let mut framebuffer = self.front_buffer_mut();
        framebuffer.fill(0)
    }

    fn commit(&mut self) {
        self.swap_buffers();
    }
}

impl Debug for Screen {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Screen").field("mode", &self.mode).finish()
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        // Leak the framebuffer to prevent freeing it
        let buffer = self.framebuffer.take();
        let _ = Box::leak(buffer);
    }
}
