//! System call ABI definitions shared by kernel and user space.

use core::{
    mem::{MaybeUninit, size_of},
    ops::{BitOr, BitOrAssign},
    slice,
};

use crate::{
    dispatch::SyscallContext,
    errno::{
        FutexError, IpcError, IrqUnderlyingErrorCode, ObjectError, PciUnderlyingErrorCode,
        ProcessHandleInstallError, ProcessLoadError, ProcessSpawnError, ProcessVmarExtractError,
        ProcessWaitError, SyscallError, SyscallFailure, UnderlyingFailure, VmError,
    },
};

/// System call arguments passed from user space to the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyscallArgs {
    pub method_id: usize,
    pub args: [usize; 5],
}

impl SyscallArgs {
    pub const fn new(method_id: usize, args: [usize; 5]) -> Self {
        Self { method_id, args }
    }

    /// Build one typed syscall frame from a stable syscall number.
    pub const fn from_syscall(syscall: Syscall, args: [usize; 5]) -> Self {
        Self::new(syscall as usize, args)
    }

    /// Build one IPC port-creation syscall frame.
    pub const fn port_create(kind: PortCreateKind) -> Self {
        Self::from_syscall(Syscall::PortCreate, [kind as usize, 0, 0, 0, 0])
    }

    /// Build one IPC send syscall frame.
    pub const fn port_send(slot: u32, desc_ptr: usize) -> Self {
        Self::port_send_with_flags(slot, desc_ptr, PortSendFlags::NONE)
    }

    /// Build one synchronous IPC send syscall frame with explicit flags.
    pub const fn port_send_with_flags(slot: u32, desc_ptr: usize, flags: PortSendFlags) -> Self {
        Self::from_syscall(
            Syscall::PortSend,
            [slot as usize, desc_ptr, flags.bits(), 0, 0],
        )
    }

    /// Build one asynchronous IPC send syscall frame.
    pub const fn port_send_async(slot: u32, desc_ptr: usize) -> Self {
        Self::port_send_with_flags(slot, desc_ptr, PortSendFlags::ASYNC_REPLY)
    }

    /// Build one IPC receive syscall frame.
    pub const fn port_recv(slot: u32, desc_ptr: usize) -> Self {
        Self::from_syscall(Syscall::PortRecv, [slot as usize, desc_ptr, 0, 0, 0])
    }

    /// Build one IPC subscribe syscall frame.
    pub const fn port_subscribe(port_slot: u32, process_slot: u32) -> Self {
        Self::from_syscall(
            Syscall::PortSubscribe,
            [port_slot as usize, process_slot as usize, 0, 0, 0],
        )
    }

    /// Build one IPC unsubscribe syscall frame.
    pub const fn port_unsubscribe(port_slot: u32, process_slot: u32) -> Self {
        Self::from_syscall(
            Syscall::PortUnsubscribe,
            [port_slot as usize, process_slot as usize, 0, 0, 0],
        )
    }

    /// Build one IPC bind syscall frame.
    pub const fn port_bind_receiver(port_slot: u32, process_slot: u32) -> Self {
        Self::from_syscall(
            Syscall::PortBindReceiver,
            [port_slot as usize, process_slot as usize, 0, 0, 0],
        )
    }

    /// Build one IPC rebind syscall frame.
    pub const fn port_rebind_receiver(port_slot: u32, process_slot: u32) -> Self {
        Self::from_syscall(
            Syscall::PortRebindReceiver,
            [port_slot as usize, process_slot as usize, 0, 0, 0],
        )
    }

    /// Build one IPC query-state syscall frame.
    pub const fn port_query_state(slot: u32) -> Self {
        Self::from_syscall(Syscall::PortQueryState, [slot as usize, 0, 0, 0, 0])
    }

    /// Build one IRQ wait syscall frame.
    pub const fn irq_wait(slot: u32, deadline: usize, flags: IrqWaitFlags) -> Self {
        Self::from_syscall(
            Syscall::IrqWait,
            [slot as usize, deadline, flags.bits(), 0, 0],
        )
    }

    /// Build one IRQ acknowledge syscall frame.
    pub const fn irq_ack(slot: u32, epoch: u64, disposition: IrqAckDisposition) -> Self {
        Self::from_syscall(
            Syscall::IrqAck,
            [slot as usize, epoch as usize, disposition as usize, 0, 0],
        )
    }

    /// Build one IPC close syscall frame.
    pub const fn port_close(slot: u32) -> Self {
        Self::from_syscall(Syscall::PortClose, [slot as usize, 0, 0, 0, 0])
    }

    /// Build one IPC freeze syscall frame.
    pub const fn port_freeze(slot: u32) -> Self {
        Self::from_syscall(Syscall::PortFreeze, [slot as usize, 0, 0, 0, 0])
    }

    /// Build one process-create syscall frame.
    pub const fn process_create(name_ptr: usize, name_len: usize, flags: usize) -> Self {
        Self::from_syscall(Syscall::ProcessCreate, [name_ptr, name_len, flags, 0, 0])
    }

    /// Build one process-load syscall frame.
    pub const fn process_load(process_slot: u32, image_slot: u32, flags: usize) -> Self {
        Self::from_syscall(
            Syscall::ProcessLoad,
            [process_slot as usize, image_slot as usize, flags, 0, 0],
        )
    }

    /// Build one process-segment-VMAR extraction syscall frame.
    pub const fn process_vmar_extract(process_slot: u32, segment: ProcessVmSegment) -> Self {
        Self::from_syscall(
            Syscall::ProcessVmarExtract,
            [process_slot as usize, segment as usize, 0, 0, 0],
        )
    }

    /// Build one process-handle-install syscall frame.
    pub const fn process_handle_install(
        process_slot: u32,
        source_slot: u32,
        capability_bits: usize,
        interface_caps: u32,
        target_slot: u32,
    ) -> Self {
        Self::from_syscall(
            Syscall::ProcessHandleInstall,
            [
                process_slot as usize,
                source_slot as usize,
                capability_bits,
                interface_caps as usize,
                target_slot as usize,
            ],
        )
    }

