#![no_std]

pub mod display;
pub mod memory;

use core::str;

use memory::MemoryMap;

use crate::{display::DisplayMode, memory::Arena};

pub const INTERFACE_MAGIC: u64 = 0x48494b415249;
pub const INTERFACE_VERSION: u32 = 3;

#[repr(C)]
#[derive(Debug)]
pub struct BootInfo {
    /// The framebuffer and its display mode
    pub framebuffer: Option<(&'static mut [u8], DisplayMode)>,

    /// The memory map provided by the firmware
    pub memory_map: MemoryMap,

    /// The RSDP address if available
    pub rsdp: Option<usize>,

    /// The physical memory offset for higher half addressing
    pub physical_memory_offset: usize,

    /// Kernel Image Information
    pub image_info: Option<KernelImageInfo>,

    /// Bootstrap userspace image bytes loaded by the bootloader.
    pub bootstrap: Option<&'static [u8]>,

    /// Arguments passed to the kernel
    pub args: Option<&'static [&'static str]>,
}

unsafe impl Send for BootInfo {}
unsafe impl Sync for BootInfo {}

#[repr(C)]
#[derive(Debug)]
pub struct KernelImageInfo {
    /// The slide offset of the kernel image in memory
    pub slide: usize,

    /// The symbol table address and size
    pub symtab: Option<KernelSymtab>,

    /// Mapped sections in the kernel image
    ///
    /// By default, bootloader will map all `LC_SEGMENT` marked sections,
    /// kernel may want to know which sections were mapped and where they
    /// are (like `__eh_frame`, `__unwind_info`, etc).
    pub mapped_sections: Option<&'static [(&'static str, Arena)]>,
}

unsafe impl Send for KernelImageInfo {}
unsafe impl Sync for KernelImageInfo {}

#[repr(C)]
#[derive(Debug)]
pub struct KernelSymtab {
    /// The address of the symbol table
    pub sym_addr: usize,

    /// The number of symbols in the table
    pub num_syms: usize,

    /// The string table address
    pub str_addr: usize,

    /// The size of the string table
    pub str_size: usize,
}

/// Requirements for loading the kernel
///
/// # Usage
///
/// ```rust
/// use libakarin_boot_proto::Requirements;
///
/// #[unsafe(link_section = "__REQ,__requests")]
/// static KERNEL_REQUIREMENTS: Requirements = Requirements::new(
///     0xFFFF_FFFF_8000_0000, // Kernel load base
///     0x4,    // Stack size of 16 KiB (in pages)
///     0xFFFF_FF00_0000_0000, // BSP stack base (low address, with 1 guard page)
///     true,   // Symbol table required
///     true,   // Mapped sections required
///     true,   // RSDP required
/// );
/// ```
#[repr(C)]
#[derive(Debug)]
#[allow(dead_code)]
pub struct Requirements {
    /// Magic number to identify the structure
    pub magic: u64,

    /// Version of the requirements structure
    pub version: u32,

    /// Load base address for the kernel
    pub load_base: usize,

    /// Size of the stack to allocate for the kernel in pages
    pub stack_size: usize,

    /// Preferred BSP default-stack base in the kernel stack segment.
    ///
    /// Bootloaders may use a temporary bootstrap stack for the initial kernel
    /// entry and let the kernel switch to its own per-CPU default stack later.
    /// The kernel stack layout reserved at this base still uses one guard page
    /// at the low end, with usable bytes starting at
    /// `bsp_stack_base + 0x1000`.
    pub bsp_stack_base: usize,

    /// Whether the symbol table is required
    pub symtab_required: bool,

    /// Whether mapped sections are required
    pub mapped_sections: bool,

    /// Whether the RSDP is required
    pub rsdp_required: bool,
}

impl Requirements {
    /// Create a new `Requirements` instance
    ///
    /// # Arguments
    ///
    /// - `stack_size`: Size of the stack to allocate for the kernel in pages
    /// - `bsp_stack_base`: preferred BSP default-stack base in the kernel stack
    ///   segment. One guard page is reserved at this base once the kernel
    ///   switches to its own stack layout.
    /// - `symtab_required`: Whether the symbol table is required
    /// - `mapped_sections`: Whether mapped sections are required
    /// - `rsdp_required`: Whether the RSDP is required memory use (1 <= num <=
    ///   4)
    pub const fn new(
        load_base: usize,
        stack_size: usize,
        bsp_stack_base: usize,
        symtab_required: bool,
        mapped_sections: bool,
        rsdp_required: bool,
    ) -> Self {
        Self {
            magic: INTERFACE_MAGIC,
            version: INTERFACE_VERSION,
            load_base,
            stack_size,
            bsp_stack_base,
            symtab_required,
            mapped_sections,
            rsdp_required,
        }
    }
}
