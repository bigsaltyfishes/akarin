#![feature(naked_functions_rustic_abi)]
#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;

use libakarin_boot_proto as protocol;

mod fs;
mod graphics;
mod loader;
mod logger;
mod memory;
mod menu;
mod misc;
mod resources;
mod session;
mod unwind;

use core::sync::atomic::{AtomicBool, Ordering};

// Removed unused import
use log::{error, info};
use uefi::entry;

use crate::{
    graphics::console::console_instance,
    menu::dialog::{Dialog, DialogType},
    unwind::stack_trace,
};

mod config;

#[entry]
fn efi_main() -> uefi::Status {
    // Your UEFI application code goes here
    #[cfg(feature = "logger_debug")]
    logger::setup();

    info!("Loaded image info: {:#x}", unwind::image_base());

    // Probe resources
    resources::probe_resources();

    // Enable console for menu
    graphics::console::enable_console();
    let console = graphics::console::console_instance();

    // Open Simple File System
    let mut fs = fs::simple::SimpleFileSystem::open().expect("Failed to open Simple File System");

    let mut menu = menu::Menu::new(&mut fs, console);
    menu.run();

    uefi::Status::SUCCESS
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    static PANIKED: AtomicBool = AtomicBool::new(false);
    if PANIKED.swap(true, Ordering::SeqCst) {
        error!("Double panic detected: {:#?}", info);
        loop {
            x86_64::instructions::hlt();
        }
    }
    // Ensure logger is enabled for panic
    #[cfg(not(feature = "logger_debug"))]
    logger::setup();

    log::error!("Panic: {:#?}", info);
    stack_trace(16);
    if let Some(console) = console_instance() {
        let msg = format!("{:#?}", info);
        let dialog = Dialog::new("Panic occurred!", &msg, DialogType::Error);
        dialog.show(console);
    }
    loop {
        x86_64::instructions::hlt();
    }
}
