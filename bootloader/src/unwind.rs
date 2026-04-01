use core::arch::asm;

use lazy_static::lazy_static;
use log::trace;
use uefi::proto::loaded_image::LoadedImage;

lazy_static! {
    static ref IMAGE_BASE: usize = {
        let loaded_image_handle = uefi::boot::get_handle_for_protocol::<LoadedImage>()
            .expect("Failed to get LoadedImage handle");
        let loaded_image = uefi::boot::open_protocol_exclusive::<LoadedImage>(loaded_image_handle)
            .expect("Failed to open LoadedImage protocol");
        loaded_image.info().0 as usize
    };
}

pub struct X86StackTrace {
    rbp: *const usize,
}

impl X86StackTrace {
    fn new() -> Self {
        let rbp: *const usize;
        unsafe {
            asm!("mov {}, rbp", out(reg) rbp);
        }
        Self { rbp }
    }

    fn next(&mut self) -> Option<usize> {
        if self.rbp.is_null() {
            return None;
        }
        let ra = unsafe { *self.rbp.offset(1) };
        self.rbp = unsafe { *self.rbp as *const usize };
        if ra != 0 { Some(ra - 1) } else { None }
    }
}

pub fn stack_trace(depth: usize) {
    let mut tracer = X86StackTrace::new();
    let mut counter = 0;
    trace!("Image Base: {:#x}", *IMAGE_BASE);
    trace!("STACK TRACE: ");
    while let Some(ra) = tracer.next() {
        counter += 1;
        trace!("{:4}:<{:#x}>", counter, ra);
        if counter >= depth {
            break;
        }
    }
}

pub fn image_base() -> usize {
    *IMAGE_BASE
}
