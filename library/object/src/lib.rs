//! Object management library for Akarin Kernel.
//!
//! This library provides a hierarchical object model with capabilities for
//! access control, and a registry for managing objects and their paths. It is
//! designed to be used in a `no_std` environment, making it suitable for kernel
//! development and embedded systems.
#![no_std]

use libakarin_sync::collections::skiplist::{SkipMap, map::Iter};

#[macro_use]
extern crate alloc;

type Map<K, V> = SkipMap<K, V>;
type MapIter<'a, K, V> = Iter<'a, K, V>;

mod namespace;
mod object;
mod registry;

pub use libakarin_syscall::{CpAccessMode, ObjectLifecycleFlags, SyscallContext, SyscallDispatch};
pub use namespace::ResourceManager;
pub use object::{
    AdminOperation, Capability, ControlPlane, Handle, NameSpace, Object, ObjectContainer,
    ObjectError, ObjectStatus, ObjectSyscallContext, Payload, PayloadRef, Permit, ReadOperation,
    WriteOperation,
};
pub use registry::{ObjectPath, Registry};
