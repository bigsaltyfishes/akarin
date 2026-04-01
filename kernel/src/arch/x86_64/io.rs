use core::fmt;

use libakarin_machine_core::io::PortIoTrait;

#[derive(Debug)]
pub struct X86PortIo;

impl libakarin_machine_core::io::PortIoTrait for X86PortIo {
    fn read_u8(port: u16) -> libakarin_machine_core::io::PortIoResult<u8> {
        Ok(unsafe { x86::io::inb(port) })
    }

    fn read_u16(port: u16) -> libakarin_machine_core::io::PortIoResult<u16> {
        Ok(unsafe { x86::io::inw(port) })
    }

    fn read_u32(port: u16) -> libakarin_machine_core::io::PortIoResult<u32> {
        Ok(unsafe { x86::io::inl(port) })
    }

    fn write_u8(port: u16, value: u8) -> libakarin_machine_core::io::PortIoResult {
        unsafe { x86::io::outb(port, value) };
        Ok(())
    }

    fn write_u16(port: u16, value: u16) -> libakarin_machine_core::io::PortIoResult {
        unsafe { x86::io::outw(port, value) };
        Ok(())
    }

    fn write_u32(port: u16, value: u32) -> libakarin_machine_core::io::PortIoResult {
        unsafe { x86::io::outl(port, value) };
        Ok(())
    }
}

struct EarlySerialWriter {
    base_port: u16,
}

impl EarlySerialWriter {
    const fn new(base_port: u16) -> Self {
        Self { base_port }
    }

    fn init(&mut self) {
        let base = self.base_port;
        let _ = X86PortIo::write_u8(base + 1, 0x00);
        let _ = X86PortIo::write_u8(base + 3, 0x80);
        let _ = X86PortIo::write_u8(base, 0x03);
        let _ = X86PortIo::write_u8(base + 1, 0x00);
        let _ = X86PortIo::write_u8(base + 3, 0x03);
        let _ = X86PortIo::write_u8(base + 2, 0xC7);
        let _ = X86PortIo::write_u8(base + 4, 0x0B);
    }

    fn write_byte(&mut self, byte: u8) {
        while X86PortIo::read_u8(self.base_port + 5)
            .map(|lsr| lsr & 0x20 == 0)
            .unwrap_or(false)
        {
            core::hint::spin_loop();
        }
        let _ = X86PortIo::write_u8(self.base_port, byte);
    }
}

impl fmt::Write for EarlySerialWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
        Ok(())
    }
}

static mut EARLY_SERIAL_WRITER: EarlySerialWriter = EarlySerialWriter::new(0x3F8);

pub fn init_early_serial() {
    let writer = unsafe { &mut *(&raw mut EARLY_SERIAL_WRITER) };
    writer.init();
}

pub fn early_serial_writer() -> &'static mut (dyn fmt::Write + Send) {
    unsafe { &mut *(&raw mut EARLY_SERIAL_WRITER) }
}
