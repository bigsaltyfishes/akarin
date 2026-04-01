use crate::memory::VirtAddr;

unsafe extern "Rust" {
    #[link_name = "\u{1}section$start$__DATA$__extable"]
    static EXCEPTION_TABLE_START: u8;
    #[link_name = "\u{1}section$end$__DATA$__extable"]
    static EXCEPTION_TABLE_END: u8;
}

/// One recoverable kernel exception site exported by one machine backend.
///
/// The current kernel only reserves this interface. Architectures may return
/// an empty table until they wire trap fixups to these entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct ExceptionTableEntry {
    /// Fault instruction pointer
    pub fault_ip_start: VirtAddr,
    /// Fault instruction pointer end (exclusive)
    pub fault_ip_end: VirtAddr,
    /// Recovery continuation instruction pointer.
    pub fixup_ip: VirtAddr,
}

impl ExceptionTableEntry {
    /// Return whether this entry covers the supplied instruction pointer.
    pub fn contains(self, ip: VirtAddr) -> bool {
        self.fault_ip_start < ip && ip < self.fault_ip_end
    }
}

/// Architecture-specific recoverable kernel exception table.
#[derive(Debug)]
pub struct ExceptionTable {
    entries: &'static [ExceptionTableEntry],
}

impl ExceptionTable {
    /// Create a new exception table by reading the linker-generated exception
    /// table section.
    pub fn new() -> Self {
        Self {
            entries: unsafe {
                let start = &EXCEPTION_TABLE_START as *const u8 as usize;
                let end = &EXCEPTION_TABLE_END as *const u8 as usize;
                let len = end.saturating_sub(start) / size_of::<ExceptionTableEntry>();
                core::slice::from_raw_parts(start as *const ExceptionTableEntry, len)
            },
        }
    }

    /// Find the fixup instruction pointer for the supplied fault instruction
    /// pointer, if any.
    pub fn find_fixup(&self, fault_ip: VirtAddr) -> Option<VirtAddr> {
        self.entries
            .iter()
            .find(|entry| entry.contains(fault_ip))
            .map(|entry| entry.fixup_ip)
    }
}
