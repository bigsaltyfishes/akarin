use alloc::boxed::Box;
use core::ops::Deref;

use async_trait::async_trait;
use libakarin_object::{
    Capability, ControlPlane, Handle, ObjectError, ObjectSyscallContext, Payload, SyscallDispatch,
};
use libakarin_sync::asynchronous::{Receiver, RecvError, SendError, Sender, bounded};
use libakarin_syscall::{IpcError, PortMethod, PortUserMessage, SyscallResult};

use super::{
    IpcInvokeFrame, Message, P_QUERY_STATE, P_RECV_MSG, P_SEND_MSG, PortSendError,
    PortUserMessageError, PortUserMessageExt,
};

/// One transient reply-port endpoint bundle.
pub struct ReplyPortEndpoints {
    owner: Handle,
    sender: Handle,
    receiver: Handle,
}

/// One anonymous, point-to-point reply port.
///
/// Reply ports are intentionally lightweight and are expected to be created
/// frequently. They are anonymous objects and therefore never enter the global
/// object namespace.
pub struct ReplyPort {
    sender: Sender<Message>,
    receiver: Receiver<Message>,
}

impl ReplyPort {
    /// Create one empty one-shot reply port.
    pub fn new() -> Self {
        let (sender, receiver) = bounded::<Message>(1);
        Self { sender, receiver }
    }

    /// Create one transient reply port and derive split send/receive handles.
    pub fn create_endpoints() -> Result<ReplyPortEndpoints, ObjectError> {
        let owner = Handle::new_anonymous(
            Payload::new(Self::new()),
            Capability::ADMIN | Capability::AGENT,
        );
        let sender = owner.derive_handle(Capability::SEND | Capability::EXECUTE, P_SEND_MSG)?;
        let receiver = owner.derive_handle(
            Capability::READ | Capability::EXECUTE,
            P_RECV_MSG | P_QUERY_STATE,
        )?;
        Ok(ReplyPortEndpoints {
            owner,
            sender,
            receiver,
        })
    }

    /// Send one reply message.
    pub async fn send(&self, message: Message) -> Result<(), PortSendError> {
        if let Err(reason) = message.validate() {
            return Err(PortSendError::InvalidMessage { message, reason });
        }
        self.sender
            .send(message)
            .await
            .map_err(|SendError::Closed(message)| PortSendError::ReceiverClosed(message))
    }

    /// Send one reply without an async runtime.
    pub fn send_blocking(&self, message: Message) -> Result<(), PortSendError> {
        if let Err(reason) = message.validate() {
            return Err(PortSendError::InvalidMessage { message, reason });
        }
        self.sender
            .send_blocking(message)
            .map_err(|SendError::Closed(message)| PortSendError::ReceiverClosed(message))
    }

    /// Try to receive one reply immediately.
    pub fn try_recv(&self) -> Result<Message, RecvError> {
        self.receiver.try_recv()
    }

    /// Receive one reply asynchronously.
    pub async fn recv(&self) -> Result<Message, RecvError> {
        self.receiver.recv().await
    }

    /// Receive one reply without an async runtime.
    pub fn recv_blocking(&self) -> Result<Message, RecvError> {
        self.receiver.recv_blocking()
    }

    /// Clone one sender endpoint so async send can outlive the control-plane
    /// borrow.
    pub fn sender_endpoint(&self) -> Sender<Message> {
        self.sender.clone()
    }

    /// Clone one receiver endpoint so async receive can outlive the
    /// control-plane borrow.
    pub fn receiver_endpoint(&self) -> Receiver<Message> {
        self.receiver.clone()
    }
}

impl ReplyPortEndpoints {
    /// Consume this bundle and return its three handles.
    pub fn into_parts(self) -> (Handle, Handle, Handle) {
        (self.owner, self.sender, self.receiver)
    }
}

pub struct ReplyPortGuard<'a> {
    port: &'a ReplyPort,
    interface_caps: u32,
    mode: GuardMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuardMode {
    Read,
    Write,
    Execute,
    Agent,
    Admin,
}

impl GuardMode {
    fn allows(self, method: PortMethod) -> bool {
        match self {
            Self::Admin | Self::Agent => {
                matches!(
                    method,
                    PortMethod::QueryState | PortMethod::Send | PortMethod::Recv
                )
            }
            Self::Read => matches!(method, PortMethod::QueryState),
            Self::Write => false,
            Self::Execute => matches!(method, PortMethod::Send | PortMethod::Recv),
        }
    }
}

impl ReplyPort {
    fn recv_ipc_error(error: RecvError) -> IpcError {
        match error {
            RecvError::Empty => IpcError::WouldBlock,
            RecvError::Closed => IpcError::ReceiverClosed,
        }
    }
}

