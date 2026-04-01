use core::{clone::Clone, default::Default, marker::Sized, pin::Pin};

use libakarin_syscall::{SyscallArgs, SyscallResult};

use crate::memory::{VirtAddr, paging::MMUFlags};

/// General trap reasons for any architecture. This is a high-level abstraction
/// that can be used by the trap handlers to determine how to handle a trap
/// without needing to know the specific details of the trap context. The actual
/// trap context will provide the necessary information to determine the
/// specific reason for the trap (e.g., syscall number, interrupt vector, page
/// fault address, etc.).
#[derive(Debug, PartialEq, Eq)]
pub enum TrapReason {
    Syscall,
    Interrupt(usize),
    PageFault(VirtAddr, MMUFlags),
    UndefinedInstruction,
    SoftwareBreakpoint,
    HardwareBreakpoint,
    UnalignedAccess,
    GeneralFault(usize),
}

/// A unified trait representing the execution context (register state)
/// for both User and Kernel modes.
///
/// This structure holds general purpose registers, special registers (RIP, RSP,
/// RFLAGS), and extended states (SIMD/FPU) required by the x86_64 architecture.
pub trait TrapContextTrait: Default + Clone + Sized {
    /// The type representing the SIMD/FPU context for this trap context.
    type SimdContext: SimdContextTrait;

    /// Create a new user context
    fn new_user() -> Self;

    /// Create a new kernel context template.
    ///
    /// This constructor is intended for synthetic kernel continuations such as
    /// scheduler entry or first-run task entry. Architectures must populate the
    /// privilege selectors and flags required to resume execution in ring 0.
    fn new_kernel() -> Self;

    /// Return whether this trap frame currently points at one pinned SIMD/FPU
    /// save area.
    fn has_simd_context(&self) -> bool;

    /// Bind one pinned SIMD/FPU save area to this trap frame.
    ///
    /// Architectures store the pointer in trap-frame owned scratch space so
    /// later save/restore operations can avoid recovering arch-private types
    /// from unrelated scheduler state.
    fn bind_simd_context(&mut self, ctx: Pin<&mut Self::SimdContext>);

    /// Restore the bound SIMD/FPU state immediately before resuming execution.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that any bound SIMD pointer remains valid for
    /// the duration of the restore and subsequent context switch.
    unsafe fn restore_simd(&self);

    /// Get the instruction pointer (RIP).
    fn instruction_pointer(&self) -> usize;

    /// Set the instruction pointer (RIP).
    fn set_instruction_pointer(&mut self, addr: usize);

    /// Get the stack pointer (RSP).
    fn stack_pointer(&self) -> usize;

    /// Set the stack pointer (RSP).
    fn set_stack_pointer(&mut self, addr: usize);

    /// Set the interrupt flag (IF)
    fn set_interrupt_en(&mut self, enabled: bool);

    /// Get the TLS (Thread Local Storage) base address.
    /// On x86_64, this usually corresponds to the `fs_base` or `gs_base`
    /// register.
    fn tls_base(&self) -> usize;

    /// Set the TLS (Thread Local Storage) base address.
    fn set_tls_base(&mut self, addr: usize);

    /// Get the trap number (e.g., interrupt vector).
    fn trap_number(&self) -> usize;

    /// Get the error code associated with the trap (e.g., PageFault
    /// error code).
    fn error_code(&self) -> usize;

    /// Get the high-level reason for the trap.
    fn reason(&self) -> TrapReason;

    /// Get the raw six-word syscall frame.
    ///
    /// Word 0 carries the syscall identifier on entry, and becomes the success
    /// / failure flag on return. Words 1..=5 carry arguments on entry and may
    /// carry return payloads on exit.
    fn syscall_args(&self) -> SyscallArgs;

    /// Return the raw six-word frame currently stored in this trap context.
    ///
    /// Service-dispatch fast paths reuse the same architectural storage as the
    /// syscall ABI, but may reinterpret the six words according to one
    /// service-specific protocol after the kernel has trapped the original
    /// caller-side syscall frame.
    fn frame_words(&self) -> [usize; 6];

    /// Replace the raw six-word frame currently stored in this trap context.
    ///
    /// Architectures must preserve the physical layout used by the syscall
    /// ABI, while allowing higher layers to repurpose the words for in-kernel
    /// service dispatch before returning to user space.
    fn set_frame_words(&mut self, words: [usize; 6]);

    /// Replace the raw six-word syscall frame.
    fn set_syscall_ret(&mut self, ret: SyscallResult);

    /// Check if this is a user mode trap (e.g., by checking the CS register).
    fn is_user_mode(&self) -> bool;

    /// Check if this is a kernel mode trap.
    fn is_kernel_mode(&self) -> bool;

    /// Restore this context and resume execution.
    ///
    /// Depending on the context state (CS/Selector), this will return to
    /// either User Mode (via `iret` or `sysret`) or Kernel Mode (via `ret` or
    /// `iret`).
    ///
    /// This function should return if and only if this is a user mode trap. For
    /// kernel mode traps, this function should not return as it will resume
    /// execution in kernel mode. The caller is responsible for ensuring that
    /// the context is properly set up before calling this function.
    unsafe fn run(&mut self, from_trap_handler: bool);
}

/// A trait for representing the SIMD/FPU context for a thread.
///
/// This context includes the state of SIMD registers (XMM/YMM/ZMM) and FPU
/// state, which must be saved and restored during context switches to ensure
/// correct execution of SIMD instructions.
pub trait SimdContextTrait: Default + Sized {
    /// Check if this SIMD context has been initialized.
    fn is_some(&self) -> bool;

    /// Check if this SIMD context is uninitialized.
    fn is_none(&self) -> bool {
        !self.is_some()
    }

    /// Save the current SIMD/FPU state into this context.
    unsafe fn save(&mut self);

    /// Restore the SIMD/FPU state from this context.
    unsafe fn restore(&self);
}
