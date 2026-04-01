use alloc::sync::Arc;

use libakarin_core::memory::{PAGE_SIZE, RegionPurpose, VmControlError, VmFlags, VmRange, Vmo};
use libakarin_machine_core::memory::{VirtAddr, paging::CachePolicy};
use libakarin_object::{Handle, ObjectError};
use libakarin_syscall::{
    SYSCALL_STATUS_OK, SyscallArgs, SyscallResult, VmarMapArgs, VmoChildMode, VmoOpRangeOperation,
    errno::VmError,
};

use super::SyscallEnvironment;
use crate::{UserPtr, UserSlice, sched::process::ProcessVmError, syscall::abi::IntoSyscallResult};

/// Translate one nested VM control-plane result into the fast syscall VM
/// error space without re-encoding subsystem failures as `ObjectError`.
fn map_vm_control_result<T>(
    result: Result<Result<T, VmControlError>, ObjectError>,
) -> Result<T, ProcessVmError> {
    match result {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(ProcessVmError::from(error)),
        Err(error) => Err(ProcessVmError::Object(error)),
    }
}

pub async fn vmo_create(args: SyscallArgs) -> SyscallResult {
    let words = args.to_words();
    let size = words[1];
    let page_size = words[2];
    let Some(flags) = VmFlags::from_bits(words[3] as u32) else {
        return VmError::InvalidArgument.into_syscall_result();
    };
    if size == 0 || page_size != PAGE_SIZE {
        return VmError::InvalidArgument.into_syscall_result();
    }

    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    match process.create_vmo("VMO", size, page_size, flags) {
        Ok(handle) => SyscallResult::new(
            SYSCALL_STATUS_OK,
            [process.install_handle_auto(handle) as usize, 0, 0, 0, 0],
        ),
        Err(err) => err.into_syscall_result(),
    }
}

pub async fn vmo_create_child(args: SyscallArgs) -> SyscallResult {
    let words = args.to_words();
    let slot = words[1] as u32;
    let offset = words[2];
    let size = words[3];
    let Ok(mode) = VmoChildMode::try_from(words[4]) else {
        return VmError::InvalidArgument.into_syscall_result();
    };
    if words[5] != 0 {
        return VmError::InvalidArgument.into_syscall_result();
    }
    if size == 0 {
        return VmError::InvalidRange.into_syscall_result();
    }

    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    match process.create_child_vmo(slot, offset, size, mode) {
        Ok(handle) => SyscallResult::new(
            SYSCALL_STATUS_OK,
            [process.install_handle_auto(handle) as usize, 0, 0, 0, 0],
        ),
        Err(err) => err.into_syscall_result(),
    }
}

pub async fn vmo_get_size(args: SyscallArgs) -> SyscallResult {
    let words = args.to_words();
    let slot = words[1] as u32;
    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    match process.with_handle(slot, |handle: &Handle| {
        handle.read_cp_with::<Vmo, _, _>(|vmo| vmo.size())
    }) {
        Ok(Ok(size)) => SyscallResult::new(SYSCALL_STATUS_OK, [size, 0, 0, 0, 0]),
        Ok(Err(err)) | Err(err) => err.into_syscall_result(),
    }
}

pub async fn vmo_get_stream_size(args: SyscallArgs) -> SyscallResult {
    let words = args.to_words();
    let slot = words[1] as u32;
    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    match process.with_handle(slot, |handle: &Handle| {
        handle.read_cp_with::<Vmo, _, _>(|vmo| vmo.stream_size())
    }) {
        Ok(Ok(size)) => SyscallResult::new(SYSCALL_STATUS_OK, [size, 0, 0, 0, 0]),
        Ok(Err(err)) | Err(err) => err.into_syscall_result(),
    }
}

pub async fn vmo_op_range(args: SyscallArgs) -> SyscallResult {
    let words = args.to_words();
    let slot = words[1] as u32;
    let Ok(operation) = VmoOpRangeOperation::try_from(words[2]) else {
        return VmError::InvalidArgument.into_syscall_result();
    };
    let offset = words[3];
    let len = words[4];
    if words[5] != 0 {
        return VmError::InvalidArgument.into_syscall_result();
    }

    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    match process.vmo_op_range(slot, operation, offset, len) {
        Ok(bytes) => SyscallResult::new(SYSCALL_STATUS_OK, [bytes, 0, 0, 0, 0]),
        Err(err) => err.into_syscall_result(),
    }
}

