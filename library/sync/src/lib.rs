#![no_std]

#[macro_use]
extern crate alloc;

#[cfg(feature = "host-thread-local-gc")]
extern crate std;

pub mod asynchronous;
pub mod collections;
pub mod gc;
pub mod mailbox;
pub mod spin;
