#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

extern crate alloc;

use alloc::{boxed::Box, vec::Vec};
use core::{
    alloc::Layout,
    arch::{asm, naked_asm},
    sync::atomic::AtomicU32,
};

use libakarin_syscall::{FutexWakeFlags, Syscall, SyscallStatus};
use userland_runtime::{
    Duration, Futex, FutexCallError, FutexError, FutexMutex, GLOBAL_ALLOCATOR, RUNTIME,
    RawSyscallInvoker, RegionPurpose, StartupInfo, VmFlags, VmoChildMode,
};

struct BootstrapVmSelfTest<'a> {
    invoker: &'a RawSyscallInvoker,
    startup: &'a StartupInfo,
}

impl<'a> BootstrapVmSelfTest<'a> {
    fn run_allocator(&self) -> Result<(), usize> {
        let mut bytes = Vec::with_capacity(8192);
        bytes.extend_from_slice(b"akarin-bootstrap-runtime");
        bytes.resize(16384, 0x5A);
        bytes.reserve_exact(32768);
        if bytes.capacity() < 49152 {
            return Err(0x11);
        }

        let mut checksum = 0usize;
        for value in &bytes {
            checksum = checksum.wrapping_add(*value as usize);
        }
        if checksum != 1_474_882usize {
            return Err(0x12);
        }

        let marker = Box::new(0xA5A5_5A5A_F0F0_0F0Fu64);
        if *marker != 0xA5A5_5A5A_F0F0_0F0Fu64 {
            return Err(0x13);
        }
        drop(marker);

        let mut words = Vec::with_capacity(4);
        for value in 0..2048u64 {
            words.push(value ^ 0x55AA_33CC_0F0F_F0F0);
        }
        words.reserve_exact(4096);
        let folded = words.iter().fold(0u64, |acc, value| acc ^ *value);
        if folded != 0 {
            return Err(0x14);
        }
        drop(words);

        let mut trim_candidate = Vec::with_capacity(self.startup.page_size * 32);
        trim_candidate.resize(self.startup.page_size * 24, 0xA7);
        trim_candidate.shrink_to_fit();
        drop(trim_candidate);
        let _ = GLOBAL_ALLOCATOR.trim(0);

        let recycled = Box::new([0xC3u8; 128]);
        if recycled[0] != 0xC3 || recycled[127] != 0xC3 {
            return Err(0x15);
        }

        Ok(())
    }

    fn run_private_cow(&self) -> Result<(), usize> {
        let page_size = self.startup.page_size;
        let parent_slot = self
            .invoker
            .create_paged_vmo(
                page_size,
                page_size,
                VmFlags::READ | VmFlags::WRITE | VmFlags::USER | VmFlags::MAP,
            )
            .map_err(|_| 0x20usize)?;
        let child_slot = self
            .invoker
            .create_child_vmo(parent_slot, 0, page_size, VmoChildMode::PrivateCow)
            .map_err(|_| 0x21usize)?;
        let (vmar_slot, base, span) = self
            .invoker
            .allocate_child_vmar_any(self.startup.mapped_vmar_slot, page_size * 2)
            .map_err(|_| 0x22usize)?;
        if span < page_size * 2 {
            return Err(0x23);
        }

        let map_flags = VmFlags::READ | VmFlags::WRITE | VmFlags::USER;
        let (parent_base, parent_len) = self
            .invoker
            .map_vmo(
                vmar_slot,
                parent_slot,
                base,
                page_size,
                0,
                map_flags,
                RegionPurpose::User,
            )
            .map_err(|_| 0x24usize)?;
        if parent_base != base || parent_len != page_size {
            return Err(0x25);
        }

        let (child_base, child_len) = self
            .invoker
            .map_vmo(
                vmar_slot,
                child_slot,
                base + page_size,
                page_size,
                0,
                map_flags,
                RegionPurpose::User,
            )
            .map_err(|_| 0x26usize)?;
        if child_base != base + page_size || child_len != page_size {
            return Err(0x27);
        }

        unsafe {
            let parent = core::slice::from_raw_parts_mut(parent_base as *mut u8, 6);
            parent.copy_from_slice(b"parent");
            let child_before = core::slice::from_raw_parts(child_base as *const u8, 6);
            if child_before != b"parent" {
                return Err(0x28);
            }

            let child = core::slice::from_raw_parts_mut(child_base as *mut u8, 6);
            child.copy_from_slice(b"child!");
            let parent_after = core::slice::from_raw_parts(parent_base as *const u8, 6);
            if parent_after != b"parent" {
                return Err(0x29);
            }
            let child_after = core::slice::from_raw_parts(child_base as *const u8, 6);
            if child_after != b"child!" {
                return Err(0x2A);
            }
        }

        Ok(())
    }

    fn run_futex_smoke(&self) -> Result<(), usize> {
        let word = AtomicU32::new(1);
        match Futex::wait(&word, 0, None) {
            Err(FutexCallError::Underlying(FutexError::WouldBlock)) => {}
            _ => return Err(0x30),
        }

        match Futex::wait(&word, 1, Some(Duration::from_nanos(0))) {
            Err(FutexCallError::Underlying(FutexError::TimedOut)) => {}
            _ => return Err(0x31),
        }

        match Futex::wake(&word, 1) {
            Ok(0) => Ok(()),
            _ => Err(0x32),
        }
    }

