//! Typed userspace pointer helpers used by syscall handlers.
//!
//! These wrappers validate ranges against the target process address space
//! before issuing machine-specific uaccess reads, writes, or bulk copies.

use alloc::vec::Vec;
use core::{marker::PhantomData, mem::size_of};

use libakarin_core::memory::{VSpaceError, VmFlags, VmRange};
use libakarin_machine_core::memory::{AddressSpaceTrait, UaccessError, VirtAddr};

use crate::{arch::Machine, sched::process::Process};

/// User-pointer validation or access failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserPtrError {
    /// The supplied address range overflowed or is otherwise malformed.
    InvalidRange,
    /// The range is not mapped in the target process.
    NotMapped,
    /// The range exists but lacks the required access permissions.
    PermissionDenied,
    /// The machine-specific uaccess helper faulted while touching the range.
    Fault,
}

impl From<VSpaceError> for UserPtrError {
    fn from(value: VSpaceError) -> Self {
        match value {
            VSpaceError::InvalidRange => Self::InvalidRange,
            VSpaceError::AlreadyMapped => Self::InvalidRange,
            VSpaceError::NotMapped => Self::NotMapped,
            VSpaceError::PermissionDenied => Self::PermissionDenied,
        }
    }
}

impl From<UaccessError> for UserPtrError {
    fn from(value: UaccessError) -> Self {
        match value {
            UaccessError::InvalidRange => Self::InvalidRange,
            UaccessError::NotMapped => Self::NotMapped,
            UaccessError::PermissionDenied => Self::PermissionDenied,
            UaccessError::Fault => Self::Fault,
        }
    }
}

/// One typed pointer originating from user space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserPtr<T> {
    addr: usize,
    _marker: PhantomData<*mut T>,
}

impl<T> UserPtr<T> {
    /// Create one typed user pointer from a raw address.
    pub const fn new(addr: usize) -> Self {
        Self {
            addr,
            _marker: PhantomData,
        }
    }

    /// Return the raw address carried by this typed pointer.
    pub const fn addr(self) -> usize {
        self.addr
    }

    /// Reinterpret this pointer as pointing to another element type.
    pub const fn cast<U>(self) -> UserPtr<U> {
        UserPtr::new(self.addr)
    }

    /// Validate that the pointed-to object is readable in `process`.
    pub fn validate_read(self, process: &Process) -> Result<(), UserPtrError> {
        process
            .validate_user_range(self.byte_range()?, VmFlags::READ)
            .map_err(Into::into)
    }

    /// Validate that the pointed-to object is writable in `process`.
    pub fn validate_write(self, process: &Process) -> Result<(), UserPtrError> {
        process
            .validate_user_range(self.byte_range()?, VmFlags::WRITE)
            .map_err(Into::into)
    }

    /// Read one `Copy` value from userspace after validation succeeds.
    pub fn read(self, process: &Process) -> Result<T, UserPtrError>
    where
        T: Copy,
    {
        self.validate_read(process)?;
        Machine::read_user(VirtAddr::new(self.addr)).map_err(UserPtrError::from)
    }

    /// Write one `Copy` value into userspace after validation succeeds.
    pub fn write(self, process: &Process, value: T) -> Result<(), UserPtrError>
    where
        T: Copy,
    {
        self.validate_write(process)?;
        Machine::write_user(VirtAddr::new(self.addr), value).map_err(UserPtrError::from)
    }

    fn byte_range(self) -> Result<VmRange, UserPtrError> {
        let size = size_of::<T>();
        if size == 0 {
            return Err(UserPtrError::InvalidRange);
        }
        let end = self
            .addr
            .checked_add(size)
            .ok_or(UserPtrError::InvalidRange)?;
        VmRange::new(VirtAddr::new(self.addr), VirtAddr::new(end)).ok_or(UserPtrError::InvalidRange)
    }
}