    /// Build one initial-task-spawn syscall frame.
    pub const fn process_task_spawn(
        process_slot: u32,
        entry: usize,
        user_sp: usize,
        tls_base: usize,
        flags: usize,
    ) -> Self {
        Self::from_syscall(
            Syscall::ProcessTaskSpawn,
            [process_slot as usize, entry, user_sp, tls_base, flags],
        )
    }

    /// Build one userspace service-object creation syscall frame.
    pub const fn user_object_create(
        parent_slot: u32,
        process_slot: u32,
        name_ptr: usize,
        name_len: usize,
        usr_ip: usize,
    ) -> Self {
        Self::from_syscall(
            Syscall::UserObjectCreate,
            [
                parent_slot as usize,
                process_slot as usize,
                name_ptr,
                name_len,
                usr_ip,
            ],
        )
    }

    /// Build one userspace object-service wait syscall frame.
    pub const fn wait_object_request(object_slot: u32) -> Self {
        Self::from_syscall(
            Syscall::WaitObjectRequest,
            [object_slot as usize, 0, 0, 0, 0],
        )
    }

    /// Build one userspace pager-object creation syscall frame.
    pub const fn pager_create(
        register_slot: u32,
        process_slot: u32,
        usr_ip: usize,
        flags: usize,
    ) -> Self {
        Self::from_syscall(
            Syscall::PagerCreate,
            [
                register_slot as usize,
                process_slot as usize,
                usr_ip,
                flags,
                0,
            ],
        )
    }

    /// Build one userspace pager wait syscall frame.
    pub const fn wait_pager_request(pager_slot: u32) -> Self {
        Self::from_syscall(Syscall::WaitPagerRequest, [pager_slot as usize, 0, 0, 0, 0])
    }

    /// Build one userspace syscall-handler creation syscall frame.
    pub const fn syscall_handler_create(
        register_slot: u32,
        process_slot: u32,
        usr_ip: usize,
        syscall_nr: Syscall,
        flags: usize,
    ) -> Self {
        Self::from_syscall(
            Syscall::SyscallHandlerCreate,
            [
                register_slot as usize,
                process_slot as usize,
                usr_ip,
                syscall_nr as usize,
                flags,
            ],
        )
    }

    /// Build one userspace syscall-handler wait syscall frame.
    pub const fn wait_syscall_request(handler_slot: u32) -> Self {
        Self::from_syscall(
            Syscall::WaitSyscallRequest,
            [handler_slot as usize, 0, 0, 0, 0],
        )
    }

    /// Build one successful service reply syscall frame.
    pub const fn service_reply_ok(values: [usize; 5]) -> Self {
        Self::from_syscall(Syscall::ServiceReplyOk, values)
    }

    /// Build one object-error service reply syscall frame.
    pub const fn service_reply_object_error(error: ObjectError) -> Self {
        Self::from_syscall(
            Syscall::ServiceReplyObjectError,
            [error as usize, 0, 0, 0, 0],
        )
    }

    /// Build one underlying-error service reply syscall frame.
    pub const fn service_reply_underlying(kind: usize, code: usize) -> Self {
        Self::from_syscall(Syscall::ServiceReplyUnderlying, [kind, code, 0, 0, 0])
    }

    /// Build one task-create syscall frame.
    pub const fn task_create(
        process_slot: u32,
        entry: usize,
        user_sp: usize,
        tls_base: usize,
        flags: usize,
    ) -> Self {
        Self::from_syscall(
            Syscall::TaskCreate,
            [process_slot as usize, entry, user_sp, tls_base, flags],
        )
    }

    /// Build one task-exit syscall frame.
    pub const fn task_exit(code: usize) -> Self {
        Self::from_syscall(Syscall::TaskExit, [code, 0, 0, 0, 0])
    }

    /// Build one futex wait syscall frame.
    pub const fn futex_wait(
        user_addr: usize,
        expected: u32,
        timeout_ns: usize,
        flags: FutexWaitFlags,
    ) -> Self {
        Self::from_syscall(
            Syscall::FutexWait,
            [user_addr, expected as usize, timeout_ns, flags.bits(), 0],
        )
    }

    /// Build one futex wake syscall frame.
    pub const fn futex_wake(user_addr: usize, wake_count: usize, flags: FutexWakeFlags) -> Self {
        Self::from_syscall(
            Syscall::FutexWake,
            [user_addr, wake_count, flags.bits(), 0, 0],
        )
    }

    /// Build one child-VMO creation syscall frame.
    pub const fn vmo_create_child(
        parent_slot: u32,
        parent_offset: usize,
        size: usize,
        mode: VmoChildMode,
    ) -> Self {
        Self::from_syscall(
            Syscall::VmoCreateChild,
            [parent_slot as usize, parent_offset, size, mode as usize, 0],
        )
    }

    pub fn method_id(&self) -> usize {
        self.method_id
    }

    pub fn args(&self) -> &[usize; 5] {
        &self.args
    }

    pub fn arg(&self, index: usize) -> Option<usize> {
        self.args.get(index).copied()
    }

    pub const fn to_words(self) -> [usize; 6] {
        [
            self.method_id,
            self.args[0],
            self.args[1],
            self.args[2],
            self.args[3],
            self.args[4],
        ]
    }
}

impl From<[usize; 6]> for SyscallArgs {
    fn from(words: [usize; 6]) -> Self {
        Self {
            method_id: words[0],
            args: [words[1], words[2], words[3], words[4], words[5]],
        }
    }
}

/// System call result returned from the kernel to user space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyscallResult {
    pub status: usize,
    pub values: [usize; 5],
}

impl SyscallResult {
    pub const fn new(status: usize, values: [usize; 5]) -> Self {
        Self { status, values }
    }

    /// Return whether the syscall completed successfully.
    pub const fn is_ok(&self) -> bool {
        self.status == SYSCALL_STATUS_OK
    }

    /// Return whether the syscall failed due to one object/capability error.
    pub const fn is_object_error(&self) -> bool {
        self.status == SYSCALL_STATUS_OBJECT_ERROR
    }

    /// Return whether the syscall failed due to one subsystem-defined
    /// underlying error.
    pub const fn is_underlying_error(&self) -> bool {
        self.status == SYSCALL_STATUS_UNDERLYING_ERROR
    }