pub async fn vmo_read(args: SyscallArgs) -> SyscallResult {
    let words = args.to_words();
    let slot = words[1] as u32;
    let offset = words[2];
    let out = UserSlice::<u8>::new(words[3], words[4]);
    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let result: Result<SyscallResult, ProcessVmError> = (|| {
        let mut buffer = alloc::vec![0u8; out.len()];
        map_vm_control_result(process.with_handle(slot, |handle: &Handle| {
            handle.read_cp_with::<Vmo, _, _>(|vmo| vmo.read_vm(offset, &mut buffer))
        }))?;
        out.copy_from_slice(&process, &buffer)
            .map_err(|_| ProcessVmError::Underlying(VmError::Fault))?;
        Ok(SyscallResult::new(
            SYSCALL_STATUS_OK,
            [buffer.len(), 0, 0, 0, 0],
        ))
    })();

    match result {
        Ok(frame) => frame,
        Err(err) => err.into_syscall_result(),
    }
}

pub async fn vmo_write(args: SyscallArgs) -> SyscallResult {
    let words = args.to_words();
    let slot = words[1] as u32;
    let offset = words[2];
    let input = UserSlice::<u8>::new(words[3], words[4]);
    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let result: Result<SyscallResult, ProcessVmError> = (|| {
        let bytes = input
            .copy_to_vec(&process)
            .map_err(|_| ProcessVmError::Underlying(VmError::Fault))?;
        map_vm_control_result(process.with_handle(slot, |handle: &Handle| {
            handle.write_cp_with::<Vmo, _, _>(|vmo| vmo.write_vm(offset, &bytes))
        }))?;
        Ok(SyscallResult::new(
            SYSCALL_STATUS_OK,
            [bytes.len(), 0, 0, 0, 0],
        ))
    })();

    match result {
        Ok(frame) => frame,
        Err(err) => err.into_syscall_result(),
    }
}

pub async fn vmo_set_size(args: SyscallArgs) -> SyscallResult {
    let words = args.to_words();
    let slot = words[1] as u32;
    let new_size = words[2];
    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let result: Result<(usize, usize), ProcessVmError> = (|| {
        map_vm_control_result(process.with_handle(slot, |handle: &Handle| {
            handle.write_cp_with::<Vmo, _, _>(|vmo| vmo.set_size_vm(new_size))
        }))
    })();
    match result {
        Ok((size, stream_size)) => {
            SyscallResult::new(SYSCALL_STATUS_OK, [size, stream_size, 0, 0, 0])
        }
        Err(err) => err.into_syscall_result(),
    }
}

pub async fn vmo_set_stream_size(args: SyscallArgs) -> SyscallResult {
    let words = args.to_words();
    let slot = words[1] as u32;
    let new_stream_size = words[2];
    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let result: Result<usize, ProcessVmError> = (|| {
        map_vm_control_result(process.with_handle(slot, |handle: &Handle| {
            handle.write_cp_with::<Vmo, _, _>(|vmo| vmo.set_stream_size_vm(new_stream_size))
        }))
    })();
    match result {
        Ok(stream_size) => SyscallResult::new(SYSCALL_STATUS_OK, [stream_size, 0, 0, 0, 0]),
        Err(err) => err.into_syscall_result(),
    }
}

pub async fn vmo_set_cache_policy(args: SyscallArgs) -> SyscallResult {
    let words = args.to_words();
    let slot = words[1] as u32;
    let policy = match words[2] {
        0 => CachePolicy::Cached,
        1 => CachePolicy::Uncached,
        2 => CachePolicy::UncachedDevice,
        3 => CachePolicy::WriteCombining,
        _ => return VmError::InvalidArgument.into_syscall_result(),
    };
    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let result: Result<VmFlags, ProcessVmError> = (|| {
        map_vm_control_result(process.with_handle(slot, |handle: &Handle| {
            handle.write_cp_with::<Vmo, _, _>(|vmo| vmo.set_cache_policy_vm(policy))
        }))
    })();
    match result {
        Ok(flags) => SyscallResult::new(SYSCALL_STATUS_OK, [flags.bits() as usize, 0, 0, 0, 0]),
        Err(err) => err.into_syscall_result(),
    }
}

pub async fn vmo_transfer_data(args: SyscallArgs) -> SyscallResult {
    let words = args.to_words();
    let dst_slot = words[1] as u32;
    let dst_offset = words[2];
    let src_slot = words[3] as u32;
    let src_offset = words[4];
    let len = words[5];
    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let result: Result<SyscallResult, ProcessVmError> = (|| {
        let copied = match process.with_handle(dst_slot, |handle: &Handle| {
            handle.admin_cp_with::<Vmo, _, _>(|dst| {
                let shared: Arc<Vmo> =
                    map_vm_control_result(process.with_handle(src_slot, |src_handle: &Handle| {
                        src_handle.read_cp_with::<Vmo, _, _>(|src| src.share_vm())
                    }))?;
                dst.transfer_from_vm(dst_offset, shared.as_ref(), src_offset, len)
                    .map_err(ProcessVmError::from)
            })
        }) {
            Ok(Ok(copied)) => copied,
            Ok(Err(err)) => return Err(err),
            Err(err) => return Err(ProcessVmError::Object(err)),
        };
        Ok(SyscallResult::new(SYSCALL_STATUS_OK, [copied, 0, 0, 0, 0]))
    })();

    match result {
        Ok(frame) => frame,
        Err(err) => err.into_syscall_result(),
    }
}