impl<'a> ReplyPortGuard<'a> {
    fn new(port: &'a ReplyPort, interface_caps: u32, mode: GuardMode) -> Self {
        Self {
            port,
            interface_caps,
            mode,
        }
    }

    fn require_method(&self, method: PortMethod, caps: u32) -> Result<(), ObjectError> {
        if !self.mode.allows(method) {
            return Err(ObjectError::InsufficientCapabilities);
        }

        if self.interface_caps == u32::MAX || (self.interface_caps & caps) == caps {
            Ok(())
        } else {
            Err(ObjectError::InsufficientCapabilities)
        }
    }

    fn dispatch_ipc_error(error: IpcError) -> Result<SyscallResult, ObjectError> {
        Ok(IpcInvokeFrame::ipc_error(error).into())
    }

    fn dispatch_user_error(error: PortUserMessageError) -> Result<SyscallResult, ObjectError> {
        match error.into_object_or_underlying() {
            Ok(error) => Err(error),
            Err(error) => Self::dispatch_ipc_error(error),
        }
    }
}

impl Deref for ReplyPortGuard<'_> {
    type Target = ReplyPort;

    fn deref(&self) -> &Self::Target {
        self.port
    }
}

impl ControlPlane for ReplyPort {
    type ReadGuard<'a>
        = ReplyPortGuard<'a>
    where
        Self: 'a;
    type WriteGuard<'a>
        = ReplyPortGuard<'a>
    where
        Self: 'a;
    type ExecuteGuard<'a>
        = ReplyPortGuard<'a>
    where
        Self: 'a;
    type AgentGuard<'a>
        = ReplyPortGuard<'a>
    where
        Self: 'a;
    type AdminGuard<'a>
        = ReplyPortGuard<'a>
    where
        Self: 'a;

    fn read(&self, interface_caps: u32) -> Self::ReadGuard<'_> {
        ReplyPortGuard::new(self, interface_caps, GuardMode::Read)
    }

    fn write(&self, interface_caps: u32) -> Self::WriteGuard<'_> {
        ReplyPortGuard::new(self, interface_caps, GuardMode::Write)
    }

    fn execute(&self, interface_caps: u32) -> Self::ExecuteGuard<'_> {
        ReplyPortGuard::new(self, interface_caps, GuardMode::Execute)
    }

    fn agent(&self, interface_caps: u32) -> Self::AgentGuard<'_> {
        ReplyPortGuard::new(self, interface_caps, GuardMode::Agent)
    }

    fn admin(&self, interface_caps: u32) -> Self::AdminGuard<'_> {
        ReplyPortGuard::new(self, interface_caps, GuardMode::Admin)
    }
}

#[async_trait]
impl SyscallDispatch<ObjectSyscallContext> for ReplyPortGuard<'_> {
    async fn dispatch(
        &self,
        caller: &ObjectSyscallContext,
        method_id: usize,
        arg1: usize,
        _arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        let Ok(method) = PortMethod::try_from(method_id) else {
            return Self::dispatch_ipc_error(IpcError::InvalidArgument);
        };
        match method {
            PortMethod::QueryState => {
                self.require_method(method, P_QUERY_STATE)?;
                Ok(IpcInvokeFrame::empty_ok().into())
            }
            PortMethod::Send => {
                self.require_method(method, P_SEND_MSG)?;
                let desc = match PortUserMessage::read(caller, arg1) {
                    Ok(desc) => desc,
                    Err(_) => return Self::dispatch_ipc_error(IpcError::Fault),
                };
                let (message, transfer) = match desc.into_message(caller) {
                    Ok(message) => message,
                    Err(error) => return Self::dispatch_user_error(error),
                };
                if let Err(error) = self
                    .port
                    .send(message)
                    .await
                    .map_err(PortSendError::ipc_error)
                {
                    return Self::dispatch_ipc_error(error);
                }
                if let Err(error) = transfer.commit(caller) {
                    return Self::dispatch_user_error(error);
                }
                Ok(IpcInvokeFrame::empty_ok().into())
            }
            PortMethod::Recv => {
                self.require_method(method, P_RECV_MSG)?;
                let mut message = match self.port.recv().await {
                    Ok(message) => message,
                    Err(error) => {
                        return Self::dispatch_ipc_error(ReplyPort::recv_ipc_error(error));
                    }
                };
                let desc = match PortUserMessage::read(caller, arg1) {
                    Ok(desc) => desc,
                    Err(_) => return Self::dispatch_ipc_error(IpcError::Fault),
                };
                match desc.write_message(caller, &mut message, arg1) {
                    Ok(()) => Ok(IpcInvokeFrame::empty_ok().into()),
                    Err(error) => Self::dispatch_user_error(error),
                }
            }
            _ => Self::dispatch_ipc_error(IpcError::InvalidArgument),
        }
    }
}
