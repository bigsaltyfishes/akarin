use alloc::sync::Arc;

use libakarin_object::{Handle, ObjectError};
use libakarin_sync::asynchronous::RecvError;
use libakarin_syscall::{
    PortCreateKind, PortSendFlags, PortUserMessage, SYSCALL_STATUS_OK, SyscallArgs, SyscallResult,
    errno::IpcError,
};

use super::{ProcessContext, SyscallEnvironment};
use crate::{
    ipc::{
        BroadcastPort, BusPort, PortBindError, PortSendError, PortState, PortUserMessageExt,
        ProcessInboxSender, ReplyPort, UnicastPort,
    },
    sched::process::{Process, ProcessControl},
    syscall::abi::IntoSyscallResult,
};

enum SendOutcome {
    Queued,
    Reply(crate::ipc::Message),
    ReplyHandle(u32),
}

impl From<PortSendError> for IpcError {
    fn from(error: PortSendError) -> Self {
        match error {
            PortSendError::Unbound(_) => Self::Unbound,
            PortSendError::Closed(_) => Self::PortClosed,
            PortSendError::ReceiverClosed(_) => Self::ReceiverClosed,
            PortSendError::CloneFailed { .. } | PortSendError::InvalidMessage { .. } => {
                Self::InvalidMessage
            }
        }
    }
}

struct RecvIpcError(RecvError);

impl From<RecvError> for RecvIpcError {
    fn from(error: RecvError) -> Self {
        Self(error)
    }
}

impl From<RecvIpcError> for IpcError {
    fn from(error: RecvIpcError) -> Self {
        let RecvIpcError(error) = error;
        match error {
            RecvError::Empty => Self::WouldBlock,
            RecvError::Closed => Self::ReceiverClosed,
        }
    }
}

impl From<PortBindError> for IpcError {
    fn from(error: PortBindError) -> Self {
        match error {
            PortBindError::AlreadyBound => Self::AlreadyBound,
            PortBindError::AlreadySubscribed => Self::AlreadySubscribed,
            PortBindError::NotSubscribed => Self::NotSubscribed,
            PortBindError::Closed => Self::PortClosed,
        }
    }
}

pub async fn create(args: SyscallArgs) -> SyscallResult {
    let Ok(kind) = PortCreateKind::try_from(args.args()[0]) else {
        return IpcError::InvalidArgument.into_syscall_result();
    };
    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };

    let created = match kind {
        PortCreateKind::Unicast => process.create_unicast_port(),
        PortCreateKind::Broadcast => process.create_broadcast_port(),
        PortCreateKind::Bus => process.create_bus_port(),
    };

    match created {
        Ok(handle) => SyscallResult::new(
            SYSCALL_STATUS_OK,
            [process.install_handle_auto(handle) as usize, 0, 0, 0, 0],
        ),
        Err(err) => err.into_syscall_result(),
    }
}

pub async fn send(args: SyscallArgs) -> SyscallResult {
    let slot = args.args()[0] as u32;
    let desc_ptr = args.args()[1];
    let Some(flags) = PortSendFlags::from_bits(args.args()[2]) else {
        return IpcError::InvalidArgument.into_syscall_result();
    };
    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let handle = match acquire_handle(&process, slot) {
        Ok(handle) => handle,
        Err(err) => return err.into_syscall_result(),
    };
    let caller = match SyscallEnvironment::current_process_ref() {
        Ok(process) => ProcessContext::new(process),
        Err(err) => return err.into_syscall_result(),
    };
    let desc = match PortUserMessage::read(&caller, desc_ptr) {
        Ok(desc) => desc,
        Err(_err) => return IpcError::Fault.into_syscall_result(),
    };
    let (message, transfer) = match desc.into_message(&caller) {
        Ok(message) => message,
        Err(error) => return error.into_syscall_result(),
    };

    match send_with_handle(&caller, &handle, message, flags).await {
        Ok(SendOutcome::Queued) => match transfer.commit(&caller) {
            Ok(()) => SyscallResult::new(SYSCALL_STATUS_OK, [0, 0, 0, 0, 0]),
            Err(error) => error.into_syscall_result(),
        },
        Ok(SendOutcome::Reply(mut reply)) => {
            match desc.write_message(&caller, &mut reply, desc_ptr) {
                Ok(()) => match transfer.commit(&caller) {
                    Ok(()) => SyscallResult::new(SYSCALL_STATUS_OK, [0, 0, 0, 0, 0]),
                    Err(error) => error.into_syscall_result(),
                },
                Err(error) => error.into_syscall_result(),
            }
        }
        Ok(SendOutcome::ReplyHandle(slot)) => match transfer.commit(&caller) {
            Ok(()) => SyscallResult::new(SYSCALL_STATUS_OK, [slot as usize, 0, 0, 0, 0]),
            Err(error) => error.into_syscall_result(),
        },
        Err(err) => err.into_syscall_result(),
    }
}

