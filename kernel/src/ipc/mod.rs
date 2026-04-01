#![allow(dead_code)]

mod cap;
mod inbox;
mod message;
mod port;
mod reply;
mod user;

use libakarin_syscall::{IpcError, SYSCALL_STATUS_OK, SyscallFailure, SyscallResult};

struct IpcInvokeFrame;

impl IpcInvokeFrame {
    fn ok(values: [usize; 5]) -> [usize; 6] {
        [
            SYSCALL_STATUS_OK,
            values[0],
            values[1],
            values[2],
            values[3],
            values[4],
        ]
    }

    fn empty_ok() -> [usize; 6] {
        Self::ok([0, 0, 0, 0, 0])
    }

    fn ipc_error(error: IpcError) -> [usize; 6] {
        SyscallResult::from(SyscallFailure::from(error)).to_words()
    }
}

#[allow(unused_imports)]
pub use cap::{
    P_BIND_RECV, P_CLOSE_PORT, P_DRAIN, P_FORWARD, P_LISTEN, P_PUBLISH, P_QUERY_STATE,
    P_REBIND_RECV, P_RECV_MSG, P_SEND_MSG, P_SET_FILTER, P_SET_POLICY, P_SUBSCRIBE, P_UNSUBSCRIBE,
};
#[allow(unused_imports)]
pub use inbox::{ProcessInbox, ProcessInboxReceiver, ProcessInboxSender, QueuedMessage};
#[allow(unused_imports)]
pub use message::{Message, MessageFlags, MessageHeader, MessageValidationError};
#[allow(unused_imports)]
pub use port::{
    BroadcastPort, BusPort, PortBindError, PortKind, PortSendError, PortState, PortStats,
    UnicastPort,
};
pub use reply::ReplyPort;
pub use user::{PortUserMessageError, PortUserMessageExt};
