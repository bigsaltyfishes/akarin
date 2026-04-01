//! Kernel debugging support.
//!
//! This module groups the logger, symbol table, and simple backtrace helpers
//! used during early boot, panic reporting, and runtime diagnostics.

use libakarin_machine_core::backtrace::StackFrameTrait;

use crate::{
    arch::StackFrame,
    debug::symtab::Symtab,
};

/// Multi-sink kernel logger implementation and sink registration helpers.
pub mod logger;
/// Symbol-table loading and runtime lookup helpers used by diagnostics.
pub mod symtab;

/// Print one best-effort symbolic backtrace for the current CPU.
pub fn backtrace() {
    let symtab = Symtab::global().expect("backtrace requested but no symbol table available");
    let mut frame_iter = StackFrame::default();
    log::info!("Backtrace:");
    while let Some(ip) = frame_iter.next() {
        if let Some(symbol) = symtab.resolve(ip as _) {
            log::info!("  {:p}: <{:#} + {:#x?}>", ip, symbol.0, symbol.1);
        } else {
            log::info!("  {:p}: <unknown>", ip);
        }
    }
}