pub async fn recv(args: SyscallArgs) -> SyscallResult {
    let slot = args.args()[0] as u32;
    let desc_ptr = args.args()[1];
    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let handle = match acquire_handle(&process, slot) {
        Ok(handle) => handle,
        Err(err) => return err.into_syscall_result(),
    };
    let caller = match SyscallEnvironment::current_process_ref() {
        Ok(process) => ProcessContext::new(process),
        Err(err) => return err.into_syscall_result(),
    };

    let mut message = match recv_with_handle(&caller, &handle).await {
        Ok(message) => message,
        Err(err) => return err.into_syscall_result(),
    };
    let desc = match PortUserMessage::read(&caller, desc_ptr) {
        Ok(desc) => desc,
        Err(_err) => return IpcError::Fault.into_syscall_result(),
    };
    match desc.write_message(&caller, &mut message, desc_ptr) {
        Ok(()) => SyscallResult::new(SYSCALL_STATUS_OK, [0, 0, 0, 0, 0]),
        Err(error) => error.into_syscall_result(),
    }
}

pub async fn subscribe(args: SyscallArgs) -> SyscallResult {
    fanout_membership(args, MembershipOp::Subscribe).await
}

pub async fn unsubscribe(args: SyscallArgs) -> SyscallResult {
    fanout_membership(args, MembershipOp::Unsubscribe).await
}

pub async fn bind_receiver(args: SyscallArgs) -> SyscallResult {
    bind(args, false).await
}

pub async fn rebind_receiver(args: SyscallArgs) -> SyscallResult {
    bind(args, true).await
}

pub async fn query_state(args: SyscallArgs) -> SyscallResult {
    let slot = args.args()[0] as u32;
    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let handle = match acquire_handle(&process, slot) {
        Ok(handle) => handle,
        Err(err) => return err.into_syscall_result(),
    };

    match query_with_handle(&handle) {
        Ok(frame) => SyscallResult::from(frame),
        Err(err) => err.into_syscall_result(),
    }
}

pub async fn close(args: SyscallArgs) -> SyscallResult {
    lifecycle(args, false).await
}

pub async fn freeze(args: SyscallArgs) -> SyscallResult {
    lifecycle(args, true).await
}

fn acquire_handle(process: &Arc<Process>, slot: u32) -> Result<Handle, ObjectError> {
    process.acquire_handle(slot)
}

fn resolve_process(process: &Arc<Process>, slot: u32) -> Result<Handle, ObjectError> {
    if slot == u32::MAX {
        process.task_process_handle()
    } else {
        acquire_handle(process, slot)
    }
}

fn read_process_route(process: &Handle) -> Result<(u64, ProcessInboxSender), ObjectError> {
    process.read_cp_with::<ProcessControl, _, _>(|process| {
        Ok::<_, ObjectError>((
            process.process_id(),
            process.mailbox_control().inbox_sender(),
        ))
    })?
}

async fn send_with_handle(
    caller: &ProcessContext,
    handle: &Handle,
    message: crate::ipc::Message,
    flags: PortSendFlags,
) -> Result<SendOutcome, IpcError> {
    if let Ok(()) = handle.execute_cp_with::<UnicastPort, _, _>(|_port| ()) {
        return send_unicast(caller, handle, message, flags).await;
    }
    if let Ok(()) = handle.execute_cp_with::<BroadcastPort, _, _>(|_port| ()) {
        if flags.contains(PortSendFlags::ASYNC_REPLY) {
            return Err(IpcError::InvalidArgument);
        }
        handle
            .execute_cp_async_with::<BroadcastPort, _, _>(async move |port| {
                port.send(message).await
            })
            .await
            .map_err(|_| IpcError::InvalidPortKind)?
            .map_err(IpcError::from)?;
        return Ok(SendOutcome::Queued);
    }
    if let Ok(()) = handle.execute_cp_with::<BusPort, _, _>(|_port| ()) {
        if flags.contains(PortSendFlags::ASYNC_REPLY) {
            return Err(IpcError::InvalidArgument);
        }
        handle
            .execute_cp_async_with::<BusPort, _, _>(async move |port| port.publish(message).await)
            .await
            .map_err(|_| IpcError::InvalidPortKind)?
            .map_err(IpcError::from)?;
        return Ok(SendOutcome::Queued);
    }
    if let Ok(()) = handle.execute_cp_with::<ReplyPort, _, _>(|_port| ()) {
        if flags.contains(PortSendFlags::ASYNC_REPLY) {
            return Err(IpcError::InvalidArgument);
        }
        handle
            .execute_cp_async_with::<ReplyPort, _, _>(async move |port| port.send(message).await)
            .await
            .map_err(|_| IpcError::InvalidPortKind)?
            .map_err(IpcError::from)?;
        return Ok(SendOutcome::Queued);
    }
    Err(IpcError::InvalidPortKind)
}

