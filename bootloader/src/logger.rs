use core::fmt::Write;

use lazy_static::lazy_static;
use log::{Level, Metadata, Record};
use spin::Mutex;
use uart_16550::SerialPort;

lazy_static! {
    static ref SERIAL_WRITER: Mutex<SerialPort> = {
        let mut serial_port = unsafe { SerialPort::new(0x3F8) };
        serial_port.init();
        Mutex::new(serial_port)
    };
}

struct Logger;

impl log::Log for Logger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        let level_str = match record.level() {
            Level::Trace => "\x1b[33mTRACE\x1b[37m",
            Level::Debug => "\x1b[33mDEBUG\x1b[37m",
            Level::Info => "\x1b[32mINFO\x1b[37m",
            Level::Warn => "\x1b[33mWARN\x1b[37m",
            Level::Error => "\x1b[31mERROR\x1b[37m",
        };

        if let Some(module) = record.module_path() {
            if !module.starts_with("akarin_bootloader") {
                return;
            }
        }

        let serial = &mut SERIAL_WRITER.lock();
        let _ = writeln!(serial, "[{}] {}", level_str, record.args());
    }

    fn flush(&self) {}
}

pub fn setup() {
    log::set_logger(&Logger).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
}
