use core::convert::TryFrom;

use libakarin_machine_core::interrupt::IrqError;
use libakarin_object::{Handle, ObjectError};
use libakarin_syscall::{IrqAckDisposition, IrqWaitFlags, SyscallArgs, SyscallResult};

use super::SyscallEnvironment;
use crate::interrupt::{IrqSessionObject, IrqSyscallCodec};

fn session_handle(slot: u32) -> Result<Handle, ObjectError> {
    SyscallEnvironment::current_process_ref()?.acquire_handle(slot)
}

fn invalid_argument() -> SyscallResult {
    IrqSyscallCodec::underlying_error(IrqError::InvalidParameter)
}

pub async fn wait(args: SyscallArgs) -> SyscallResult {
    let slot = args.args()[0] as u32;
    let deadline = args.args()[1];
    let Some(flags) = IrqWaitFlags::from_bits(args.args()[2]) else {
        return invalid_argument();
    };

    let handle = match session_handle(slot) {
        Ok(handle) => handle,
        Err(err) => return IrqSyscallCodec::object_error(err),
    };

    let future = match handle
        .execute_cp_with::<IrqSessionObject, _, _>(|guard| guard.wait_future(deadline, flags))
    {
        Ok(Ok(future)) => future,
        Ok(Err(err)) | Err(err) => return IrqSyscallCodec::object_error(err),
    };

    match future.await {
        Ok(snapshot) => IrqSyscallCodec::ok_wait(snapshot),
        Err(err) => IrqSyscallCodec::underlying_error(err),
    }
}

pub async fn ack(args: SyscallArgs) -> SyscallResult {
    let slot = args.args()[0] as u32;
    let epoch = args.args()[1] as u64;
    let Ok(disposition) = IrqAckDisposition::try_from(args.args()[2]) else {
        return invalid_argument();
    };

    let handle = match session_handle(slot) {
        Ok(handle) => handle,
        Err(err) => return IrqSyscallCodec::object_error(err),
    };

    match handle
        .write_cp_with::<IrqSessionObject, _, _>(|guard| guard.ack_epoch(epoch, disposition))
    {
        Ok(Ok(Ok(pending_count))) => IrqSyscallCodec::ok_ack(pending_count, epoch),
        Ok(Ok(Err(err))) => IrqSyscallCodec::underlying_error(err),
        Ok(Err(err)) | Err(err) => IrqSyscallCodec::object_error(err),
    }
}
