use crate::{PortCreateKind, PortQueryState, PortSendFlags, SyscallArgs, SyscallResult};

/// Transport used to enter the kernel from user space or tests.
pub trait SyscallInvoker {
    type Error;

    /// Invoke one syscall frame and return the raw kernel result.
    fn invoke(&self, args: SyscallArgs) -> Result<SyscallResult, Self::Error>;
}

impl<T> SyscallInvoker for &T
where
    T: SyscallInvoker + ?Sized,
{
    type Error = T::Error;

    fn invoke(&self, args: SyscallArgs) -> Result<SyscallResult, Self::Error> {
        (*self).invoke(args)
    }
}

impl<T> SyscallInvoker for &mut T
where
    T: SyscallInvoker + ?Sized,
{
    type Error = T::Error;

    fn invoke(&self, args: SyscallArgs) -> Result<SyscallResult, Self::Error> {
        (**self).invoke(args)
    }
}

/// User-space wrapper error for one syscall submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvokeError<E> {
    /// The transport failed before the kernel returned a result.
    Transport(E),
    /// The kernel returned one failed syscall result.
    Kernel(SyscallResult),
}

impl<E> InvokeError<E> {
    /// Return the embedded transport error if this failure happened before the
    /// kernel produced one result frame.
    pub fn transport(self) -> Option<E> {
        match self {
            Self::Transport(err) => Some(err),
            Self::Kernel(_) => None,
        }
    }

    /// Return the raw failed kernel result frame.
    pub fn kernel(self) -> Option<SyscallResult> {
        match self {
            Self::Transport(_) => None,
            Self::Kernel(result) => Some(result),
        }
    }
}

fn require_ok<E>(result: SyscallResult) -> Result<SyscallResult, InvokeError<E>> {
    if result.is_ok() {
        Ok(result)
    } else {
        Err(InvokeError::Kernel(result))
    }
}

/// Typed IPC entry points built on top of the raw six-word syscall ABI.
pub struct IpcClient<I> {
    invoker: I,
}

impl<I> IpcClient<I> {
    /// Wrap one raw syscall transport with IPC-specific helpers.
    pub const fn new(invoker: I) -> Self {
        Self { invoker }
    }

    /// Borrow the underlying syscall transport.
    pub const fn invoker(&self) -> &I {
        &self.invoker
    }
}

impl<I> IpcClient<I>
where
    I: SyscallInvoker,
{
    /// Create one IPC port and return the installed caller-local handle slot.
    pub fn create(&self, kind: PortCreateKind) -> Result<u32, InvokeError<I::Error>> {
        let result = self
            .invoker
            .invoke(SyscallArgs::port_create(kind))
            .map_err(InvokeError::Transport)?;
        let result = require_ok(result)?;
        Ok(result.values[0] as u32)
    }

    /// Send one message and wait for the kernel-side reply path to finish.
    ///
    /// For `Unicast` ports this is the default request/reply mode. If the
    /// message does not carry one explicit reply port, the kernel creates a
    /// transient reply port internally and blocks the caller until the reply
    /// arrives.
    pub fn send(&self, slot: u32, desc_ptr: usize) -> Result<(), InvokeError<I::Error>> {
        let result = self
            .invoker
            .invoke(SyscallArgs::port_send(slot, desc_ptr))
            .map_err(InvokeError::Transport)?;
        require_ok(result).map(|_| ())
    }

    /// Queue one message and return immediately with one reply-port handle
    /// slot installed into the caller.
    pub fn send_async(&self, slot: u32, desc_ptr: usize) -> Result<u32, InvokeError<I::Error>> {
        let result = self
            .invoker
            .invoke(SyscallArgs::port_send_async(slot, desc_ptr))
            .map_err(InvokeError::Transport)?;
        let result = require_ok(result)?;
        Ok(result.values[0] as u32)
    }

    /// Send one message with explicit mode flags.
    pub fn send_with_flags(
        &self,
        slot: u32,
        desc_ptr: usize,
        flags: PortSendFlags,
    ) -> Result<SyscallResult, InvokeError<I::Error>> {
        self.invoker
            .invoke(SyscallArgs::port_send_with_flags(slot, desc_ptr, flags))
            .map_err(InvokeError::Transport)
            .and_then(require_ok)
    }

    /// Receive one message from the target port into the supplied descriptor.
    pub fn recv(&self, slot: u32, desc_ptr: usize) -> Result<(), InvokeError<I::Error>> {
        let result = self
            .invoker
            .invoke(SyscallArgs::port_recv(slot, desc_ptr))
            .map_err(InvokeError::Transport)?;
        require_ok(result).map(|_| ())
    }

    /// Subscribe one process to one fan-out port.
    pub fn subscribe(
        &self,
        port_slot: u32,
        process_slot: u32,
    ) -> Result<(), InvokeError<I::Error>> {
        let result = self
            .invoker
            .invoke(SyscallArgs::port_subscribe(port_slot, process_slot))
            .map_err(InvokeError::Transport)?;
        require_ok(result).map(|_| ())
    }

    /// Remove one process from one fan-out port subscription set.
    pub fn unsubscribe(
        &self,
        port_slot: u32,
        process_slot: u32,
    ) -> Result<(), InvokeError<I::Error>> {
        let result = self
            .invoker
            .invoke(SyscallArgs::port_unsubscribe(port_slot, process_slot))
            .map_err(InvokeError::Transport)?;
        require_ok(result).map(|_| ())
    }

    /// Bind the receiving process of one unicast port.
    pub fn bind_receiver(
        &self,
        port_slot: u32,
        process_slot: u32,
    ) -> Result<(), InvokeError<I::Error>> {
        let result = self
            .invoker
            .invoke(SyscallArgs::port_bind_receiver(port_slot, process_slot))
            .map_err(InvokeError::Transport)?;
        require_ok(result).map(|_| ())
    }

    /// Replace the receiving process of one unicast port.
    pub fn rebind_receiver(
        &self,
        port_slot: u32,
        process_slot: u32,
    ) -> Result<(), InvokeError<I::Error>> {
        let result = self
            .invoker
            .invoke(SyscallArgs::port_rebind_receiver(port_slot, process_slot))
            .map_err(InvokeError::Transport)?;
        require_ok(result).map(|_| ())
    }

    /// Query one port state snapshot.
    pub fn query_state(&self, slot: u32) -> Result<PortQueryState, InvokeError<I::Error>> {
        let result = self
            .invoker
            .invoke(SyscallArgs::port_query_state(slot))
            .map_err(InvokeError::Transport)?;
        let result = require_ok(result)?;
        PortQueryState::from_result(result).map_err(|_| InvokeError::Kernel(result))
    }

    /// Close one port object.
    pub fn close(&self, slot: u32) -> Result<(), InvokeError<I::Error>> {
        let result = self
            .invoker
            .invoke(SyscallArgs::port_close(slot))
            .map_err(InvokeError::Transport)?;
        require_ok(result).map(|_| ())
    }

    /// Freeze one port object.
    pub fn freeze(&self, slot: u32) -> Result<(), InvokeError<I::Error>> {
        let result = self
            .invoker
            .invoke(SyscallArgs::port_freeze(slot))
            .map_err(InvokeError::Transport)?;
        require_ok(result).map(|_| ())
    }
}