    fn run_futex_mutex_smoke(&self) -> Result<(), usize> {
        let mutex = FutexMutex::new(0x41u32);
        {
            let mut guard = mutex.lock().map_err(|_| 0x33usize)?;
            *guard += 1;
        }

        let guard = mutex.lock().map_err(|_| 0x34usize)?;
        if *guard != 0x42 {
            return Err(0x35);
        }
        drop(guard);

        Ok(())
    }

    fn run_simd_smoke(&self) -> Result<(), usize> {
        unsafe { sse_syscall_roundtrip()? };
        unsafe { avx_syscall_roundtrip()? };
        Ok(())
    }
}

#[target_feature(enable = "sse2")]
unsafe fn sse_syscall_roundtrip() -> Result<(), usize> {
    let futex_word = AtomicU32::new(0);
    let input = [0x11u32, 0x22, 0x33, 0x44];
    let mut output = [0u32; 4];
    let mut status = Syscall::FutexWake as usize;
    let mut value0 = (&futex_word as *const AtomicU32).cast::<u32>() as usize;
    unsafe {
        asm!(
            "movdqu xmm0, [{input}]",
            "syscall",
            "movdqu [{output}], xmm0",
            input = in(reg) input.as_ptr(),
            output = in(reg) output.as_mut_ptr(),
            inlateout("rax") status,
            inlateout("rdi") value0,
            inlateout("rsi") 1usize => _,
            inlateout("rdx") FutexWakeFlags::NONE.bits() => _,
            inlateout("r10") 0usize => _,
            inlateout("r8") 0usize => _,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }

    if status != SyscallStatus::Ok as usize {
        return Err(0x36);
    }
    if value0 != 0 {
        return Err(0x37);
    }
    if output != input {
        return Err(0x38);
    }
    Ok(())
}

#[target_feature(enable = "avx")]
unsafe fn avx_syscall_roundtrip() -> Result<(), usize> {
    let futex_word = AtomicU32::new(0);
    let input = [0x51u32, 0x62, 0x73, 0x84, 0x95, 0xA6, 0xB7, 0xC8];
    let mut output = [0u32; 8];
    let mut status = Syscall::FutexWake as usize;
    let mut value0 = (&futex_word as *const AtomicU32).cast::<u32>() as usize;
    unsafe {
        asm!(
            "vmovdqu ymm0, [{input}]",
            "syscall",
            "vmovdqu [{output}], ymm0",
            input = in(reg) input.as_ptr(),
            output = in(reg) output.as_mut_ptr(),
            inlateout("rax") status,
            inlateout("rdi") value0,
            inlateout("rsi") 1usize => _,
            inlateout("rdx") FutexWakeFlags::NONE.bits() => _,
            inlateout("r10") 0usize => _,
            inlateout("r8") 0usize => _,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }

    if status != SyscallStatus::Ok as usize {
        return Err(0x39);
    }
    if value0 != 0 {
        return Err(0x3A);
    }
    if output != input {
        return Err(0x3B);
    }
    Ok(())
}

#[unsafe(no_mangle)]
#[unsafe(naked)]
pub unsafe extern "C" fn main() -> ! {
    naked_asm!(
        "mov rdi, rsp",
        "and rsp, -16",
        "call {}",
        "ud2",
        sym bootstrap_entry,
    );
}

extern "C" fn bootstrap_entry(initial_stack_pointer: usize) -> ! {
    let invoker = RawSyscallInvoker;
    let startup = match unsafe { RUNTIME.initialize(initial_stack_pointer) } {
        Ok(startup) => startup,
        Err(_) => invoker.exit_process(0x10),
    };

    let mut bytes = Vec::with_capacity(8192);
    bytes.extend_from_slice(b"akarin-bootstrap-runtime");
    bytes.resize(16384, 0x5A);
    let marker = Box::new(0xA5A5_5A5A_F0F0_0F0Fu64);
    let mut checksum = 0usize;
    for value in &bytes {
        checksum = checksum.wrapping_add(*value as usize);
    }
    if checksum != 1_474_882usize || *marker != 0xA5A5_5A5A_F0F0_0F0Fu64 {
        invoker.exit_process(0x11);
    }

    let vm_test = BootstrapVmSelfTest {
        invoker: &invoker,
        startup,
    };
    if let Err(code) = vm_test.run_allocator() {
        invoker.exit_process(code);
    }
    if let Err(code) = vm_test.run_private_cow() {
        invoker.exit_process(code);
    }
    if let Err(code) = vm_test.run_futex_smoke() {
        invoker.exit_process(code);
    }
    if let Err(code) = vm_test.run_futex_mutex_smoke() {
        invoker.exit_process(code);
    }
    if let Err(code) = vm_test.run_simd_smoke() {
        invoker.exit_process(code);
    }

    invoker.exit_process(0);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    let invoker = RawSyscallInvoker;
    invoker.exit_process(0xFE);
}

#[alloc_error_handler]
fn alloc_error(_layout: Layout) -> ! {
    let invoker = RawSyscallInvoker;
    invoker.exit_process(0xFD);
}