/// One user-owned typed slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserSlice<T> {
    base: usize,
    len: usize,
    _marker: PhantomData<*mut T>,
}

impl<T> UserSlice<T> {
    /// Create one typed userspace slice view from a base address and length.
    pub const fn new(base: usize, len: usize) -> Self {
        Self {
            base,
            len,
            _marker: PhantomData,
        }
    }

    /// Return the raw base address of the slice.
    pub const fn base(&self) -> usize {
        self.base
    }

    /// Return the element count of the slice.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Return whether the slice contains zero elements.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Validate that the slice range is readable in `process`.
    pub fn validate_read(self, process: &Process) -> Result<(), UserPtrError> {
        if self.is_empty() {
            return Ok(());
        }
        process
            .validate_user_range(self.byte_range()?, VmFlags::READ)
            .map_err(Into::into)
    }

    /// Validate that the slice range is writable in `process`.
    pub fn validate_write(self, process: &Process) -> Result<(), UserPtrError> {
        if self.is_empty() {
            return Ok(());
        }
        process
            .validate_user_range(self.byte_range()?, VmFlags::WRITE)
            .map_err(Into::into)
    }

    /// Copy userspace elements into one caller-provided output slice.
    pub fn copy_into_slice(self, process: &Process, out: &mut [T]) -> Result<(), UserPtrError>
    where
        T: Copy,
    {
        if out.len() > self.len {
            return Err(UserPtrError::InvalidRange);
        }
        if out.is_empty() {
            return Ok(());
        }

        let view = Self::new(self.base, out.len());
        view.validate_read(process)?;
        let bytes = unsafe {
            core::slice::from_raw_parts_mut(
                out.as_mut_ptr().cast::<u8>(),
                size_of::<T>() * out.len(),
            )
        };
        Machine::copy_from_user(VirtAddr::new(self.base), bytes).map_err(UserPtrError::from)?;
        Ok(())
    }

    /// Copy one kernel slice into the userspace slice range.
    pub fn copy_from_slice(self, process: &Process, input: &[T]) -> Result<(), UserPtrError>
    where
        T: Copy,
    {
        if input.len() > self.len {
            return Err(UserPtrError::InvalidRange);
        }
        if input.is_empty() {
            return Ok(());
        }

        let view = Self::new(self.base, input.len());
        view.validate_write(process)?;
        let bytes = unsafe {
            core::slice::from_raw_parts(input.as_ptr().cast::<u8>(), size_of::<T>() * input.len())
        };
        Machine::copy_to_user(VirtAddr::new(self.base), bytes).map_err(UserPtrError::from)?;
        Ok(())
    }

    /// Copy the full userspace slice into one newly allocated `Vec`.
    pub fn copy_to_vec(self, process: &Process) -> Result<Vec<T>, UserPtrError>
    where
        T: Copy,
    {
        let mut out = Vec::with_capacity(self.len);
        if self.is_empty() {
            return Ok(out);
        }
        self.validate_read(process)?;
        unsafe {
            out.set_len(self.len);
        }
        let bytes = unsafe {
            core::slice::from_raw_parts_mut(
                out.as_mut_ptr().cast::<u8>(),
                size_of::<T>() * self.len,
            )
        };
        Machine::copy_from_user(VirtAddr::new(self.base), bytes).map_err(UserPtrError::from)?;
        Ok(out)
    }

    fn byte_range(self) -> Result<VmRange, UserPtrError> {
        let elem_size = size_of::<T>();
        if elem_size == 0 {
            return Err(UserPtrError::InvalidRange);
        }
        let bytes = self
            .len
            .checked_mul(elem_size)
            .ok_or(UserPtrError::InvalidRange)?;
        let end = self
            .base
            .checked_add(bytes)
            .ok_or(UserPtrError::InvalidRange)?;
        VmRange::new(VirtAddr::new(self.base), VirtAddr::new(end)).ok_or(UserPtrError::InvalidRange)
    }
}
