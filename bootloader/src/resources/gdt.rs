use core::ptr::NonNull;

use log::info;
use uefi::boot::{AllocateType, MemoryType, PAGE_SIZE, allocate_pages};
use x86_64::{
    registers::segmentation::{CS, DS, ES, SS, Segment},
    structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector},
};

use crate::{
    protocol::memory::{Arena, ArenaKind, ArenaReserveReason},
    resources::UefiResource,
};

static mut GDT: Option<Gdt> = None;

pub struct Gdt {
    arena: Arena,
    code_selector: SegmentSelector,
    data_selector: SegmentSelector,
}

impl Gdt {
    pub fn as_arena(&self) -> &Arena {
        &self.arena
    }

    /// Safety: This function loads the GDT and modifies segment registers.
    pub unsafe fn load(&'static self) {
        unsafe {
            let gdt_ptr = NonNull::new_unchecked(self.arena.start as *mut GlobalDescriptorTable);
            let gdt = &*gdt_ptr.as_ptr();

            gdt.load();
            CS::set_reg(self.code_selector);
            DS::set_reg(self.data_selector);
            ES::set_reg(self.data_selector);
            SS::set_reg(self.data_selector);
            info!(
                "Loaded GDT with code selector: {:?}, data selector: {:?}",
                self.code_selector, self.data_selector
            );
        }
    }
}

impl UefiResource for Gdt {
    fn probe() -> Option<()> {
        let addr = allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_DATA, 1).ok()?;
        let ptr: NonNull<GlobalDescriptorTable> = addr.cast();
        let mut gdt = GlobalDescriptorTable::new();
        let code_selector = gdt.append(Descriptor::kernel_code_segment());
        let data_selector = gdt.append(Descriptor::kernel_data_segment());

        unsafe {
            ptr.write(gdt);
        };

        unsafe {
            GDT = Some(Gdt {
                arena: Arena {
                    start: addr.as_ptr() as usize,
                    end: addr.as_ptr() as usize + PAGE_SIZE,
                    kind: ArenaKind::BootloaderReserved(
                        ArenaReserveReason::X86GlobalDescriptorTable,
                    ),
                },
                code_selector,
                data_selector,
            });
        }
        Some(())
    }

    fn resource() -> Option<&'static mut Self> {
        unsafe {
            let ptr = &raw mut GDT;
            (*ptr).as_mut()
        }
    }
}
