use core::slice;

use libakarin_boot_proto::KernelSymtab;
use libakarin_dyld::Nlist as KernelSymbol;
use libakarin_machine_core::sync::{NoOp, ScopedGuard};
use libakarin_sync::spin::Once;
use rustc_demangle::{Demangle, demangle};

use crate::RuntimeServices;

static SYMTAB: Once<Option<Symtab>, ScopedGuard<NoOp>> = Once::new();

/// Kernel symbol table view backed by the bootloader-provided Mach-O symtab and
/// string table slices.
pub struct Symtab {
    symtab: &'static [KernelSymbol],
    strtab: &'static [u8],
    slide: usize,
}

impl Symtab {
    /// Create a symbol table view from the boot protocol symbol metadata.
    pub fn new(slide: usize, image_symtab: &KernelSymtab) -> Self {
        let symtab = unsafe {
            slice::from_raw_parts(
                image_symtab.sym_addr as *const KernelSymbol,
                image_symtab.num_syms,
            )
        };
        let strtab = unsafe {
            slice::from_raw_parts(image_symtab.str_addr as *const u8, image_symtab.str_size)
        };
        Self {
            symtab,
            strtab,
            slide,
        }
    }

    /// Return the global kernel symbol table, lazily initialized from BootInfo.
    pub fn global() -> Option<&'static Self> {
        SYMTAB
            .get_or_else(|| {
                RuntimeServices::boot_info()
                    .image_info
                    .as_ref()
                    .and_then(|image| image.symtab.as_ref().map(|symtab| (symtab, image.slide)))
                    .map(|(symtab, slide)| Self::new(slide, symtab))
            })
            .as_ref()
    }

    /// Resolve an instruction pointer to the closest symbol name at or before
    /// that address.
    pub fn resolve(&self, mut ip: usize) -> Option<(Demangle<'static>, usize)> {
        ip = ip.checked_sub(self.slide)?;
        let mut idx = self
            .symtab
            .partition_point(|sym| (sym.n_value as usize) <= ip);
        while idx > 0 {
            idx -= 1;
            let symbol = &self.symtab[idx];
            if !Self::is_resolvable_symbol(symbol) {
                continue;
            }
            let name = self.symbol_name(symbol)?;
            return Some((demangle(name), ip - symbol.n_value as usize));
        }
        None
    }

    fn is_resolvable_symbol(symbol: &KernelSymbol) -> bool {
        const N_STAB: u8 = 0xe0;
        const N_TYPE: u8 = 0x0e;
        const N_SECT: u8 = 0x0e;

        symbol.n_value != 0 && (symbol.n_type & N_STAB) == 0 && (symbol.n_type & N_TYPE) == N_SECT
    }

    fn symbol_name(&self, symbol: &KernelSymbol) -> Option<&'static str> {
        let offset = symbol.n_strx as usize;
        if offset == 0 || offset >= self.strtab.len() {
            return None;
        }
        let bytes = &self.strtab[offset..];
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        let name = core::str::from_utf8(&bytes[..end]).ok()?;
        if name.is_empty() { None } else { Some(name) }
    }
}
