use core::arch::asm;

use hashbrown::HashMap;
use libakarin_machine_core::{
    cpu::{PerCpuRuntime, PerCpuTrait},
    cpu_local,
    memory::FrameZone,
    sync::{NoOp, ScopedGuard},
};
use libakarin_sync::spin::{Lazy, Once};
use raw_cpuid::CpuId;

use crate::{RuntimeServices, arch::guards::IrqSaveGuard, device::acpi::early_cpu_topology};

static PERCPU_AREA_BASE: Once<usize, ScopedGuard<NoOp>> = Once::new();
static PERCPU_AREA_NUM: Once<usize, ScopedGuard<NoOp>> = Once::new();
static PERCPU_AREA_SIZE: Once<usize, ScopedGuard<NoOp>> = Once::new();
const IRQ_STACK_PAGES: usize = 4;

// Keep crate visibility here so cross-module inlining on Mach-O does not
// internalize the symbol and leave external references dangling at link time.
#[allow(private_interfaces)]
static CPU_TOPOLOGY: Lazy<CpuTopology, ScopedGuard<NoOp>> = Lazy::new(|| CpuTopology::discover());

cpu_local! {
    static LOCAL_APIC_ID: usize = usize::MAX;
}

cpu_local! {
    static CPU_ID: usize = usize::MAX;
}

cpu_local! {
    static IRQ_STACK_TOP: Lazy<usize, ScopedGuard<NoOp>> = Lazy::new(|| {
        RuntimeServices::global()
            .frame_allocator()
            .alloc(None, FrameZone::default(), IRQ_STACK_PAGES)
            .expect("failed to allocate per-CPU irq stack")
            .as_usize()
            + IRQ_STACK_PAGES * 4096
    });
}

cpu_local! {
    static IRQ_NESTING: usize = 0;
}

#[unsafe(no_mangle)]
static IRQ_STACK_TOP_OFFSET: Lazy<usize, ScopedGuard<NoOp>> = Lazy::new(|| IRQ_STACK_TOP.offset());

#[unsafe(no_mangle)]
static IRQ_NESTING_OFFSET: Lazy<usize, ScopedGuard<NoOp>> = Lazy::new(|| IRQ_NESTING.offset());

struct CpuTopology {
    lapic_to_cpuid: HashMap<usize, usize>,
    cpu_count: usize,
}

impl CpuTopology {
    fn discover() -> Self {
        let boot = RuntimeServices::boot_info();
        let topology = early_cpu_topology(boot.rsdp, boot.physical_memory_offset)
            .expect("ACPI MADT not available during CPU topology discovery");
        let mut lapic_to_cpuid = HashMap::new();
        let cpuid = CpuId::new();
        let bsp_apic_id = if let Some(mut ext_info) = cpuid.get_extended_topology_info() {
            ext_info
                .next()
                .map(|t| t.x2apic_id() as usize)
                .unwrap_or_else(|| {
                    cpuid
                        .get_feature_info()
                        .map(|f| f.initial_local_apic_id() as usize)
                        .expect("failed to get local APIC ID from CPUID")
                })
        } else {
            CpuId::new()
                .get_feature_info()
                .map(|f| f.initial_local_apic_id() as usize)
                .expect("failed to get current LAPIC ID from CPUID")
        };

        let mut idx = 1;
        lapic_to_cpuid.insert(bsp_apic_id as usize, 0); // BSP is always CPU 0
        topology.lapic_ids().iter().for_each(|&id| {
            if id != bsp_apic_id as _ {
                lapic_to_cpuid.insert(id as usize, idx);
                idx += 1;
            }
        });
        let cpu_count = topology.cpu_count();

        Self {
            lapic_to_cpuid,
            cpu_count,
        }
    }
}

pub struct PerCpu;

impl PerCpu {
    pub fn install_runtime() {
        PerCpuRuntime::setup::<ScopedGuard<IrqSaveGuard>, Self>();
    }

    pub fn set_current_cpu_base() {
        let cpuid = CpuId::new();
        let lapic_id = if let Some(mut ext_info) = cpuid.get_extended_topology_info() {
            ext_info
                .next()
                .map(|t| t.x2apic_id() as usize)
                .unwrap_or_else(|| {
                    cpuid
                        .get_feature_info()
                        .map(|f| f.initial_local_apic_id() as usize)
                        .expect("failed to get local APIC ID from CPUID")
                })
        } else {
            CpuId::new()
                .get_feature_info()
                .map(|f| f.initial_local_apic_id() as usize)
                .expect("failed to get current LAPIC ID from CPUID")
        };
        let cpu_id = CPU_TOPOLOGY
            .lapic_to_cpuid
            .get(&lapic_id)
            .copied()
            .expect("failed to resolve current CPU ID from LAPIC ID");
        let base = Self::area_base(cpu_id);

        unsafe {
            Self::reg_write(base);
        }
    }

