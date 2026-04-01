use crate::context::TrapContextTrait;

/// A trait for representing a stack frame in a backtrace.
///
/// This trait should be implemented for the architecture-specific
/// stack frame structure, providing a method to retrieve the next
/// stack frame in the call stack.
pub trait StackFrameTrait<T>: Default + Sized
where
    T: TrapContextTrait,
{
    fn from_ctx(ctx: &T) -> Self;
    fn next(&mut self) -> Option<*const usize>;
}
