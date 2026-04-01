use core::fmt::Debug;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortIoError {
    InvalidWidth,
}

pub type PortIoResult<T = ()> = Result<T, PortIoError>;

/// Hardware port-IO operations exported by machine implementations.
pub trait PortIoTrait: Send + Sync + Debug {
    fn read_u8(port: u16) -> PortIoResult<u8>;
    fn read_u16(port: u16) -> PortIoResult<u16>;
    fn read_u32(port: u16) -> PortIoResult<u32>;

    fn write_u8(port: u16, value: u8) -> PortIoResult;
    fn write_u16(port: u16, value: u16) -> PortIoResult;
    fn write_u32(port: u16, value: u32) -> PortIoResult;
}
