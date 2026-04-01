use alloc::boxed::Box;
use core::{
    arch::{asm, global_asm},
    fmt::Debug,
    pin::Pin,
};

use aligned_vec::{AVec, ConstAlign};
use bitflags::bitflags;
use libakarin_machine_core::{
    self,
    context::{self, TrapReason},
    memory::{PageTableTrait, VirtAddr, paging::MMUFlags},
    sync::{NoOp, ScopedGuard},
};
use libakarin_sync::spin::Once;
use libakarin_syscall::{SyscallArgs, SyscallResult};
use x86_64::instructions::interrupts;


global_asm!(include_str!("trap.S"));
global_asm!(include_str!("syscall.S"));

const SYSCALL_IRQ: usize = 256;
const SIMD_ALIGN: usize = 64;

#[derive(Debug, Clone, Copy)]
struct SimdRuntime {
    xsave_area_size: usize,
    xsave_mask: u64,
}

static SIMD_RUNTIME: Once<SimdRuntime, ScopedGuard<NoOp>> = Once::new();

pub(super) fn install_simd_runtime(xsave_area_size: usize, xsave_mask: u64) {
    let runtime = SimdRuntime {
        xsave_area_size,
        xsave_mask,
    };
    SIMD_RUNTIME.get_or_else(|| runtime);
}

fn simd_runtime() -> &'static SimdRuntime {
    SIMD_RUNTIME.get()
}

unsafe extern "C" {
    /// Run the given trap context.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the provided `TrapContext` points to one
    /// valid, properly initialized trap frame for the target continuation.
    ///
    /// # Returns
    ///
    /// This function will return if and only if this is a user context, any
    /// trap context from kernel should never return.
    #[allow(improper_ctypes)]
    pub unsafe fn __run_context(ctx: &mut TrapContext, from_trap_handler: bool);
}

/// Trap context for x86_64 architecture.
///
/// This structure represents the CPU state at the moment a trap (interrupt,
/// exception, or syscall) occurs. It includes general-purpose registers and
/// the architectural return frame pushed by hardware. SIMD state is not stored
/// inline; instead, the first two words carry one pointer-sized hook to an
/// optional pinned XSAVE area owned by the task.
#[repr(C, align(16))]
#[derive(Debug, Default, Clone)]
pub struct TrapContext {
    /// Pointer to one pinned SIMD/FPU save area owned by the task.
    simd_ctx_ptr: usize,
    /// Reserved scratch word paired with `simd_ctx_ptr` to preserve the
    /// assembly frame layout.
    simd_reserved: usize,

    /// General-purpose register `RAX`.
    rax: usize,
    /// General-purpose register `RBX`.
    rbx: usize,
    /// General-purpose register `RCX`.
    rcx: usize,
    /// General-purpose register `RDX`.
    rdx: usize,
    /// General-purpose register `RDI`.
    rdi: usize,
    /// General-purpose register `RSI`.
    rsi: usize,
    /// General-purpose register `RBP`.
    pub(super) rbp: usize,

    /// General-purpose register `R8`.
    r8: usize,
    /// General-purpose register `R9`.
    r9: usize,
    /// General-purpose register `R10`.
    r10: usize,
    /// General-purpose register `R11`.
    r11: usize,
    /// General-purpose register `R12`.
    r12: usize,
    /// General-purpose register `R13`.
    r13: usize,
    /// General-purpose register `R14`.
    r14: usize,
    /// General-purpose register `R15`.
    r15: usize,

    /// User TLS base restored into `FSBASE` on resume.
    fs: usize,

    /// Kernel GS base captured at trap entry.
    gs: usize,

    /// Trap number pushed by the entry stub.
    trap_number: usize,
    /// Trap error code pushed by hardware or synthesized by the entry stub.
    error_code: usize,

    /// Saved instruction pointer.
    rip: usize,
    /// Saved code segment selector.
    cs: usize,
    /// Saved `RFLAGS`.
    rflags: usize,
    /// Saved stack pointer.
    rsp: usize,
    /// Saved stack segment selector.
    ss: usize,
}

impl context::TrapContextTrait for TrapContext {
    type SimdContext = SimdContext;