async fn send_unicast(
    caller: &ProcessContext,
    handle: &Handle,
    mut message: crate::ipc::Message,
    flags: PortSendFlags,
) -> Result<SendOutcome, IpcError> {
    if message.reply_port().is_some() {
        if flags.contains(PortSendFlags::ASYNC_REPLY) {
            return Err(IpcError::InvalidArgument);
        }
        handle
            .execute_cp_async_with::<UnicastPort, _, _>(async move |port| port.send(message).await)
            .await
            .map_err(|_| IpcError::InvalidPortKind)?
            .map_err(IpcError::from)?;
        return Ok(SendOutcome::Queued);
    }

    let (owner, sender, receiver_handle) = ReplyPort::create_endpoints()
        .map_err(|_| IpcError::InvalidMessage)?
        .into_parts();
    message.set_reply_port(Some(sender));

    handle
        .execute_cp_async_with::<UnicastPort, _, _>(async move |port| port.send(message).await)
        .await
        .map_err(|_| IpcError::InvalidPortKind)?
        .map_err(IpcError::from)?;

    if flags.contains(PortSendFlags::ASYNC_REPLY) {
        let current = caller.process();
        current
            .retain_anonymous_owner(owner)
            .map_err(|_| IpcError::InvalidMessage)?;
        let slot = current.install_handle_auto(receiver_handle);
        Ok(SendOutcome::ReplyHandle(slot))
    } else {
        let reply = receiver_handle
            .execute_cp_async_with::<ReplyPort, _, _>(async move |port| port.recv().await)
            .await
            .map_err(|_| IpcError::InvalidPortKind)?
            .map_err(RecvIpcError::from)
            .map_err(IpcError::from)?;
        Ok(SendOutcome::Reply(reply))
    }
}

async fn recv_with_handle(
    caller: &ProcessContext,
    handle: &Handle,
) -> Result<crate::ipc::Message, IpcError> {
    if let Ok(port_id) = handle.execute_cp_with::<UnicastPort, _, _>(|port| port.id()) {
        return recv_process_message(caller, port_id).await;
    }
    if let Ok(port_id) = handle.execute_cp_with::<BroadcastPort, _, _>(|port| port.id()) {
        return recv_process_message(caller, port_id).await;
    }
    if let Ok(port_id) = handle.execute_cp_with::<BusPort, _, _>(|port| port.id()) {
        return recv_process_message(caller, port_id).await;
    }
    if let Ok(()) = handle.execute_cp_with::<ReplyPort, _, _>(|_port| ()) {
        return handle
            .execute_cp_async_with::<ReplyPort, _, _>(async move |port| port.recv().await)
            .await
            .map_err(|_| IpcError::InvalidPortKind)?
            .map_err(RecvIpcError::from)
            .map_err(IpcError::from);
    }
    Err(IpcError::InvalidPortKind)
}

async fn recv_process_message(
    caller: &ProcessContext,
    port_id: u64,
) -> Result<crate::ipc::Message, IpcError> {
    caller
        .process()
        .clone()
        .recv_message(port_id)
        .await
        .map_err(RecvIpcError::from)
        .map_err(IpcError::from)
}