    /// Return whether the syscall failed.
    pub const fn is_err(&self) -> bool {
        self.status != SYSCALL_STATUS_OK
    }

    pub const fn to_words(self) -> [usize; 6] {
        [
            self.status,
            self.values[0],
            self.values[1],
            self.values[2],
            self.values[3],
            self.values[4],
        ]
    }

    pub fn value(&self, index: usize) -> Option<usize> {
        self.values.get(index).copied()
    }

    /// Decode one failed syscall frame into one typed failure envelope.
    pub fn failure(&self) -> Option<SyscallFailure> {
        match SyscallStatus::try_from(self.status).ok()? {
            SyscallStatus::Ok => None,
            SyscallStatus::ObjectError => {
                let code = self.values[0];
                let object_error = ObjectError::from_abi_code(code)?;
                Some(SyscallFailure::Object(object_error))
            }
            SyscallStatus::UnderlyingError => {
                let code = self.values[0];
                let kind = self.values[1];
                let detail = [self.values[2], self.values[3], self.values[4]];
                Some(SyscallFailure::Underlying(UnderlyingFailure::from_parts(
                    code, kind, detail,
                )))
            }
        }
    }

    /// Build one syscall result from one typed failure envelope.
    pub fn from_failure(failure: SyscallFailure) -> Self {
        match failure {
            SyscallFailure::Object(error) => {
                Self::new(SYSCALL_STATUS_OBJECT_ERROR, [error.abi_code(), 0, 0, 0, 0])
            }
            SyscallFailure::Underlying(error) => {
                let (kind, code, detail) = error.to_parts();
                Self::new(
                    SYSCALL_STATUS_UNDERLYING_ERROR,
                    [code, kind, detail[0], detail[1], detail[2]],
                )
            }
        }
    }
}

impl From<SyscallFailure> for SyscallResult {
    fn from(value: SyscallFailure) -> Self {
        Self::from_failure(value)
    }
}

impl TryFrom<SyscallResult> for SyscallFailure {
    type Error = ();

    fn try_from(value: SyscallResult) -> Result<Self, Self::Error> {
        value.failure().ok_or(())
    }
}

impl From<ObjectError> for SyscallResult {
    fn from(value: ObjectError) -> Self {
        Self::from_failure(SyscallFailure::from(value))
    }
}

macro_rules! impl_from_underlying_result {
    ($ty:ty) => {
        impl From<$ty> for SyscallResult {
            fn from(value: $ty) -> Self {
                Self::from_failure(SyscallFailure::from(value))
            }
        }
    };
}

impl_from_underlying_result!(SyscallError);
impl_from_underlying_result!(IpcError);
impl_from_underlying_result!(VmError);
impl_from_underlying_result!(ProcessLoadError);
impl_from_underlying_result!(ProcessVmarExtractError);
impl_from_underlying_result!(ProcessHandleInstallError);
impl_from_underlying_result!(ProcessSpawnError);
impl_from_underlying_result!(ProcessWaitError);
impl_from_underlying_result!(FutexError);
impl_from_underlying_result!(IrqUnderlyingErrorCode);
impl_from_underlying_result!(PciUnderlyingErrorCode);

impl From<[usize; 6]> for SyscallResult {
    fn from(words: [usize; 6]) -> Self {
        Self {
            status: words[0],
            values: [words[1], words[2], words[3], words[4], words[5]],
        }
    }
}

/// Stable top-level syscall result kinds.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyscallStatus {
    Ok = 0,
    ObjectError = 1,
    UnderlyingError = 2,
}

impl TryFrom<usize> for SyscallStatus {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Ok),
            1 => Ok(Self::ObjectError),
            2 => Ok(Self::UnderlyingError),
            _ => Err(()),
        }
    }
}

/// Successful syscall status word.
pub const SYSCALL_STATUS_OK: usize = SyscallStatus::Ok as usize;
/// Object-error syscall status word.
pub const SYSCALL_STATUS_OBJECT_ERROR: usize = SyscallStatus::ObjectError as usize;
/// Underlying-error syscall status word.
pub const SYSCALL_STATUS_UNDERLYING_ERROR: usize = SyscallStatus::UnderlyingError as usize;

/// Generic ELF-style auxv terminator.
pub const AT_NULL: usize = 0;
/// Generic page-size auxv tag.
pub const AT_PAGESZ: usize = 6;
/// Generic entry-point auxv tag.
pub const AT_ENTRY: usize = 9;

/// Akarin private auxv tag carrying the current process self-handle slot.
pub const AT_AK_PROCESS_SELF: usize = 0x4153_0001;
/// Akarin private auxv tag carrying the bootstrap root-namespace handle slot.
pub const AT_AK_ROOT_NS: usize = 0x4153_0002;
/// Akarin private auxv tag carrying the root-VMAR handle slot.
pub const AT_AK_ROOT_VMAR: usize = 0x4153_0003;
/// Akarin private auxv tag carrying the syscall-table handle slot.
pub const AT_AK_SYSCALL_TABLE: usize = 0x4153_0004;
/// Akarin private auxv tag carrying the system page size.
pub const AT_AK_PAGE_SIZE: usize = 0x4153_0005;
/// Akarin private auxv tag carrying the heap-VMAR handle slot.
pub const AT_AK_HEAP_VMAR: usize = 0x4153_0006;
/// Akarin private auxv tag carrying the userspace heap base address.
pub const AT_AK_HEAP_BASE: usize = 0x4153_0007;
/// Akarin private auxv tag carrying the userspace heap upper bound.
pub const AT_AK_HEAP_LIMIT: usize = 0x4153_0008;
/// Akarin private auxv tag carrying the generic userspace mapping VMAR slot.
pub const AT_AK_MAPPED_VMAR: usize = 0x4153_0009;

/// One raw auxiliary-vector pair passed on the initial userspace stack.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuxEntry {
    pub a_type: usize,
    pub a_val: usize,
}

/// Decoded process image-load information returned by `ProcessLoad`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessLoadInfo {
    pub entry_ip: usize,
    pub image_base: usize,
    pub image_end: usize,
    pub slide: usize,
}