    fn new_user() -> Self {
        let mut ctx = Self::default();
        ctx.cs = 0x1b; // User code segment selector
        ctx.ss = 0x23; // User data segment selector
        ctx.rflags = 0x202; // Interrupt Enable flag
        ctx
    }

    fn new_kernel() -> Self {
        let mut ctx = Self::default();
        ctx.cs = 0x8; // Kernel code segment selector
        ctx.ss = 0x10; // Kernel data segment selector
        ctx.rflags = 0x202;
        ctx
    }

    fn has_simd_context(&self) -> bool {
        self.simd_ctx_ptr != 0
    }

    fn bind_simd_context(&mut self, ctx: Pin<&mut Self::SimdContext>) {
        let ptr = unsafe { ctx.get_unchecked_mut() as *mut Self::SimdContext };
        self.simd_ctx_ptr = ptr as usize;
        self.simd_reserved = 0;
    }

    unsafe fn restore_simd(&self) {
        if !self.has_simd_context() {
            return;
        }

        let ctx_ptr = self.simd_ctx_ptr as *const Self::SimdContext;
        unsafe {
            context::SimdContextTrait::restore(&*ctx_ptr);
        }
    }

    fn instruction_pointer(&self) -> usize {
        self.rip
    }

    fn set_instruction_pointer(&mut self, addr: usize) {
        self.rip = addr;
    }

    fn stack_pointer(&self) -> usize {
        self.rsp
    }

    fn set_interrupt_en(&mut self, enabled: bool) {
        if enabled {
            self.rflags |= 0x200; // Set IF flag
        } else {
            self.rflags &= !0x200; // Clear IF flag
        }
    }

    fn set_stack_pointer(&mut self, addr: usize) {
        self.rsp = addr;
    }

    fn tls_base(&self) -> usize {
        self.fs
    }

    fn set_tls_base(&mut self, addr: usize) {
        self.fs = addr;
    }

    fn trap_number(&self) -> usize {
        self.trap_number
    }

    fn error_code(&self) -> usize {
        self.error_code
    }

    fn reason(&self) -> TrapReason {
        use x86::irq::*;
        const X86_INT_BASE: u8 = 0x20;
        const X86_INT_MAX: u8 = 0xff;

        if self.trap_number == SYSCALL_IRQ {
            return TrapReason::Syscall;
        }

        match self.trap_number as u8 {
            DEBUG_VECTOR => TrapReason::HardwareBreakpoint,
            BREAKPOINT_VECTOR => TrapReason::SoftwareBreakpoint,
            INVALID_OPCODE_VECTOR => TrapReason::UndefinedInstruction,
            ALIGNMENT_CHECK_VECTOR => TrapReason::UnalignedAccess,
            PAGE_FAULT_VECTOR => {
                bitflags! {
                    struct PageFaultErrorCode: u32 {
                        const PRESENT =     1 << 0;
                        const WRITE =       1 << 1;
                        const USER =        1 << 2;
                        const RESERVED =    1 << 3;
                        const INST =        1 << 4;
                    }
                }
                let fault_vaddr = VirtAddr::new(
                    x86_64::registers::control::Cr2::read().unwrap().as_u64() as usize,
                );
                let code = PageFaultErrorCode::from_bits_truncate(self.error_code as u32);
                let mut flags = MMUFlags::empty();
                if code.contains(PageFaultErrorCode::WRITE) {
                    flags |= MMUFlags::WRITE
                } else {
                    flags |= MMUFlags::READ
                }
                if code.contains(PageFaultErrorCode::USER) {
                    flags |= MMUFlags::USER
                }
                if code.contains(PageFaultErrorCode::INST) {
                    flags |= MMUFlags::EXECUTE
                }
                if code.contains(PageFaultErrorCode::RESERVED) {
                    panic!("page table entry has reserved bits set!");
                }
                TrapReason::PageFault(fault_vaddr, flags)
            }
            vec @ X86_INT_BASE..=X86_INT_MAX => TrapReason::Interrupt(vec as usize),
            _ => TrapReason::GeneralFault(self.trap_number),
        }
    }

    fn syscall_args(&self) -> SyscallArgs {
        SyscallArgs::new(self.rax, [self.rdi, self.rsi, self.rdx, self.r10, self.r8])
    }

    fn frame_words(&self) -> [usize; 6] {
        [self.rax, self.rdi, self.rsi, self.rdx, self.r10, self.r8]
    }

