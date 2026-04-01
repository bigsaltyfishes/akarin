use core::fmt::Write;

/// Trait for machine initialization routines.
///
/// This trait should be implemented by each platform to provide
/// the necessary initialization steps.
pub trait MachineInitTrait {
    /// Early initialization code that runs before anything else.
    ///
    /// Usually you need to set up serial console here.
    fn early_init();

    /// Return the early boot writer, if one exists.
    ///
    /// This hook is intended for pre-allocator diagnostics such as UART.
    /// Implementations should return the same static writer instance each time.
    fn early_writer() -> Option<&'static mut (dyn Write + Send)> {
        None
    }

    /// Return an upper bound for the number of CPUs that may participate.
    ///
    /// This hook runs after boot information is published but before the full
    /// runtime object graph exists. Implementations must never undercount;
    /// overcounting is acceptable for early GC sizing.
    fn possible_cpu_num() -> usize {
        1
    }

    /// BSP initialization after kernel early runtime is ready.
    ///
    /// This phase should enable required CPU features, initialize per-cpu
    /// memory/TSS, and set up GDT/IDT for the bootstrap processor.
    fn bsp_init();

    /// ISA device initialization after BSP is ready.
    ///
    /// This phase should initialize devices like APIC/ACPI/clock sources.
    fn device_init();

    /// Finalize machine initialization after all devices are ready.
    ///
    /// This phase should complete any remaining machine setup, such as enabling
    /// interrupts or starting secondary CPUs.
    fn finalize_init() {}

    /// SMP initialization on BSP.
    ///
    /// This phase should bring up APs and complete cross-core wiring.
    fn smp_init();

    /// Secondary initialization code that runs on APs.
    ///
    /// You need to set up per-CPU structures here.
    fn ap_init();
}