impl ProcessLoadInfo {
    /// Decode one successful `ProcessLoad` syscall result.
    pub fn from_result(result: SyscallResult) -> Result<Self, ()> {
        if !result.is_ok() {
            return Err(());
        }

        Ok(Self {
            entry_ip: result.values[0],
            image_base: result.values[1],
            image_end: result.values[2],
            slide: result.values[3],
        })
    }
}

/// One VMAR map argument block passed by pointer through the fast-syscall ABI.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmarMapArgs {
    pub base: usize,
    pub size: usize,
    pub vmo_offset: usize,
    pub flags: u32,
    pub purpose: usize,
}

/// Stable child-VMO creation modes accepted by `Syscall::VmoCreateChild`.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmoChildMode {
    /// Create one fixed-size shared view into the parent range.
    SharedView = 1,
    /// Create one child that starts shared and splits on the first write
    /// fault of each page.
    PrivateCow = 2,
}

impl TryFrom<usize> for VmoChildMode {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            x if x == Self::SharedView as usize => Ok(Self::SharedView),
            x if x == Self::PrivateCow as usize => Ok(Self::PrivateCow),
            _ => Err(()),
        }
    }
}

/// Stable page-granular VMO range operations accepted by
/// `Syscall::VmoOpRange`.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmoOpRangeOperation {
    /// Materialize committed backing pages across the target range.
    Commit = 1,
    /// Remove committed backing pages across the target range.
    Decommit = 2,
    /// Zero every committed page already backing the target range.
    Zero = 3,
    /// Report the currently committed byte count across the target range.
    QueryCommitted = 4,
}

impl TryFrom<usize> for VmoOpRangeOperation {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            x if x == Self::Commit as usize => Ok(Self::Commit),
            x if x == Self::Decommit as usize => Ok(Self::Decommit),
            x if x == Self::Zero as usize => Ok(Self::Zero),
            x if x == Self::QueryCommitted as usize => Ok(Self::QueryCommitted),
            _ => Err(()),
        }
    }
}

/// Stable top-level syscall numbers.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Syscall {
    HandleClose = 0x0001,
    HandleClone = 0x0002,
    HandleDerive = 0x0003,
    HandleUpgrade = 0x0004,
    ObjectLocate = 0x0010,
    ObjectReadMeta = 0x0011,
    ObjectManageChild = 0x0012,
    ObjectInvoke = 0x0013,
    TaskYield = 0x0020,
    TaskSleep = 0x0021,
    ProcessExit = 0x0022,
    ProcessCreate = 0x0023,
    ProcessLoad = 0x0024,
    ProcessVmarExtract = 0x0025,
    ProcessHandleInstall = 0x0026,
    ProcessTaskSpawn = 0x0027,
    TaskCreate = 0x0028,
    TaskExit = 0x0029,
    FutexWait = 0x002a,
    FutexWake = 0x002b,
    UserObjectCreate = 0x002c,
    WaitObjectRequest = 0x002d,
    ServiceReplyOk = 0x002e,
    ServiceReplyObjectError = 0x002f,
    ServiceReplyUnderlying = 0x0030,
    /// Create one IPC port and install its handle into the caller.
    PortCreate = 0x0031,
    /// Send one IPC message. The default unicast path is synchronous
    /// request/reply; `PortSendFlags::ASYNC_REPLY` returns one reply handle
    /// instead of waiting.
    PortSend = 0x0032,
    /// Receive one IPC message from the selected port.
    PortRecv = 0x0033,
    /// Subscribe one process to one broadcast or bus port.
    PortSubscribe = 0x0034,
    /// Remove one process from one broadcast or bus port subscription set.
    PortUnsubscribe = 0x0035,
    /// Bind one unicast port to its receiving process.
    PortBindReceiver = 0x0036,
    /// Replace the receiving process of one unicast port.
    PortRebindReceiver = 0x0037,
    /// Query one IPC port state snapshot.
    PortQueryState = 0x0038,
    /// Close one IPC port.
    PortClose = 0x0039,
    /// Freeze one IPC port.
    PortFreeze = 0x003a,
    /// Wait for one IRQ delivery on one shared IRQ session.
    IrqWait = 0x0040,
    /// Acknowledge one IRQ delivery epoch on one shared IRQ session.
    IrqAck = 0x0041,
    PagerCreate = 0x0042,
    WaitPagerRequest = 0x0043,
    SyscallHandlerCreate = 0x0044,
    WaitSyscallRequest = 0x0045,
    VmoCreate = 0x0100,
    VmoCreateChild = 0x0101,
    VmoCreateContiguous = 0x0102,
    VmoCreatePhysical = 0x0103,
    VmoGetSize = 0x0104,
    VmoGetStreamSize = 0x0105,
    VmoOpRange = 0x0106,
    VmoRead = 0x0107,
    VmoReplaceAsExecutable = 0x0108,
    VmoSetCachePolicy = 0x0109,
    VmoSetSize = 0x010a,
    VmoSetStreamSize = 0x010b,
    VmoTransferData = 0x010c,
    VmoWrite = 0x010d,
    VmarAllocate = 0x0200,
    VmarDestroy = 0x0201,
    VmarMap = 0x0202,
    VmarMapClock = 0x0203,
    VmarMapIob = 0x0204,
    VmarOpRange = 0x0205,
    VmarProtect = 0x0206,
    VmarUnmap = 0x0207,
}

