//! x86_64 interrupt-controller implementations.
//!
//! The x86 port currently exposes one APIC-backed controller that owns:
//! - LAPIC timer and IPI vectors;
//! - IOAPIC-routed external IRQ lines;
//! - synthetic message IRQ blocks used for MSI-style delivery.

pub mod apic;
