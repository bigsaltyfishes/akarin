//! Service request frame definitions.

use libakarin_object::CpAccessMode;

/// Service operation codes delivered to one general userspace object server.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectServiceOp {
    /// One regular control-plane invoke request.
    Invoke = 1,
}

/// Service operation codes delivered to one userspace pager thread.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PagerRequestOp {
    /// One page-fault resolution request.
    Fault = 1,
}

/// One raw six-word service request frame delivered to one waiting service
/// thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceFrame {
    words: [usize; 6],
}

impl ServiceFrame {
    /// Create one raw service request frame from six caller-supplied words.
    pub const fn new(words: [usize; 6]) -> Self {
        Self { words }
    }

    /// Return the raw words stored in this frame.
    pub const fn words(self) -> [usize; 6] {
        self.words
    }

    /// Encode one general userspace-object request frame.
    pub const fn object_request(
        mode: CpAccessMode,
        interface_caps: u32,
        method_id: usize,
        arg0: usize,
        arg1: usize,
    ) -> Self {
        Self::new([
            ObjectServiceOp::Invoke as usize,
            mode as usize,
            interface_caps as usize,
            method_id,
            arg0,
            arg1,
        ])
    }

    /// Encode one pager fault request frame.
    pub const fn pager_fault_request(
        fault_addr: usize,
        access_flags: usize,
        backing_cookie: usize,
        page_offset: usize,
        request_flags: usize,
    ) -> Self {
        Self::new([
            PagerRequestOp::Fault as usize,
            fault_addr,
            access_flags,
            backing_cookie,
            page_offset,
            request_flags,
        ])
    }

    /// Encode one forwarded syscall request frame.
    pub const fn syscall_request(syscall_nr: usize, args: [usize; 5]) -> Self {
        Self::new([syscall_nr, args[0], args[1], args[2], args[3], args[4]])
    }
}

/// One pending service delivery prepared for a waiting task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceDelivery {
    /// Userspace entry point to resume once the waiting thread is selected.
    pub usr_ip: usize,
    /// Service request frame written into the trap context before resume.
    pub frame: ServiceFrame,
}

impl ServiceDelivery {
    /// Create one pending service delivery from the selected entry point and
    /// request frame.
    pub const fn new(usr_ip: usize, frame: ServiceFrame) -> Self {
        Self { usr_ip, frame }
    }
}

/// High-level role attached to one task currently participating in the
/// userspace service subsystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceRole {
    /// The task is servicing one general userspace object request.
    Object,
    /// The task is servicing one future pager request.
    Pager,
    /// The task is servicing one future syscall-handler request.
    SyscallHandler,
}

/// Waiting discipline currently expected from one service thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceWaitKind {
    /// The task is blocked in `WaitObjectRequest`.
    Object,
    /// The task is blocked in one future pager wait path.
    Pager,
    /// The task is blocked in one future syscall-handler wait path.
    SyscallHandler,
}

impl ServiceWaitKind {
    /// Return the service role implied by this wait kind.
    pub const fn role(self) -> ServiceRole {
        match self {
            Self::Object => ServiceRole::Object,
            Self::Pager => ServiceRole::Pager,
            Self::SyscallHandler => ServiceRole::SyscallHandler,
        }
    }
}
