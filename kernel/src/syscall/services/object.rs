use alloc::{string::String, vec::Vec};
use core::convert::TryFrom;

use libakarin_object::{CpAccessMode, ObjectStatus, ReadOperation};
use libakarin_syscall::{SYSCALL_STATUS_OK, SyscallArgs, SyscallResult};

use super::{ObjectMetadata, ProcessContext, SyscallEnvironment, SyscallUnderlyingError};
use crate::{
    RuntimeServices,
    syscall::{abi::IntoSyscallResult, user_ptr::UserSlice},
};

pub async fn handle_close(args: SyscallArgs) -> SyscallResult {
    let slot = args.args()[0] as u32;
    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    match process.close_handle(slot) {
        Ok(()) => SyscallResult::new(SYSCALL_STATUS_OK, [0, 0, 0, 0, 0]),
        Err(err) => err.into_syscall_result(),
    }
}

pub async fn handle_clone(args: SyscallArgs) -> SyscallResult {
    let slot = args.args()[0] as u32;
    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    match process.clone_handle(slot) {
        Ok(new_slot) => SyscallResult::new(SYSCALL_STATUS_OK, [new_slot as usize, 0, 0, 0, 0]),
        Err(err) => err.into_syscall_result(),
    }
}

pub async fn handle_derive(args: SyscallArgs) -> SyscallResult {
    let slot = args.args()[0] as u32;
    let Some(capability) = libakarin_object::Capability::from_bits(args.args()[1] as u32) else {
        return SyscallUnderlyingError::InvalidArgument.into_syscall_result();
    };
    let interface_caps = args.args()[2] as u32;
    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    match process.derive_handle(slot, capability, interface_caps) {
        Ok(new_slot) => SyscallResult::new(SYSCALL_STATUS_OK, [new_slot as usize, 0, 0, 0, 0]),
        Err(err) => err.into_syscall_result(),
    }
}

pub async fn locate(args: SyscallArgs) -> SyscallResult {
    let base_slot = args.args()[0] as u32;
    let path_ptr = UserSlice::<u8>::new(args.args()[1], args.args()[2]);
    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };

    let bytes = match path_ptr.copy_to_vec(&process) {
        Ok(bytes) => bytes,
        Err(_) => return SyscallUnderlyingError::Fault.into_syscall_result(),
    };
    let path_string = match String::from_utf8(bytes) {
        Ok(path_string) => path_string,
        Err(_) => return SyscallUnderlyingError::InvalidUtf8.into_syscall_result(),
    };
    let base_handle = if path_string.starts_with('/') {
        None
    } else {
        match process.acquire_handle(base_slot) {
            Ok(handle) => Some(handle),
            Err(err) => return err.into_syscall_result(),
        }
    };

    let located = match base_handle {
        Some(base_handle) => RuntimeServices::global().registry().locate_and_then(
            Some(base_handle),
            &libakarin_object::ObjectPath::new(&path_string),
            |handle| handle,
        ),
        None => RuntimeServices::global()
            .registry()
            .locate_and_then::<libakarin_object::Handle, _, _>(
                None::<libakarin_object::Handle>,
                &libakarin_object::ObjectPath::new(&path_string),
                |handle| handle,
            ),
    };

    let located = match located {
        Ok(handle) => handle,
        Err(err) => return err.into_syscall_result(),
    };

    let slot = process.install_handle_auto(located);
    SyscallResult::new(SYSCALL_STATUS_OK, [slot as usize, 0, 0, 0, 0])
}

pub async fn read_meta(args: SyscallArgs) -> SyscallResult {
    let slot = args.args()[0] as u32;
    let Ok(meta) = ObjectMetadata::try_from(args.args()[1]) else {
        return SyscallUnderlyingError::InvalidArgument.into_syscall_result();
    };
    if matches!(meta, ObjectMetadata::Name) {
        let out = UserSlice::<u8>::new(args.args()[2], args.args()[3]);
        let process = match SyscallEnvironment::current_process_ref() {
            Ok(process) => process,
            Err(err) => return err.into_syscall_result(),
        };
        let bytes = match process.with_handle(slot, |handle: &libakarin_object::Handle| {
            handle.read_with(|object: &dyn ReadOperation| Ok(Vec::from(object.name().as_bytes())))
        }) {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(err)) | Err(err) => return err.into_syscall_result(),
        };
        if out.len() < bytes.len() {
            return SyscallUnderlyingError::BufferTooSmall.into_syscall_result();
        }
        return match out.copy_from_slice(&process, &bytes) {
            Ok(()) => SyscallResult::new(SYSCALL_STATUS_OK, [bytes.len(), 0, 0, 0, 0]),
            Err(_) => SyscallUnderlyingError::Fault.into_syscall_result(),
        };
    }

    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };

    match process.with_handle(slot, |handle: &libakarin_object::Handle| {
        handle.read_with(|object: &dyn ReadOperation| match meta {
            ObjectMetadata::Id => Ok(SyscallResult::new(
                SYSCALL_STATUS_OK,
                [object.id(), 0, 0, 0, 0],
            )),
            ObjectMetadata::Parent => {
                let parent = object.parent().unwrap_or(0);
                Ok(SyscallResult::new(
                    SYSCALL_STATUS_OK,
                    [parent, object.parent().is_some() as usize, 0, 0, 0],
                ))
            }
            ObjectMetadata::MaskedCapabilities => Ok(SyscallResult::new(
                SYSCALL_STATUS_OK,
                [object.masked_caps().bits() as usize, 0, 0, 0, 0],
            )),
            ObjectMetadata::LifecycleFlags => Ok(SyscallResult::new(
                SYSCALL_STATUS_OK,
                [object.lifecycle_flags().bits() as usize, 0, 0, 0, 0],
            )),
            ObjectMetadata::Status => Ok(SyscallResult::new(
                SYSCALL_STATUS_OK,
                [
                    match object.status() {
                        ObjectStatus::Active => 0,
                        ObjectStatus::Destroying => 1,
                    },
                    0,
                    0,
                    0,
                    0,
                ],
            )),
            ObjectMetadata::ChildCount => Ok(SyscallResult::new(
                SYSCALL_STATUS_OK,
                [object.children().count(), 0, 0, 0, 0],
            )),
            ObjectMetadata::Name => unreachable!("name metadata handled above"),
        })
    }) {
        Ok(Ok(frame)) => frame,
        Ok(Err(err)) | Err(err) => err.into_syscall_result(),
    }
}

pub async fn invoke(args: SyscallArgs) -> SyscallResult {
    let slot = args.args()[0] as u32;
    let Ok(mode) = CpAccessMode::try_from(args.args()[1]) else {
        return SyscallUnderlyingError::InvalidArgument.into_syscall_result();
    };
    let method_id = args.args()[2];
    let arg1 = args.args()[3];
    let arg2 = args.args()[4];
    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let caller = ProcessContext::new(process.clone());
    let handle = match process.acquire_handle(slot) {
        Ok(handle) => handle,
        Err(err) => return err.into_syscall_result(),
    };

    match handle.invoke_cp(&caller, mode, method_id, arg1, arg2).await {
        Ok(frame) => frame,
        Err(err) => err.into_syscall_result(),
    }
}