impl TryFrom<usize> for Syscall {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            x if x == Self::HandleClose as usize => Ok(Self::HandleClose),
            x if x == Self::HandleClone as usize => Ok(Self::HandleClone),
            x if x == Self::HandleDerive as usize => Ok(Self::HandleDerive),
            x if x == Self::HandleUpgrade as usize => Ok(Self::HandleUpgrade),
            x if x == Self::ObjectLocate as usize => Ok(Self::ObjectLocate),
            x if x == Self::ObjectReadMeta as usize => Ok(Self::ObjectReadMeta),
            x if x == Self::ObjectManageChild as usize => Ok(Self::ObjectManageChild),
            x if x == Self::ObjectInvoke as usize => Ok(Self::ObjectInvoke),
            x if x == Self::TaskYield as usize => Ok(Self::TaskYield),
            x if x == Self::TaskSleep as usize => Ok(Self::TaskSleep),
            x if x == Self::ProcessExit as usize => Ok(Self::ProcessExit),
            x if x == Self::ProcessCreate as usize => Ok(Self::ProcessCreate),
            x if x == Self::ProcessLoad as usize => Ok(Self::ProcessLoad),
            x if x == Self::ProcessVmarExtract as usize => Ok(Self::ProcessVmarExtract),
            x if x == Self::ProcessHandleInstall as usize => Ok(Self::ProcessHandleInstall),
            x if x == Self::ProcessTaskSpawn as usize => Ok(Self::ProcessTaskSpawn),
            x if x == Self::TaskCreate as usize => Ok(Self::TaskCreate),
            x if x == Self::TaskExit as usize => Ok(Self::TaskExit),
            x if x == Self::FutexWait as usize => Ok(Self::FutexWait),
            x if x == Self::FutexWake as usize => Ok(Self::FutexWake),
            x if x == Self::UserObjectCreate as usize => Ok(Self::UserObjectCreate),
            x if x == Self::WaitObjectRequest as usize => Ok(Self::WaitObjectRequest),
            x if x == Self::ServiceReplyOk as usize => Ok(Self::ServiceReplyOk),
            x if x == Self::ServiceReplyObjectError as usize => Ok(Self::ServiceReplyObjectError),
            x if x == Self::ServiceReplyUnderlying as usize => Ok(Self::ServiceReplyUnderlying),
            x if x == Self::PortCreate as usize => Ok(Self::PortCreate),
            x if x == Self::PortSend as usize => Ok(Self::PortSend),
            x if x == Self::PortRecv as usize => Ok(Self::PortRecv),
            x if x == Self::PortSubscribe as usize => Ok(Self::PortSubscribe),
            x if x == Self::PortUnsubscribe as usize => Ok(Self::PortUnsubscribe),
            x if x == Self::PortBindReceiver as usize => Ok(Self::PortBindReceiver),
            x if x == Self::PortRebindReceiver as usize => Ok(Self::PortRebindReceiver),
            x if x == Self::PortQueryState as usize => Ok(Self::PortQueryState),
            x if x == Self::PortClose as usize => Ok(Self::PortClose),
            x if x == Self::PortFreeze as usize => Ok(Self::PortFreeze),
            x if x == Self::IrqWait as usize => Ok(Self::IrqWait),
            x if x == Self::IrqAck as usize => Ok(Self::IrqAck),
            x if x == Self::PagerCreate as usize => Ok(Self::PagerCreate),
            x if x == Self::WaitPagerRequest as usize => Ok(Self::WaitPagerRequest),
            x if x == Self::SyscallHandlerCreate as usize => Ok(Self::SyscallHandlerCreate),
            x if x == Self::WaitSyscallRequest as usize => Ok(Self::WaitSyscallRequest),
            x if x == Self::VmoCreate as usize => Ok(Self::VmoCreate),
            x if x == Self::VmoCreateChild as usize => Ok(Self::VmoCreateChild),
            x if x == Self::VmoCreateContiguous as usize => Ok(Self::VmoCreateContiguous),
            x if x == Self::VmoCreatePhysical as usize => Ok(Self::VmoCreatePhysical),
            x if x == Self::VmoGetSize as usize => Ok(Self::VmoGetSize),
            x if x == Self::VmoGetStreamSize as usize => Ok(Self::VmoGetStreamSize),
            x if x == Self::VmoOpRange as usize => Ok(Self::VmoOpRange),
            x if x == Self::VmoRead as usize => Ok(Self::VmoRead),
            x if x == Self::VmoReplaceAsExecutable as usize => Ok(Self::VmoReplaceAsExecutable),
            x if x == Self::VmoSetCachePolicy as usize => Ok(Self::VmoSetCachePolicy),
            x if x == Self::VmoSetSize as usize => Ok(Self::VmoSetSize),
            x if x == Self::VmoSetStreamSize as usize => Ok(Self::VmoSetStreamSize),
            x if x == Self::VmoTransferData as usize => Ok(Self::VmoTransferData),
            x if x == Self::VmoWrite as usize => Ok(Self::VmoWrite),
            x if x == Self::VmarAllocate as usize => Ok(Self::VmarAllocate),
            x if x == Self::VmarDestroy as usize => Ok(Self::VmarDestroy),
            x if x == Self::VmarMap as usize => Ok(Self::VmarMap),
            x if x == Self::VmarMapClock as usize => Ok(Self::VmarMapClock),
            x if x == Self::VmarMapIob as usize => Ok(Self::VmarMapIob),
            x if x == Self::VmarOpRange as usize => Ok(Self::VmarOpRange),
            x if x == Self::VmarProtect as usize => Ok(Self::VmarProtect),
            x if x == Self::VmarUnmap as usize => Ok(Self::VmarUnmap),
            _ => Err(()),
        }
    }
}

/// Immediate-return object metadata items supported by the current syscall ABI.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectMetadata {
    Id = 0,
    Parent = 1,
    MaskedCapabilities = 2,
    Status = 3,
    ChildCount = 4,
    Name = 5,
    LifecycleFlags = 6,
}

impl TryFrom<usize> for ObjectMetadata {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            x if x == Self::Id as usize => Ok(Self::Id),
            x if x == Self::Parent as usize => Ok(Self::Parent),
            x if x == Self::MaskedCapabilities as usize => Ok(Self::MaskedCapabilities),
            x if x == Self::Status as usize => Ok(Self::Status),
            x if x == Self::ChildCount as usize => Ok(Self::ChildCount),
            x if x == Self::Name as usize => Ok(Self::Name),
            x if x == Self::LifecycleFlags as usize => Ok(Self::LifecycleFlags),
            _ => Err(()),
        }
    }
}

/// Stable lifecycle flags returned by object metadata queries.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectLifecycleFlags(u32);

impl ObjectLifecycleFlags {
    const VALID_BITS: u32 = 1 << 0;

