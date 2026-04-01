//! Global Descriptor Table and TSS setup for x86_64.
//!
//! The descriptor tables here provide three services:
//! - kernel and user code/data selectors for `iretq` / `sysret`;
//! - one per-CPU TSS with dedicated IST stacks for the narrow set of critical
//!   exceptions;
//! - the raw TSS offset exported to `trap.S`, which switches to the per-CPU IRQ
//!   stack before calling the Rust trap handler.

use libakarin_machine_core::{
    cpu_local,
    memory::FrameZone,
    sync::{NoOp, ScopedGuard},
};
use libakarin_sync::spin::Lazy;
use x86_64::{
    VirtAddr,
    instructions::tables::load_tss,
    registers::{
        model_specific::{LStar, Star},
        segmentation::{CS, SS, Segment},
    },
    structures::{
        gdt::{Descriptor, GlobalDescriptorTable},
        tss::TaskStateSegment,
    },
};

use crate::RuntimeServices;

/// IST slot reserved for double-fault recovery.
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;
/// IST slot reserved for non-maskable interrupts.
pub const NMI_IST_INDEX: u16 = 1;
/// IST slot reserved for machine-check exceptions.
pub const MACHINE_CHECK_IST_INDEX: u16 = 2;
/// IST slot reserved for debug and breakpoint exceptions.
pub const DEBUG_IST_INDEX: u16 = 3;
/// Historical page-fault IST slot kept reserved for trap ABI stability.
pub const PAGE_FAULT_IST_INDEX: u16 = 4;

cpu_local! {
    /// Per-CPU TSS containing the emergency IST stacks used by the IDT.
    static TASK_STATE_SEGMENT: Lazy<TaskStateSegment, ScopedGuard<NoOp>> = Lazy::new(|| {
        let frame_allocator = RuntimeServices::global().frame_allocator();
        let mut tss = TaskStateSegment::new();
        let alloc_stack = |num: usize| {
            VirtAddr::new(frame_allocator.alloc(None, FrameZone::default(), num)
                .expect("failed to allocate TSS stack")
                .as_usize() as u64 + 4096)
        };
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = alloc_stack(1);
        tss.interrupt_stack_table[NMI_IST_INDEX as usize] = alloc_stack(1);
        tss.interrupt_stack_table[MACHINE_CHECK_IST_INDEX as usize] = alloc_stack(1);
        tss.interrupt_stack_table[DEBUG_IST_INDEX as usize] = alloc_stack(2);
        tss.interrupt_stack_table[PAGE_FAULT_IST_INDEX as usize] = alloc_stack(1);
        tss
    })
}

unsafe extern "C" {
    unsafe fn __syscall_entry();
}

/// Raw offset of the current CPU's TSS storage inside the `cpu_local!` area.
///
/// `trap.S` reads this symbol directly while switching onto the per-CPU IRQ
/// stack, so the lazy value must be forced before the first trap occurs.
#[unsafe(no_mangle)]
static TASK_STATE_SEGMENT_OFFSET: Lazy<usize, ScopedGuard<NoOp>> =
    Lazy::new(|| TASK_STATE_SEGMENT.offset());

/// Build and load one GDT for the current CPU, then program syscall MSRs and
/// the active TSS selector.
pub fn load() {
    // `trap.S` reads the raw offset value via the exported symbol, so force the
    // lazy value to initialize before the first trap can touch it.
    let _ = *TASK_STATE_SEGMENT_OFFSET;

    unsafe {
        let mut gdt = GlobalDescriptorTable::new();
        let kcode = gdt.append(Descriptor::kernel_code_segment());
        let kdata = gdt.append(Descriptor::kernel_data_segment());
        let ucode = gdt.append(Descriptor::user_code_segment());
        let _udata = gdt.append(Descriptor::user_data_segment());
        let tss_selector = gdt.append(Descriptor::tss_segment(
            TASK_STATE_SEGMENT.current_ref_raw(),
        ));
        let frame: *mut GlobalDescriptorTable = RuntimeServices::global()
            .frame_allocator()
            .alloc(None, FrameZone::default(), 1)
            .expect("failed to allocate frame for GDT")
            .as_mut_ptr() as _;
        frame.write(gdt);
        let gdt = &mut *frame;

        gdt.load();

        CS::set_reg(kcode);
        SS::set_reg(kdata);

        Star::write_raw(ucode.0, kcode.0);
        LStar::write(VirtAddr::new(__syscall_entry as *const () as u64));

        load_tss(tss_selector);
    }
}
