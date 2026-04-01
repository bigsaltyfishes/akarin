//! Kernel-internal PCI support surfaces used by future user-space PCI drivers.
#![allow(dead_code, unused_imports)]
// This module is staged ahead of the future PCI object layer. Keep the public
// surface visible to later wiring without letting temporary skeletons drown out
// unrelated warnings.

mod cap;
mod config;
mod interrupt;
mod object;

pub use cap::{
    PciBarSummary, PciCapabilitySummary, PciInterruptCapabilities, PciInterruptMode,
    PciInterruptSummary, PciMsiCapability, PciMsixCapability,
};
pub use config::{PciBdf, PciConfigAccess, PciEcamConfig};
pub use interrupt::{PciInterruptAllocation, PciInterruptBinder};
pub use object::{PciFunction, PciFunctionInfo, PciHostBridge, PublishedPciHostBridge};
