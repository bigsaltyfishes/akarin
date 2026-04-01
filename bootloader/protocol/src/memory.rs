#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum ArenaReserveReason {
    X86GlobalDescriptorTable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum ArenaKind {
    /// Usable memory
    Usable,

    /// Memory reserved by the firmware
    Reserved,

    /// ACPI Reclaimable memory
    AcpiReclaimable,

    /// ACPI NVS memory
    AcpiNvs,

    /// Bad memory
    BadMemory,

    /// Arena BootInfo stored
    BootloaderProvideInfo,

    /// Bootloader Reserved
    BootloaderReserved(ArenaReserveReason),

    /// Kernel and its modules
    ExecutableAndModules,

    /// Kernel stack
    KernelStack,

    /// Kernel Page Table
    KernelPageTable,

    /// Kernel Symbol Table and String Table
    KernelSymbolTable,

    /// Memory reserved for kernel,
    /// this is requested by the kernel via `Requirements`
    KernelReserved,

    /// EFI Framebuffer
    Framebuffer,

    /// End of memory map
    EndOfMemoryMap,

    /// Unknown kind
    Unknown(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arena {
    pub start: usize,
    pub end: usize,
    pub kind: ArenaKind,
}

#[derive(Debug)]
pub struct MemoryMap {
    count: usize,
    arenas: *const Arena,
}

#[derive(Debug)]
pub struct MemoryMapIter<'a> {
    arenas: &'a [Arena],
    index: usize,
}

impl MemoryMap {
    pub fn from_raw(count: usize, arenas: *const Arena) -> Self {
        Self { count, arenas }
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn get(&self, index: usize) -> Option<&Arena> {
        if index >= self.count {
            return None;
        }

        unsafe { Some(&*self.arenas.add(index)) }
    }

    pub fn iter(&self) -> MemoryMapIter<'_> {
        let arenas = unsafe { core::slice::from_raw_parts(self.arenas, self.count) };

        MemoryMapIter { arenas, index: 0 }
    }
}

impl Iterator for MemoryMapIter<'_> {
    type Item = Arena;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(arena) = self.arenas.get(self.index) {
            if arena.kind == ArenaKind::EndOfMemoryMap {
                return None;
            }
        } else {
            return None;
        }

        let arena = self.arenas[self.index];
        self.index += 1;
        Some(arena)
    }
}