    /// No lifecycle attribute is attached to the object.
    pub const NONE: Self = Self(0);
    /// The object may only be removed by the kernel's internal delete path.
    pub const STICKY: Self = Self(1 << 0);

    /// Return the raw lifecycle-flag bits.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Return whether all bits in `other` are set.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Decode one raw lifecycle-flag word, rejecting unknown bits.
    pub const fn from_bits(bits: u32) -> Option<Self> {
        if bits & !Self::VALID_BITS == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }
}

impl BitOr for ObjectLifecycleFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for ObjectLifecycleFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Stable mode flags accepted by `Syscall::FutexWait`.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FutexWaitFlags(usize);

impl FutexWaitFlags {
    const VALID_BITS: usize = 0;

    /// No extra mode flags are set.
    pub const NONE: Self = Self(0);

    /// Return the raw mode bits.
    pub const fn bits(self) -> usize {
        self.0
    }

    /// Return whether all bits in `other` are present in `self`.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Decode one raw flag word, rejecting unsupported bits.
    pub const fn from_bits(bits: usize) -> Option<Self> {
        if bits & !Self::VALID_BITS == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }
}

impl BitOr for FutexWaitFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for FutexWaitFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Stable mode flags accepted by `Syscall::FutexWake`.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FutexWakeFlags(usize);

impl FutexWakeFlags {
    const VALID_BITS: usize = 0;

    /// No extra mode flags are set.
    pub const NONE: Self = Self(0);

    /// Return the raw mode bits.
    pub const fn bits(self) -> usize {
        self.0
    }

    /// Return whether all bits in `other` are present in `self`.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Decode one raw flag word, rejecting unsupported bits.
    pub const fn from_bits(bits: usize) -> Option<Self> {
        if bits & !Self::VALID_BITS == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }
}

impl BitOr for FutexWakeFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for FutexWakeFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Slow-path object-invoke methods supported by process objects.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessMethod {
    /// Return the current lifecycle phase of the target process object.
    QueryPhase = 0,
    /// Install one delegated handle into a creating process.
    InheritHandle = 1,
    /// Abort one creating process and tear down its published state.
    AbortCreation = 2,
    /// Derive one fixed userspace segment VMAR handle from a creating
    /// process.
    DeriveSegmentVmar = 3,
    /// Wait until the target process reports one terminal exit code.
    Wait = 4,
}

impl TryFrom<usize> for ProcessMethod {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::QueryPhase),
            1 => Ok(Self::InheritHandle),
            2 => Ok(Self::AbortCreation),
            3 => Ok(Self::DeriveSegmentVmar),
            4 => Ok(Self::Wait),
            _ => Err(()),
        }
    }
}

/// Fixed userspace layout segments that may be delegated during process
/// creation.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessVmSegment {
    UserImage = 0,
    UserMapped = 1,
    UserHeap = 2,
    UserStack = 3,
    UserMmio = 4,
}

impl TryFrom<usize> for ProcessVmSegment {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::UserImage),
            1 => Ok(Self::UserMapped),
            2 => Ok(Self::UserHeap),
            3 => Ok(Self::UserStack),
            4 => Ok(Self::UserMmio),
            _ => Err(()),
        }
    }
}

/// Stable lifecycle phases returned by `ProcessMethod::QueryPhase`.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessObjectPhase {
    Creating = 0,
    Live = 1,
    Terminating = 2,
}

impl TryFrom<usize> for ProcessObjectPhase {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Creating),
            1 => Ok(Self::Live),
            2 => Ok(Self::Terminating),
            _ => Err(()),
        }
    }
}

/// Slow-path argument block used by `ProcessMethod::InheritHandle`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessInheritHandleArgs {
    /// Caller-local slot holding the handle to delegate.
    pub source_slot: u32,
    /// Generic capability set requested for the derived target handle.
    pub capability_bits: u32,
    /// Object-defined interface capabilities for the derived target handle.
    pub interface_caps: u32,
    /// Fixed target slot inside the destination process, or
    /// [`INVALID_HANDLE_SLOT`] to auto-allocate.
    pub target_slot: u32,
}

/// Fast-path IPC port creation kinds.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortCreateKind {
    /// Create one point-to-point request port with exactly one logical
    /// receiving process.
    Unicast = 0,
    /// Create one fan-out event port with one sender and multiple receiving
    /// subscribers.
    Broadcast = 1,
    /// Create one multi-publisher, multi-subscriber event bus.
    Bus = 2,
}

impl TryFrom<usize> for PortCreateKind {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            x if x == Self::Unicast as usize => Ok(Self::Unicast),
            x if x == Self::Broadcast as usize => Ok(Self::Broadcast),
            x if x == Self::Bus as usize => Ok(Self::Bus),
            _ => Err(()),
        }
    }
}

/// Per-send mode flags for `Syscall::PortSend`.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortSendFlags(usize);

impl PortSendFlags {
    const VALID_BITS: usize = 1 << 0;

    /// No extra send behavior.
    pub const NONE: Self = Self(0);
    /// Return immediately after queueing and hand the caller one reply-port
    /// receive handle.
    pub const ASYNC_REPLY: Self = Self(1 << 0);

    /// Return the raw flag bits.
    pub const fn bits(self) -> usize {
        self.0
    }

    /// Return whether all bits in `other` are set.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Decode one raw flag word, rejecting unknown bits.
    pub const fn from_bits(bits: usize) -> Option<Self> {
        if bits & !Self::VALID_BITS == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }
}

impl BitOr for PortSendFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for PortSendFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Slow-path object-invoke methods supported by IPC port objects.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortMethod {
    /// Return the current port kind, lifecycle state, and statistics.
    QueryState = 0,
    /// Submit one message to the target port.
    Send = 1,
    /// Receive one message from the target port.
    Recv = 2,
    /// Bind the receiver process of one unicast port.
    BindReceiver = 3,
    /// Replace the receiver process of one unicast port.
    RebindReceiver = 4,
    /// Permanently close the port.
    Close = 5,
    /// Freeze the port so new traffic is rejected while state remains visible.
    Freeze = 6,
    /// Add one subscriber to a broadcast or bus port.
    Subscribe = 7,
    /// Remove one subscriber from a broadcast or bus port.
    Unsubscribe = 8,
}

