use alloc::boxed::Box;

use async_trait::async_trait;

use crate::SyscallResult;

/// User-memory copy failure reported by one syscall context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserCopyError {
    /// The caller-provided user pointer or range could not be accessed.
    Fault,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpAccessMode {
    Read,
    Write,
    Execute,
    Agent,
    Admin,
}

impl CpAccessMode {
    pub fn verify_caps<CapabilityLike, F>(&self, caps: CapabilityLike, contains: F) -> bool
    where
        F: Fn(CapabilityLike, Self) -> bool,
        CapabilityLike: Copy,
    {
        contains(caps, *self)
    }
}

impl TryFrom<usize> for CpAccessMode {
    type Error = ();

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Read),
            1 => Ok(Self::Write),
            2 => Ok(Self::Execute),
            3 => Ok(Self::Agent),
            4 => Ok(Self::Admin),
            _ => Err(()),
        }
    }
}

pub trait SyscallContext: Send + Sync {
    type ObjectError;
    type UserError;
    type Handle;
    type Payload;
    type Capability;

    fn current_process(&self) -> Result<Self::Handle, Self::ObjectError>;

    fn install_handle(&self, handle: Self::Handle) -> Result<u32, Self::ObjectError>;

    fn create_anonymous_object(
        &self,
        payload: Self::Payload,
        capability: Self::Capability,
        interface_caps: u32,
    ) -> Result<u32, Self::ObjectError>;

    fn destroy_anonymous_object(&self, slot: u32) -> Result<(), Self::ObjectError>;

    fn close_handle(&self, slot: u32) -> Result<(), Self::ObjectError>;

    fn acquire_handle(&self, slot: u32) -> Result<Self::Handle, Self::ObjectError>;

    fn take_handle(&self, slot: u32) -> Result<Self::Handle, Self::ObjectError>;

    fn copy_from_user(&self, src: usize, out: &mut [u8]) -> Result<(), Self::UserError>;

    fn copy_to_user(&self, dst: usize, input: &[u8]) -> Result<(), Self::UserError>;
}

#[async_trait]
pub trait SyscallDispatch<C>: Send + Sync
where
    C: SyscallContext + ?Sized,
{
    #[allow(unused_variables)]
    async fn dispatch(
        &self,
        caller: &C,
        method_id: usize,
        arg1: usize,
        arg2: usize,
    ) -> Result<SyscallResult, C::ObjectError> {
        let _ = (caller, method_id, arg1, arg2);
        unreachable!("default syscall dispatch must be overridden")
    }
}

#[async_trait]
impl<T, C> SyscallDispatch<C> for &T
where
    T: SyscallDispatch<C> + ?Sized,
    C: SyscallContext + ?Sized,
{
    async fn dispatch(
        &self,
        caller: &C,
        method_id: usize,
        arg1: usize,
        arg2: usize,
    ) -> Result<SyscallResult, C::ObjectError> {
        (*self).dispatch(caller, method_id, arg1, arg2).await
    }
}

#[async_trait]
impl<T, C> SyscallDispatch<C> for &mut T
where
    T: SyscallDispatch<C> + ?Sized,
    C: SyscallContext + ?Sized,
{
    async fn dispatch(
        &self,
        caller: &C,
        method_id: usize,
        arg1: usize,
        arg2: usize,
    ) -> Result<SyscallResult, C::ObjectError> {
        (**self).dispatch(caller, method_id, arg1, arg2).await
    }
}
