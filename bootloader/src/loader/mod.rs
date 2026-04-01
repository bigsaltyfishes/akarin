pub mod loadable;

const MACHO_KRNL_FLAG: u32 = 0x4000_0000;
const KERNEL_SPACE_START: usize = 0xffff_8000_0000_0000;