impl TryFrom<usize> for PortMethod {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::QueryState),
            1 => Ok(Self::Send),
            2 => Ok(Self::Recv),
            3 => Ok(Self::BindReceiver),
            4 => Ok(Self::RebindReceiver),
            5 => Ok(Self::Close),
            6 => Ok(Self::Freeze),
            7 => Ok(Self::Subscribe),
            8 => Ok(Self::Unsubscribe),
            _ => Err(()),
        }
    }
}

/// Public port kinds returned by the IPC query-state ABI.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortKind {
    Unicast = 0,
    Broadcast = 1,
    Bus = 2,
    Reply = 3,
}

impl TryFrom<usize> for PortKind {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Unicast),
            1 => Ok(Self::Broadcast),
            2 => Ok(Self::Bus),
            3 => Ok(Self::Reply),
            _ => Err(()),
        }
    }
}

/// Public port lifecycle states returned by the IPC query-state ABI.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortState {
    Open = 0,
    Frozen = 1,
    Closed = 2,
}

impl TryFrom<usize> for PortState {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Open),
            1 => Ok(Self::Frozen),
            2 => Ok(Self::Closed),
            _ => Err(()),
        }
    }
}

/// Decoded IPC port state returned by `Syscall::PortQueryState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortQueryState {
    pub kind: PortKind,
    pub state: PortState,
    pub accepted: u64,
    pub failed: u64,
    pub peer_count: usize,
}

impl PortQueryState {
    /// Decode one successful `PortQueryState` syscall result.
    pub fn from_result(result: SyscallResult) -> Result<Self, ()> {
        if !result.is_ok() {
            return Err(());
        }

        Ok(Self {
            kind: PortKind::try_from(result.values[0])?,
            state: PortState::try_from(result.values[1])?,
            accepted: result.values[2] as u64,
            failed: result.values[3] as u64,
            peer_count: result.values[4],
        })
    }
}

/// Object-invoke methods supported by interrupt controller objects.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqControllerMethod {
    QueryInfo = 0,
    QueryTopology = 1,
    QueryFeatures = 2,
}

impl TryFrom<usize> for IrqControllerMethod {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::QueryInfo),
            1 => Ok(Self::QueryTopology),
            2 => Ok(Self::QueryFeatures),
            _ => Err(()),
        }
    }
}

/// Object-invoke methods supported by PCI host-bridge objects.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PciHostMethod {
    /// Return one summary over the published host bridge and discovered
    /// function set.
    QuerySummary = 0,
}

impl TryFrom<usize> for PciHostMethod {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::QuerySummary),
            _ => Err(()),
        }
    }
}

/// Object-invoke methods supported by PCI function resource objects.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PciFunctionMethod {
    /// Return one stable snapshot of this function's identifying fields.
    QueryInfo = 0,
    /// Return supported interrupt modes and per-mode vector limits.
    QueryInterruptModes = 1,
    /// Enable MSI for this function and return one IRQ session handle per
    /// allocated message.
    EnableMsi = 2,
    /// Enable MSI-X for this function and return one IRQ session handle per
    /// allocated table entry.
    EnableMsix = 3,
    /// Disable every message-interrupt mode currently programmed by the
    /// kernel.
    DisableInterrupts = 4,
    /// Return one compact summary over BAR layout and attributes.
    QueryBars = 5,
    /// Return one compact summary over capability availability.
    QueryCapabilities = 6,
}

impl TryFrom<usize> for PciFunctionMethod {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::QueryInfo),
            1 => Ok(Self::QueryInterruptModes),
            2 => Ok(Self::EnableMsi),
            3 => Ok(Self::EnableMsix),
            4 => Ok(Self::DisableInterrupts),
            5 => Ok(Self::QueryBars),
            6 => Ok(Self::QueryCapabilities),
            _ => Err(()),
        }
    }
}

/// Stable current-mode codes returned by PCI function interrupt queries.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PciInterruptModeKind {
    /// No message-interrupt mode is active; legacy INTx remains in effect.
    Intx = 0,
    /// MSI is currently active for this function.
    Msi = 1,
    /// MSI-X is currently active for this function.
    Msix = 2,
}

impl TryFrom<usize> for PciInterruptModeKind {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Intx),
            1 => Ok(Self::Msi),
            2 => Ok(Self::Msix),
            _ => Err(()),
        }
    }
}

/// Slow-path argument block used by PCI function interrupt-enable methods.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciInterruptBindArgs {
    /// Requested message or table-entry count.
    pub count: usize,
    /// Optional destination CPU hint. Use [`Self::NO_CPU_HINT`] to request the
    /// controller default.
    pub cpu_hint: usize,
    /// User buffer that receives `u32` handle slots for the created IRQ session
    /// objects.
    pub slots_ptr: usize,
    /// Number of `u32` entries available at `slots_ptr`.
    pub slots_len: usize,
}

impl PciInterruptBindArgs {
    /// Sentinel `cpu_hint` value used to request controller-default routing.
    pub const NO_CPU_HINT: usize = usize::MAX;
}

/// Object-invoke methods supported by IRQ line resource objects.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqLineMethod {
    QueryState = 0,
    OpenSession = 1,
    SetDestination = 3,
}

impl TryFrom<usize> for IrqLineMethod {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::QueryState),
            1 => Ok(Self::OpenSession),
            3 => Ok(Self::SetDestination),
            _ => Err(()),
        }
    }
}

/// Object-invoke methods supported by IRQ session objects.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqSessionMethod {
    QueryState = 0,
    SetEnabled = 1,
    Close = 2,
}

impl TryFrom<usize> for IrqSessionMethod {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::QueryState),
            1 => Ok(Self::SetEnabled),
            2 => Ok(Self::Close),
            _ => Err(()),
        }
    }
}

/// Shared IRQ session open flags.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrqOpenFlags(usize);

impl IrqOpenFlags {
    const VALID_BITS: usize = (1 << 0) | (1 << 1);

    /// Open one shared handler session.
    pub const SHARED: Self = Self(1 << 0);
    /// Open one monitor-only session.
    pub const MONITOR: Self = Self(1 << 1);