    pub fn lapic_id_of(cpu_id: usize) -> Option<usize> {
        unsafe { LOCAL_APIC_ID.remote_ref_raw(cpu_id).copied() }
    }

    pub fn irq_stack_top() -> usize {
        let _ = *IRQ_STACK_TOP_OFFSET;
        IRQ_STACK_TOP.with_current(|top| **top)
    }

    pub fn irq_stack_pages() -> usize {
        IRQ_STACK_PAGES
    }

    pub fn irq_nesting() -> usize {
        let _ = *IRQ_NESTING_OFFSET;
        IRQ_NESTING.with_current(|nesting| *nesting)
    }

    pub fn enter_irq_nesting() -> usize {
        let _ = *IRQ_NESTING_OFFSET;
        IRQ_NESTING.with_current(|nesting| {
            let current = *nesting;
            *nesting = current.saturating_add(1);
            current
        })
    }

    pub fn leave_irq_nesting() {
        let _ = *IRQ_NESTING_OFFSET;
        IRQ_NESTING.with_current(|nesting| {
            *nesting = nesting.saturating_sub(1);
        });
    }
}

impl PerCpuTrait for PerCpu {
    fn id() -> usize {
        if PERCPU_AREA_SIZE.is_initialized() && Self::reg_read() != 0 {
            CPU_ID.with_current(|inner| *inner)
        } else {
            0
        }
    }

    fn count() -> usize {
        CPU_TOPOLOGY.cpu_count
    }

    unsafe fn init() -> usize {
        let template = Self::template();
        let area_num = CPU_TOPOLOGY.cpu_count;
        let area_size = template.len();
        let total_size = (area_num * area_size).next_multiple_of(4096);
        let area_base = RuntimeServices::global()
            .frame_allocator()
            .alloc(None, FrameZone::default(), total_size / 4096)
            .expect("failed to allocate per-CPU area");
        let dst = unsafe { core::slice::from_raw_parts_mut(area_base.as_mut_ptr(), total_size) };

        for i in 0..area_num {
            let offset = i * area_size;
            dst[offset..offset + area_size].copy_from_slice(template);

            // Update the LAPIC ID and CPU ID for each per-CPU area.
            let lapic_id = CPU_TOPOLOGY
                .lapic_to_cpuid
                .iter()
                .find_map(|(&lapic_id, &id)| (id == i).then_some(lapic_id))
                .expect("failed to find LAPIC ID for CPU during per-CPU area initialization");
            let cpu_id = i;

            let cpuid_offset = offset + CPU_ID.offset();
            let lapic_id_offset = offset + LOCAL_APIC_ID.offset();
            dst[cpuid_offset..cpuid_offset + core::mem::size_of::<usize>()]
                .copy_from_slice(&cpu_id.to_ne_bytes());
            dst[lapic_id_offset..lapic_id_offset + core::mem::size_of::<usize>()]
                .copy_from_slice(&lapic_id.to_ne_bytes());
        }

        PERCPU_AREA_BASE
            .try_init(area_base.as_mut_ptr() as usize)
            .expect("failed to set per-CPU area base");
        PERCPU_AREA_NUM
            .try_init(area_num)
            .expect("failed to set per-CPU area number");
        PERCPU_AREA_SIZE
            .try_init(area_size)
            .expect("failed to set per-CPU area size");

        total_size
    }

    fn enable_interrupt() {
        unsafe { asm!("sti", options(nomem, nostack, preserves_flags)) }
    }

    fn disable_interrupt() {
        unsafe { asm!("cli", options(nomem, nostack, preserves_flags)) }
    }

    fn is_interrupt_enabled() -> bool {
        let rflags: usize;
        unsafe { asm!("pushf; pop {}", out(reg) rflags, options(nomem, preserves_flags)) }
        rflags & (1 << 9) != 0
    }

    fn halt() {
        unsafe { asm!("hlt", options(nomem, nostack, preserves_flags)) }
    }

    fn idle_until_interrupt() {
        // `sti; hlt` is the x86 idle handoff: interrupts stay masked while we
        // decide whether to sleep, then become visible exactly as the CPU
        // enters the halted state so wakeups cannot be lost in between.
        unsafe {
            asm!("sti; hlt", options(nomem, nostack));
        }
    }

    fn area_num() -> usize {
        *PERCPU_AREA_NUM.get()
    }

    fn area_size() -> usize {
        *PERCPU_AREA_SIZE.get()
    }

    fn area_base(cpu_id: usize) -> usize {
        *PERCPU_AREA_BASE.get() + cpu_id * Self::area_size()
    }

    fn reg_read() -> usize {
        let value: usize;
        unsafe { asm!("rdgsbase {}", out(reg) value, options(nomem, preserves_flags)) }
        value
    }

    unsafe fn reg_write(val: usize) {
        unsafe { asm!("wrgsbase {}", in(reg) val, options(nomem, preserves_flags)) }
    }
}