    fn set_frame_words(&mut self, words: [usize; 6]) {
        self.rax = words[0];
        self.rdi = words[1];
        self.rsi = words[2];
        self.rdx = words[3];
        self.r10 = words[4];
        self.r8 = words[5];
    }

    fn set_syscall_ret(&mut self, ret: SyscallResult) {
        self.rax = ret.status;
        self.rdi = ret.values[0];
        self.rsi = ret.values[1];
        self.rdx = ret.values[2];
        self.r10 = ret.values[3];
        self.r8 = ret.values[4];
    }

    fn is_user_mode(&self) -> bool {
        self.cs == 0x1b // User code segment selector
    }

    fn is_kernel_mode(&self) -> bool {
        self.cs == 0x8 // Kernel code segment selector
    }

    #[inline(never)]
    unsafe fn run(&mut self, from_trap_handler: bool) {
        assert!(self.cs != 0); // CS should never be 0, as it would indicate an invalid context
        assert!(self.ss != 0); // SS should never be 0, as it would indicate an invalid context
        interrupts::disable();
        unsafe {
            self.restore_simd();
        }
        unsafe { __run_context(self, from_trap_handler) };
    }
}

/// SIMD/FPU context for x86_64 architecture.
///
/// This structure holds the state of the SIMD/FPU registers that need to be
/// saved and restored during context switches. The exact contents depend on the
/// x86_64 architecture and the features enabled (e.g., AVX, AVX-512). For
/// one aligned XSAVE area used to preserve x87, SSE, and AVX state for a task.
pub struct SimdContext {
    area: AVec<u8, ConstAlign<SIMD_ALIGN>>,
}

impl Default for SimdContext {
    fn default() -> Self {
        Self {
            area: AVec::new(SIMD_ALIGN),
        }
    }
}

impl SimdContext {
    /// Allocate one aligned XSAVE area sized for the enabled architectural
    /// SIMD feature set.
    pub fn new() -> Self {
        let runtime = simd_runtime();
        let mut area = AVec::with_capacity(SIMD_ALIGN, runtime.xsave_area_size);
        area.resize(runtime.xsave_area_size, 0);
        Self { area }
    }

    fn area_ptr(&self) -> *const u8 {
        self.area.as_ptr()
    }

    fn area_mut_ptr(&mut self) -> *mut u8 {
        self.area.as_mut_ptr()
    }

    fn xsave_mask(&self) -> u64 {
        simd_runtime().xsave_mask
    }
}

impl context::SimdContextTrait for SimdContext {
    fn is_some(&self) -> bool {
        !self.area.is_empty()
    }

    unsafe fn save(&mut self) {
        if !self.is_some() {
            return;
        }

        let mask = self.xsave_mask();
        let eax = mask as u32;
        let edx = (mask >> 32) as u32;
        let ptr = self.area_mut_ptr();
        unsafe {
            asm!(
                "xsaveopt [{ptr}]",
                ptr = in(reg) ptr,
                in("eax") eax,
                in("edx") edx,
                options(nostack)
            );
        }
    }

    unsafe fn restore(&self) {
        if !self.is_some() {
            return;
        }

        let mask = self.xsave_mask();
        let eax = mask as u32;
        let edx = (mask >> 32) as u32;
        let ptr = self.area_ptr();
        unsafe {
            asm!(
                "xrstor [{ptr}]",
                ptr = in(reg) ptr,
                in("eax") eax,
                in("edx") edx,
                options(nostack)
            );
        }
    }
}

impl Debug for SimdContext {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("SimdContext")
            .field(&context::SimdContextTrait::is_some(self))
            .finish()
    }
}

impl TrapContext {
    /// Save the live SIMD state into one pinned task-owned XSAVE area and bind
    /// that area to this trap frame for later restores.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the supplied save area remains pinned for
    /// the duration of any future use of this trap frame.
    pub unsafe fn save_simd(&mut self, mut ctx: Pin<Box<SimdContext>>) -> Pin<Box<SimdContext>> {
        context::TrapContextTrait::bind_simd_context(self, ctx.as_mut());
        unsafe {
            context::SimdContextTrait::save(ctx.as_mut().get_unchecked_mut());
        }
        ctx
    }
}
