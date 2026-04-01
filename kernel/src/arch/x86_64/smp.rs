use core::sync::atomic::{AtomicBool, Ordering};

use libakarin_core::clock::Clock;
use libakarin_machine_core::{
    cpu::PerCpuTrait,
    init::MachineInitTrait,
    memory::{
        AddressSpaceTrait, FrameZone, PageTableTrait, PhysAddr, VirtAddr,
        paging::{CachePolicy, MMUFlags, PhysFrameTrait, TlbInvalidator},
    },
};
use log::info;

use crate::{
    RuntimeServices,
    arch::{
        Machine, PerCpu,
        vm::{Page, PageSize, PageTable, PhysFrame},
        x86_64::interrupt::apic::Apic,
    },
    sched::stack,
};

const TRAMPOLINE_PAGE: u8 = 8;
const TRAMPOLINE_BASE: usize = (TRAMPOLINE_PAGE as usize) << 12;
const STACK_PTR_OFFSET: usize = 0x0fc0;
const ENTRY_PTR_OFFSET: usize = 0x0fc8;
const BSP_CR3_PTR_OFFSET: usize = 0x0fd0;
const TEMP_CR3_PTR_OFFSET: usize = 0x0fd8;
const TRAMPOLINE_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ap_trampoline.bin"));

static AP_BOOT_FLAG: AtomicBool = AtomicBool::new(false);
static ENTER_KMAIN_FLAG: AtomicBool = AtomicBool::new(false);

pub struct Smp {
    active_page_table: PhysAddr, // Physical CR3 of current BSP page table.
    temp_page_table_virt: VirtAddr,
}

impl Smp {
    pub fn new(ap_entry: fn()) -> Self {
        let frame_allocator = RuntimeServices::global().frame_allocator();
        let active_page_table = Machine::current_base();

        let temp_page_table_virt = frame_allocator
            .alloc(None, FrameZone::Dma32, 1)
            .expect("Failed to allocate temporary page table");
        let temp_page_table_phys = Machine::virt_to_phys(temp_page_table_virt)
            .expect("Failed to get physical address of temporary page table");

        let trampoline_page_virt = frame_allocator
            .alloc(Some(PhysAddr::new(TRAMPOLINE_BASE)), FrameZone::LowMem, 1)
            .expect("Failed to allocate trampoline page");
        let trampoline_page_phys = Machine::virt_to_phys(trampoline_page_virt)
            .expect("Failed to get physical address of trampoline page");

        let dst =
            unsafe { core::slice::from_raw_parts_mut(trampoline_page_virt.as_mut_ptr(), 4096) };
        let src: &[u8] = TRAMPOLINE_BIN;
        dst[..src.len()].copy_from_slice(src);

        // Keep STACK_PTR dynamic and patch it per-AP before sending IPIs.
        dst[ENTRY_PTR_OFFSET..ENTRY_PTR_OFFSET + 8]
            .copy_from_slice(&(ap_entry as usize).to_le_bytes());
        dst[BSP_CR3_PTR_OFFSET..BSP_CR3_PTR_OFFSET + 8]
            .copy_from_slice(&active_page_table.as_usize().to_le_bytes());
        dst[TEMP_CR3_PTR_OFFSET..TEMP_CR3_PTR_OFFSET + 8]
            .copy_from_slice(&temp_page_table_phys.as_usize().to_le_bytes());

        let active_root_virt = Machine::phys_to_virt(active_page_table)
            .expect("Failed to convert active CR3 to virtual address");
        let mut active_page_table_view =
            PageTable::from_raw(frame_allocator, active_root_virt.as_mut_ptr());

        unsafe {
            match active_page_table_view.map(
                Page::new(VirtAddr::new(TRAMPOLINE_BASE), PageSize::Size4K),
                PhysFrame::from_addr(None, trampoline_page_phys, 1),
                MMUFlags::READ | MMUFlags::WRITE | MMUFlags::EXECUTE,
                CachePolicy::Cached,
            ) {
                Ok(inv) => {
                    inv.invalidate();
                    true
                }
                Err(libakarin_machine_core::memory::paging::PagingError::AlreadyMapped) => false,
                Err(err) => panic!("Failed to map trampoline into active page table: {err:?}"),
            }
        };

        // Required by AP bringup flow:
        // map trampoline into active CR3 first, then clone active PML4 into temp CR3.
        let temp_root_ptr = temp_page_table_virt.as_mut_ptr();
        unsafe {
            core::ptr::copy_nonoverlapping(active_root_virt.as_ptr(), temp_root_ptr, 4096);
        }

        log::debug!(
            "[x86_64/smp] staging ready: active_cr3={:#x} temp_cr3={:#x} trampoline={:#x}",
            active_page_table.as_usize(),
            temp_page_table_phys.as_usize(),
            trampoline_page_phys.as_usize(),
        );

        Self {
            active_page_table,
            temp_page_table_virt,
        }
    }