async fn fanout_membership(args: SyscallArgs, op: MembershipOp) -> SyscallResult {
    let port_slot = args.args()[0] as u32;
    let process_slot = args.args()[1] as u32;
    let current = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let handle = match acquire_handle(&current, port_slot) {
        Ok(handle) => handle,
        Err(err) => return err.into_syscall_result(),
    };
    let target = match resolve_process(&current, process_slot) {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let (pid, inbox) = match read_process_route(&target) {
        Ok(route) => route,
        Err(err) => return err.into_syscall_result(),
    };

    let result = if let Ok(()) = handle.write_cp_with::<BroadcastPort, _, _>(|_port| ()) {
        handle.write_cp_with::<BroadcastPort, _, _>(|port| match op {
            MembershipOp::Subscribe => port.subscribe(pid, inbox),
            MembershipOp::Unsubscribe => port.unsubscribe(pid),
        })
    } else if let Ok(()) = handle.write_cp_with::<BusPort, _, _>(|_port| ()) {
        handle.write_cp_with::<BusPort, _, _>(|port| match op {
            MembershipOp::Subscribe => port.subscribe(pid, inbox),
            MembershipOp::Unsubscribe => port.unsubscribe(pid),
        })
    } else {
        return IpcError::InvalidPortKind.into_syscall_result();
    };

    match result {
        Ok(Ok(())) => SyscallResult::new(SYSCALL_STATUS_OK, [pid as usize, 0, 0, 0, 0]),
        Ok(Err(err)) => IpcError::from(err).into_syscall_result(),
        Err(err) => err.into_syscall_result(),
    }
}

async fn bind(args: SyscallArgs, rebind: bool) -> SyscallResult {
    let port_slot = args.args()[0] as u32;
    let process_slot = args.args()[1] as u32;
    let current = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let handle = match acquire_handle(&current, port_slot) {
        Ok(handle) => handle,
        Err(err) => return err.into_syscall_result(),
    };
    let target = match resolve_process(&current, process_slot) {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let (pid, inbox) = match read_process_route(&target) {
        Ok(route) => route,
        Err(err) => return err.into_syscall_result(),
    };

    let result = if rebind {
        handle.admin_cp_with::<UnicastPort, _, _>(|port| port.rebind_receiver(pid, inbox))
    } else {
        handle.write_cp_with::<UnicastPort, _, _>(|port| port.bind_receiver(pid, inbox))
    };

    match result {
        Ok(Ok(())) => SyscallResult::new(SYSCALL_STATUS_OK, [pid as usize, 0, 0, 0, 0]),
        Ok(Err(err)) => IpcError::from(err).into_syscall_result(),
        Err(err) => err.into_syscall_result(),
    }
}

fn query_with_handle(handle: &Handle) -> Result<[usize; 6], ObjectError> {
    if let Ok(kind) = handle.read_cp_with::<UnicastPort, _, _>(|port| {
        let stats = port.stats();
        Ok(SyscallResult::new(
            SYSCALL_STATUS_OK,
            [
                0,
                port.state() as usize,
                stats.accepted as usize,
                stats.failed as usize,
                port.receiver_pid().unwrap_or(usize::MAX as u64) as usize,
            ],
        )
        .to_words())
    }) {
        return kind;
    }
    if let Ok(kind) = handle.read_cp_with::<BroadcastPort, _, _>(|port| {
        let stats = port.stats();
        Ok(SyscallResult::new(
            SYSCALL_STATUS_OK,
            [
                1,
                port.state() as usize,
                stats.accepted as usize,
                stats.failed as usize,
                port.subscriber_count(),
            ],
        )
        .to_words())
    }) {
        return kind;
    }
    if let Ok(kind) = handle.read_cp_with::<BusPort, _, _>(|port| {
        let stats = port.stats();
        Ok(SyscallResult::new(
            SYSCALL_STATUS_OK,
            [
                2,
                port.state() as usize,
                stats.accepted as usize,
                stats.failed as usize,
                port.subscriber_count(),
            ],
        )
        .to_words())
    }) {
        return kind;
    }
    if let Ok(kind) = handle.read_cp_with::<ReplyPort, _, _>(|_port| {
        Ok(
            SyscallResult::new(SYSCALL_STATUS_OK, [3, PortState::Open as usize, 0, 0, 0])
                .to_words(),
        )
    }) {
        return kind;
    }
    Err(ObjectError::InvalidArgument)
}

async fn lifecycle(args: SyscallArgs, freeze: bool) -> SyscallResult {
    let slot = args.args()[0] as u32;
    let process = match SyscallEnvironment::current_process_ref() {
        Ok(process) => process,
        Err(err) => return err.into_syscall_result(),
    };
    let handle = match acquire_handle(&process, slot) {
        Ok(handle) => handle,
        Err(err) => return err.into_syscall_result(),
    };

    let result = if let Ok(()) = handle.admin_cp_with::<UnicastPort, _, _>(|_port| ()) {
        handle.admin_cp_with::<UnicastPort, _, _>(|port| {
            if freeze {
                port.freeze();
            } else {
                port.close();
            }
        })
    } else if let Ok(()) = handle.admin_cp_with::<BroadcastPort, _, _>(|_port| ()) {
        handle.admin_cp_with::<BroadcastPort, _, _>(|port| {
            if freeze {
                port.freeze();
            } else {
                port.close();
            }
        })
    } else if let Ok(()) = handle.admin_cp_with::<BusPort, _, _>(|_port| ()) {
        handle.admin_cp_with::<BusPort, _, _>(|port| {
            if freeze {
                port.freeze();
            } else {
                port.close();
            }
        })
    } else {
        return IpcError::InvalidPortKind.into_syscall_result();
    };

    match result {
        Ok(()) => SyscallResult::new(SYSCALL_STATUS_OK, [0, 0, 0, 0, 0]),
        Err(err) => err.into_syscall_result(),
    }
}

enum MembershipOp {
    Subscribe,
    Unsubscribe,
}