    /// Return the raw flag bits.
    pub const fn bits(self) -> usize {
        self.0
    }

    /// Return whether all bits in `other` are set.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Decode one raw flag word, rejecting unknown bits.
    pub const fn from_bits(bits: usize) -> Option<Self> {
        if bits & !Self::VALID_BITS == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }
}

impl BitOr for IrqOpenFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for IrqOpenFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Per-wait mode flags for `Syscall::IrqWait`.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrqWaitFlags(usize);

impl IrqWaitFlags {
    const VALID_BITS: usize = 1 << 0;

    /// Block until one delivery becomes visible.
    pub const NONE: Self = Self(0);
    /// Return immediately when no delivery is pending.
    pub const NONBLOCK: Self = Self(1 << 0);

    /// Return the raw flag bits.
    pub const fn bits(self) -> usize {
        self.0
    }

    /// Return whether all bits in `other` are set.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Decode one raw flag word, rejecting unknown bits.
    pub const fn from_bits(bits: usize) -> Option<Self> {
        if bits & !Self::VALID_BITS == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }
}

impl BitOr for IrqWaitFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for IrqWaitFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Shared IRQ ACK disposition returned by one session.
#[repr(usize)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqAckDisposition {
    Observed = 0,
    Claimed = 1,
}

impl TryFrom<usize> for IrqAckDisposition {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Observed),
            1 => Ok(Self::Claimed),
            _ => Err(()),
        }
    }
}

// Syscall-visible errno definitions now live under `crate::errno`.

/// Interrupt controller interface capabilities.
pub const IRQ_CTRL_QUERY: u32 = 1 << 0;
/// Interrupt controller topology-query capability.
pub const IRQ_CTRL_TOPOLOGY: u32 = 1 << 1;
/// Interrupt controller feature-query capability.
pub const IRQ_CTRL_FEATURES: u32 = 1 << 2;

/// IRQ line state-query capability.
pub const IRQ_LINE_QUERY: u32 = 1 << 0;
/// IRQ line session-open capability.
pub const IRQ_LINE_OPEN: u32 = 1 << 1;
/// IRQ line destination-routing capability.
pub const IRQ_LINE_ROUTE: u32 = 1 << 3;

/// IRQ session wait capability.
pub const IRQ_SESSION_WAIT: u32 = 1 << 0;
/// IRQ session acknowledge capability.
pub const IRQ_SESSION_ACK: u32 = 1 << 1;
/// IRQ session state-query capability.
pub const IRQ_SESSION_QUERY: u32 = 1 << 2;
/// IRQ session enable/disable capability.
pub const IRQ_SESSION_ENABLE: u32 = 1 << 3;
/// IRQ session bind capability reserved for future use.
pub const IRQ_SESSION_BIND: u32 = 1 << 4;

/// Process phase-query capability.
pub const PROCESS_QUERY: u32 = 1 << 0;
/// Creating-process delegated-handle install capability.
pub const PROCESS_INHERIT_HANDLE: u32 = 1 << 1;
/// Creating-process abort capability.
pub const PROCESS_ABORT: u32 = 1 << 2;
/// Creating-process fixed userspace segment delegation capability.
pub const PROCESS_DERIVE_SEGMENT_VMAR: u32 = 1 << 3;
/// Lifecycle wait capability for one supervisor process handle.
pub const PROCESS_WAIT: u32 = 1 << 4;

/// PCI host-bridge query capability.
pub const PCI_HOST_QUERY: u32 = 1 << 0;
/// PCI function info/mode query capability.
pub const PCI_FUNCTION_QUERY: u32 = 1 << 0;
/// PCI function interrupt programming capability.
pub const PCI_FUNCTION_INTERRUPT: u32 = 1 << 1;

/// PCI function supports legacy INTx.
pub const PCI_INTERRUPT_MODE_INTX: usize = 1 << 0;
/// PCI function supports MSI.
pub const PCI_INTERRUPT_MODE_MSI: usize = 1 << 1;
/// PCI function supports MSI-X.
pub const PCI_INTERRUPT_MODE_MSIX: usize = 1 << 2;
/// PCI function exposes a PCI capability list.
pub const PCI_CAPABILITY_LIST: usize = 1 << 0;
/// PCI function exposes an MSI capability.
pub const PCI_CAPABILITY_MSI: usize = 1 << 1;
/// PCI function exposes an MSI-X capability.
pub const PCI_CAPABILITY_MSIX: usize = 1 << 2;

/// Sentinel slot value used to mark an absent attached handle.
pub const INVALID_HANDLE_SLOT: u32 = u32::MAX;

/// User-space descriptor used by port send/recv operations.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PortUserMessage {
    pub txid: u64,
    pub proto_id: u32,
    pub opcode: u32,
    pub inline_words_ptr: usize,
    pub inline_words_len: usize,
    pub handle_slots_ptr: usize,
    pub handle_slots_len: usize,
    pub buffer_slot: u32,
    /// Optional reply-port slot carried by this message. `recv` fills it when
    /// the incoming message carries one. `send` may also supply an existing
    /// reply-port handle, for example when forwarding a request.
    pub reply_port_slot: u32,
}

impl PortUserMessage {
    /// Read one descriptor from user memory.
    pub fn read<C>(caller: &C, ptr: usize) -> Result<Self, C::UserError>
    where
        C: SyscallContext + ?Sized,
    {
        let mut value = MaybeUninit::<Self>::uninit();
        let bytes = unsafe {
            slice::from_raw_parts_mut(value.as_mut_ptr().cast::<u8>(), size_of::<Self>())
        };
        caller.copy_from_user(ptr, bytes)?;
        Ok(unsafe { value.assume_init() })
    }

    /// Write one descriptor back to user memory.
    pub fn write<C>(caller: &C, ptr: usize, value: &Self) -> Result<(), C::UserError>
    where
        C: SyscallContext + ?Sized,
    {
        let bytes = unsafe {
            slice::from_raw_parts((value as *const Self).cast::<u8>(), size_of::<Self>())
        };
        caller.copy_to_user(ptr, bytes)
    }
}
