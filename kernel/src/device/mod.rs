use thiserror::Error;

pub mod acpi;
pub mod efifb;
pub mod manager;
pub mod pci;

#[derive(Debug, Error)]
pub enum DeviceProbeError {
    /// The probed device is not described by firmware or not usable.
    #[error("Device not present")]
    NotPresent,
    /// The probe failed while initializing the device.
    #[error("Initialization failed")]
    InitializationFailed,
}