pub async fn vmar_allocate(args: SyscallArgs) -> SyscallResult {
    let words = args.to_words();
    let slot = words[1] as u32;
    let requested_base = words[2];
    let requested_size = words[3];
    if requested_size == 0 {
        return VmError::InvalidArgument.into_syscall_result();
    }
    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let allocated: Result<(u32, VmRange), ProcessVmError> = (|| {
        let (handle, range) = if requested_base == 0 {
            process.allocate_child_vmar_any(slot, requested_size)?
        } else {
            let end = requested_base
                .checked_add(requested_size)
                .ok_or(ProcessVmError::Underlying(VmError::InvalidArgument))?;
            let range = VmRange::new(VirtAddr::new(requested_base), VirtAddr::new(end))
                .ok_or(ProcessVmError::Underlying(VmError::InvalidArgument))?;
            let handle = process.allocate_child_vmar(slot, range)?;
            (handle, range)
        };
        Ok((process.install_handle_auto(handle), range))
    })();
    match allocated {
        Ok((child_slot, range)) => SyscallResult::new(
            SYSCALL_STATUS_OK,
            [
                child_slot as usize,
                range.start().as_usize(),
                range.len(),
                0,
                0,
            ],
        ),
        Err(err) => err.into_syscall_result(),
    }
}

pub async fn vmar_map(args: SyscallArgs) -> SyscallResult {
    let words = args.to_words();
    let vmar_slot = words[1] as u32;
    let vmo_slot = words[2] as u32;
    let args_ptr = UserPtr::<VmarMapArgs>::new(words[3]);
    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let result: Result<SyscallResult, ProcessVmError> = (|| {
        let args = args_ptr
            .read(&process)
            .map_err(|_| ProcessVmError::Underlying(VmError::Fault))?;
        let Some(flags) = VmFlags::from_bits(args.flags) else {
            return Err(ProcessVmError::Underlying(VmError::InvalidArgument));
        };
        let Ok(purpose) = RegionPurpose::try_from(args.purpose) else {
            return Err(ProcessVmError::Underlying(VmError::InvalidArgument));
        };
        let end = args
            .base
            .checked_add(args.size)
            .ok_or(ProcessVmError::Underlying(VmError::InvalidArgument))?;
        let range = VmRange::new(VirtAddr::new(args.base), VirtAddr::new(end))
            .ok_or(ProcessVmError::Underlying(VmError::InvalidArgument))?;
        process.vmar_map(vmar_slot, vmo_slot, range, args.vmo_offset, flags, purpose)?;
        Ok(SyscallResult::new(
            SYSCALL_STATUS_OK,
            [range.start().as_usize(), range.len(), 0, 0, 0],
        ))
    })();

    match result {
        Ok(frame) => frame,
        Err(err) => err.into_syscall_result(),
    }
}

pub async fn vmar_protect(args: SyscallArgs) -> SyscallResult {
    let words = args.to_words();
    let slot = words[1] as u32;
    let start = words[2];
    let len = words[3];
    let Some(flags) = VmFlags::from_bits(words[4] as u32) else {
        return VmError::InvalidArgument.into_syscall_result();
    };
    let Some(end) = start.checked_add(len) else {
        return VmError::InvalidArgument.into_syscall_result();
    };
    let Some(range) = VmRange::new(VirtAddr::new(start), VirtAddr::new(end)) else {
        return VmError::InvalidArgument.into_syscall_result();
    };

    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    match process.vmar_protect(slot, range, flags) {
        Ok(()) => SyscallResult::new(SYSCALL_STATUS_OK, [0, 0, 0, 0, 0]),
        Err(err) => err.into_syscall_result(),
    }
}

pub async fn vmar_unmap(args: SyscallArgs) -> SyscallResult {
    let words = args.to_words();
    let slot = words[1] as u32;
    let start = VirtAddr::new(words[2]);
    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    match process.vmar_unmap(slot, start) {
        Ok(entry) => SyscallResult::new(SYSCALL_STATUS_OK, [entry.range().len(), 0, 0, 0, 0]),
        Err(err) => err.into_syscall_result(),
    }
}

pub async fn vmar_destroy(args: SyscallArgs) -> SyscallResult {
    let words = args.to_words();
    let slot = words[1] as u32;
    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    match process.destroy_vmar(slot) {
        Ok(()) => SyscallResult::new(SYSCALL_STATUS_OK, [0, 0, 0, 0, 0]),
        Err(err) => err.into_syscall_result(),
    }
}