    pub fn allocate_stack(&self, cpu_id: usize) -> VirtAddr {
        let top =
            stack::CpuFallbackStack::install(cpu_id).expect("Failed to allocate AP fallback stack");
        VirtAddr::new(top)
    }

    pub fn smp_bringup(&self) {
        // Get a instance of local APIC
        let lapic = Apic::lapic();
        match RuntimeServices::global()
            .namespaces()
            .clock_source_manager()
            .with_default_clock(|_clock: &Clock| {
                for cpu_id in 1..PerCpu::count() {
                    let apic_id =
                        PerCpu::lapic_id_of(cpu_id).expect("APIC ID not found for CPU ID") as u32;
                    info!("[x86_64/smp cpu={} apic={}] bringup start", cpu_id, apic_id);
                    let stack_top = self.allocate_stack(cpu_id);
                    unsafe {
                        let stack_ptr_paddr = PhysAddr::new(TRAMPOLINE_BASE + STACK_PTR_OFFSET);
                        Machine::write_phys(stack_ptr_paddr, &stack_top.as_usize().to_le_bytes());
                    }
                    lapic.send_init_ipi(apic_id);
                    // cp.wait(Duration::from_millis(200));
                    // lapic.send_init_ipi_deassert();
                    // cp.wait(Duration::from_millis(10));
                    lapic.send_sipi(TRAMPOLINE_PAGE, apic_id);
                    // cp.wait(Duration::from_millis(200));
                    // lapic.send_sipi(TRAMPOLINE_PAGE, apic_id);
                    // cp.wait(Duration::from_millis(200));

                    // Wait for AP handshake with timeout to avoid deadlock.
                    while !AP_BOOT_FLAG.load(Ordering::Acquire) {
                        core::hint::spin_loop();
                    }

                    AP_BOOT_FLAG.store(false, Ordering::Release);
                    info!(
                        "[x86_64/smp cpu={} apic={}] bringup complete",
                        cpu_id, apic_id
                    );
                }
                Ok(())
            }) {
            Ok(()) => {}
            Err(err) => {
                panic!("[x86_64/smp] no default clock source, AP bringup failed: {err:?}");
            }
        }
    }

    pub fn signal_enter_kmain() {
        ENTER_KMAIN_FLAG.store(true, Ordering::Release);
    }

    pub fn ap_signal_booted() {
        AP_BOOT_FLAG.store(true, Ordering::Release);
    }
}

impl Drop for Smp {
    fn drop(&mut self) {
        let frame_allocator = RuntimeServices::global().frame_allocator();

        let active_root_virt = Machine::phys_to_virt(self.active_page_table)
            .expect("Failed to convert active CR3 to virtual address");
        let mut page_table = PageTable::from_raw(frame_allocator, active_root_virt.as_mut_ptr());
        unsafe {
            let (_trampoline_frame, invalidator) = page_table
                .unmap(Page::new(VirtAddr::new(TRAMPOLINE_BASE), PageSize::Size4K))
                .expect("Failed to unmap trampoline page from active page table");
            invalidator.invalidate();
        }

        unsafe {
            frame_allocator.dealloc(self.temp_page_table_virt, 1);
        };
    }
}

fn ap_entry() {
    Machine::ap_init();
    Smp::ap_signal_booted();
    PerCpu::enable_interrupt();
    log::debug!("[x86_64/smp cpu={}] AP entry idle loop", PerCpu::id());

    while !ENTER_KMAIN_FLAG.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    TlbInvalidator::<Machine>::new(true, None, PageSize::Size4K).invalidate();

    crate::Scheduler::enter_current_cpu()
}

pub fn smp_bringup() {
    let smp = Smp::new(ap_entry);
    smp.smp_bringup();
}
