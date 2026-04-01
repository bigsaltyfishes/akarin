mod vmar;
mod vmflags;
mod vmo;
mod vspace;

use libakarin_object::ObjectError;
use libakarin_syscall::{SyscallFailure, SyscallResult, VmError};

const SYSCALL_STATUS_OK: usize = 0;
type VmInvokeError = VmError;

/// One mixed control-plane error returned by the VM subsystem.
///
/// VM guards use this instead of folding subsystem failures back into
/// `ObjectError`, allowing fast syscall paths to preserve `VmError`
/// semantics all the way to the ABI boundary.
#[derive(Debug)]
pub enum VmControlError {
    Object(ObjectError),
    Vm(VmError),
}

impl VmControlError {
    /// Split the mixed control-plane error into object and VM error classes.
    pub const fn into_object_or_vm(self) -> Result<ObjectError, VmError> {
        match self {
            Self::Object(error) => Ok(error),
            Self::Vm(error) => Err(error),
        }
    }
}

impl From<ObjectError> for VmControlError {
    fn from(value: ObjectError) -> Self {
        Self::Object(value)
    }
}

impl From<VmError> for VmControlError {
    fn from(value: VmError) -> Self {
        Self::Vm(value)
    }
}

struct VmInvokeFrame;

impl VmInvokeFrame {
    fn ok(values: [usize; 5]) -> SyscallResult {
        [
            SYSCALL_STATUS_OK,
            values[0],
            values[1],
            values[2],
            values[3],
            values[4],
        ]
        .into()
    }

    fn empty_ok() -> SyscallResult {
        Self::ok([0, 0, 0, 0, 0])
    }

    fn vm_error(error: VmInvokeError) -> SyscallResult {
        SyscallResult::from(SyscallFailure::from(error))
    }
}

pub use vmar::{
    GuardedStackLayout, PAGE_SIZE, RegionPurpose, VM_ALLOCATE, VM_DESTROY, VM_MAP, VM_PROTECT,
    VM_QUERY, VM_UNMAP, VMAR_ADMIN_INTERFACE_CAPS, VMAR_DEFAULT_INTERFACE_CAPS, VmLayoutSegment,
    VmPointerRegion, VmRange, Vmar, VmarEntry, VmarError, VmarMapping, VmarMethod,
};
pub use vmflags::VmFlags;
pub use vmo::{
    V_CREATE_CHILD, V_GET_SIZE, V_READ_DATA, V_SET_POLICY, V_SET_SIZE, V_TRANSFER, V_WRITE_DATA,
    VMO_ADMIN_INTERFACE_CAPS, VMO_DEFAULT_INTERFACE_CAPS, VmFaultResolution, Vmo, VmoBacking,
    VmoChildInfo, VmoChildKind, VmoFaultPolicy, VmoMethod, VmoPageMetadata, VmoPagePurpose,
    VmoRangeOpError,
};
pub use vspace::{VSpace, VSpaceError, VSpaceInfo};
