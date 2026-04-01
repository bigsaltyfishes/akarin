/// Port interface capability: send one message through the port.
pub const P_SEND_MSG: u32 = 1 << 0;
/// Port interface capability: receive one message from the port.
pub const P_RECV_MSG: u32 = 1 << 1;
/// Port interface capability: subscribe the caller to one fan-out port.
pub const P_SUBSCRIBE: u32 = 1 << 2;
/// Port interface capability: unsubscribe the caller from one fan-out port.
pub const P_UNSUBSCRIBE: u32 = 1 << 3;
/// Port interface capability: bind one logical receive endpoint.
pub const P_BIND_RECV: u32 = 1 << 4;
/// Port interface capability: replace the currently bound receive endpoint.
pub const P_REBIND_RECV: u32 = 1 << 5;
/// Port interface capability: query runtime state and statistics.
pub const P_QUERY_STATE: u32 = 1 << 6;
/// Port interface capability: mutate port policy.
pub const P_SET_POLICY: u32 = 1 << 7;
/// Port interface capability: close or freeze the port.
pub const P_CLOSE_PORT: u32 = 1 << 8;
/// Port interface capability: drop queued messages.
pub const P_DRAIN: u32 = 1 << 9;
/// Port interface capability: install one message filter.
pub const P_SET_FILTER: u32 = 1 << 10;
/// Port interface capability: publish one message to one bus port.
pub const P_PUBLISH: u32 = 1 << 11;
/// Port interface capability: listen on one bus route.
pub const P_LISTEN: u32 = 1 << 12;
/// Port interface capability: forward on behalf of another principal.
pub const P_FORWARD: u32 = 1 << 13;
