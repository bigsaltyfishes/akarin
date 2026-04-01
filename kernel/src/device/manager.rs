use alloc::string::ToString;

use libakarin_core::bootloader::BootloaderManager;
use libakarin_object::{
    Capability, ControlPlane, Handle, ObjectError, Payload, ResourceManager, WriteOperation,
};
use libakarin_syscall::ObjectLifecycleFlags;
use linkme::distributed_slice;

use super::DeviceProbeError;

/// A single device probe registered in the distributed probe table.
pub struct DeviceProbe {
    /// Stable probe name used for boot diagnostics.
    pub name: &'static str,
    /// Execute the probe using the supplied bootstrap context.
    pub probe: fn(&DeviceProbeContext<'_>) -> Result<Option<Handle>, DeviceProbeError>,
}

/// Distributed list of device probes executed during runtime bootstrap.
#[distributed_slice]
pub static DEVICE_PROBES: [DeviceProbe];

/// Bootstrap context shared with device probes.
pub struct DeviceProbeContext<'a> {
    bootloader_manager: &'a BootloaderManager,
    device_manager: &'a KernelDeviceManager,
    resource_manager: &'a ResourceManager,
}

impl<'a> DeviceProbeContext<'a> {
    /// Return the bootloader resource manager.
    pub fn bootloader_manager(&self) -> &'a BootloaderManager {
        self.bootloader_manager
    }

    /// Return the device manager running the probe.
    pub fn device_manager(&self) -> &'a KernelDeviceManager {
        self.device_manager
    }

    /// Return the resource manager available to the probe.
    pub fn resource_manager(&self) -> &'a ResourceManager {
        self.resource_manager
    }
}

/// Manager for objects under `/Kernel/Device`.
///
/// Direct plane:
/// - register and unregister drivers
/// - run distributed device probes
///
/// Capability plane:
/// - discover published device objects by relative path
pub struct KernelDeviceManager {
    namespace: Handle,
}

impl KernelDeviceManager {
    /// Create a manager from a namespace handle with `WRITE` capability.
    pub fn new(namespace: Handle) -> Self {
        Self { namespace }
    }

    /// Register a driver object under `/Kernel/Device/{name}`.
    pub fn register_driver<T>(&self, name: &str, payload: T) -> Result<Handle, ObjectError>
    where
        T: ControlPlane + Send + Sync + 'static,
        for<'a> T::ReadGuard<'a>: Send,
        for<'a> T::WriteGuard<'a>: Send,
        for<'a> T::ExecuteGuard<'a>: Send,
    {
        let handle = self.namespace.write_with(|ns: &dyn WriteOperation| {
            ns.add_child(
                name.to_string(),
                Capability::ADMIN | Capability::AGENT,
                Payload::new(payload),
            )
        })??;
        handle.set_lifecycle_flags(ObjectLifecycleFlags::STICKY)?;
        Ok(handle)
    }

    /// Lookup a published device object through the discoverable namespace
    /// plane.
    pub fn lookup_handle(&self, name: &str) -> Result<Handle, ObjectError> {
        self.namespace.locate(name)
    }

    /// Remove a driver object under `/Kernel/Device/{name}`.
    pub fn unregister_driver(&self, name: &str) -> Result<(), ObjectError> {
        self.namespace
            .write_with(|ns: &dyn WriteOperation| ns.remove_child(name, true).map(|_| ()))??;
        Ok(())
    }

    /// Probe and initialize all registered kernel devices.
    pub fn probe_all(
        &self,
        bootloader_manager: &BootloaderManager,
        resource_manager: &ResourceManager,
    ) -> Result<(), DeviceProbeError> {
        let context = DeviceProbeContext {
            bootloader_manager,
            device_manager: self,
            resource_manager,
        };

        for probe in DEVICE_PROBES {
            match (probe.probe)(&context) {
                Ok(Some(handle)) => {
                    log::info!("[device] probe {}: initialized", probe.name);
                    // Probes return the owner handle of the newly created
                    // device object. The device must stay published in the
                    // namespace after probe completes, so the bootstrap path
                    // abandons that handle on success.
                    unsafe { handle.forget() };
                }
                Ok(None) => log::info!("[device] probe {}: skipped", probe.name),
                Err(DeviceProbeError::NotPresent) => {
                    log::info!("[device] probe {}: not present", probe.name);
                }
                Err(err) => {
                    log::warn!("[device] probe {} failed: {:?}", probe.name, err);
                    return Err(err);
                }
            }
        }

        Ok(())
    }
}
