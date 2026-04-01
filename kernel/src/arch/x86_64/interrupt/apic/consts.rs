use core::ops::Range;

pub const IOAPIC_BASE: usize = 0x20;
pub const LAPIC_BASE: usize = 0xf0;
pub const IOAPIC_IRQ_RANGE: Range<usize> = IOAPIC_BASE..LAPIC_BASE;

pub const APIC_TIMER_INTERRUPT: usize = LAPIC_BASE + 1;
pub const APIC_ERROR_INTERRUPT: usize = LAPIC_BASE + 2;
pub const APIC_SPURIOUS_INTERRUPT: usize = LAPIC_BASE + 3;
pub const APIC_IPI_RESCHEDULE: usize = LAPIC_BASE + 4;
pub const APIC_IPI_FLUSH_TLB: usize = LAPIC_BASE + 5;
pub const APIC_IPI_PANIC: usize = LAPIC_BASE + 6;
pub const APIC_IPI_MAILBOX: usize = LAPIC_BASE + 7;
pub const APIC_IPI_CUSTOM_BASE: usize = LAPIC_BASE + 8;
