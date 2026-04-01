use core::{arch::asm, convert::Infallible};

use libakarin_core::memory::{RegionPurpose, VmFlags};
use libakarin_syscall::{
    FutexWaitFlags, FutexWakeFlags, Syscall, SyscallArgs, SyscallInvoker, SyscallResult,
    SyscallStatus, VmarMapArgs, VmoChildMode, VmoOpRangeOperation, user::InvokeError,
};

/// Raw fast-syscall transport used by userspace runtime code.
pub struct RawSyscallInvoker;

/// One failed kernel syscall observed by the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyscallFailure {
    result: SyscallResult,
}

impl SyscallFailure {
    /// Return the raw kernel result frame that caused this failure.
    pub fn result(self) -> SyscallResult {
        self.result
    }

    /// Return the coarse syscall-status kind reported by the kernel.
    pub fn status(self) -> SyscallStatus {
        SyscallStatus::try_from(self.result.status).unwrap_or(SyscallStatus::UnderlyingError)
    }

    /// Return the subsystem- or object-defined detail code from `value0`.
    pub fn detail(self) -> usize {
        self.result.values[0]
    }
}

impl RawSyscallInvoker {
    /// Submit one raw syscall frame and convert kernel failures into
    /// one stable runtime error value.
    pub fn invoke_checked(&self, args: SyscallArgs) -> Result<SyscallResult, SyscallFailure> {
        let result = match self.invoke(args) {
            Ok(result) => result,
            Err(never) => match never {},
        };
        if result.is_ok() {
            return Ok(result);
        }
        Err(SyscallFailure { result })
    }

    /// Close one caller-local handle slot.
    pub fn close_handle(&self, slot: u32) -> Result<(), SyscallFailure> {
        let args = SyscallArgs::from_syscall(Syscall::HandleClose, [slot as usize, 0, 0, 0, 0]);
        self.invoke_checked(args)?;
        Ok(())
    }

    /// Create one paged VMO and return the caller-local handle slot chosen by
    /// the kernel for the new object.
    pub fn create_paged_vmo(
        &self,
        size: usize,
        page_size: usize,
        flags: VmFlags,
    ) -> Result<u32, SyscallFailure> {
        let request = SyscallArgs::from_syscall(
            Syscall::VmoCreate,
            [size, page_size, flags.bits() as usize, 0, 0],
        );
        let response = self.invoke_checked(request)?;
        let slot = response.values[0] as u32;
        Ok(slot)
    }

    /// Create one child VMO representing a derived window into the parent.
    pub fn create_child_vmo(
        &self,
        parent_slot: u32,
        parent_offset: usize,
        size: usize,
        mode: VmoChildMode,
    ) -> Result<u32, SyscallFailure> {
        let request = SyscallArgs::vmo_create_child(parent_slot, parent_offset, size, mode);
        let response = self.invoke_checked(request)?;
        let slot = response.values[0] as u32;
        Ok(slot)
    }

    /// Wait until one futex word changes state or one timeout expires.
    pub fn futex_wait(
        &self,
        user_addr: usize,
        expected: u32,
        timeout_ns: usize,
        flags: FutexWaitFlags,
    ) -> Result<(), SyscallFailure> {
        let request = SyscallArgs::futex_wait(user_addr, expected, timeout_ns, flags);
        self.invoke_checked(request)?;
        Ok(())
    }

    /// Wake up to `wake_count` waiters sleeping on one futex word.
    pub fn futex_wake(
        &self,
        user_addr: usize,
        wake_count: usize,
        flags: FutexWakeFlags,
    ) -> Result<usize, SyscallFailure> {
        let request = SyscallArgs::futex_wake(user_addr, wake_count, flags);
        let response = self.invoke_checked(request)?;
        Ok(response.values[0])
    }

    /// Apply one page-granular range operation to the selected VMO and return
    /// the kernel-reported byte count.
    pub fn vmo_op_range(
        &self,
        slot: u32,
        operation: VmoOpRangeOperation,
        offset: usize,
        len: usize,
    ) -> Result<usize, SyscallFailure> {
        let request = SyscallArgs::from_syscall(
            Syscall::VmoOpRange,
            [slot as usize, operation as usize, offset, len, 0],
        );
        let response = self.invoke_checked(request)?;
        Ok(response.values[0])
    }

    /// Allocate one child VMAR from the supplied parent and return the chosen
    /// handle slot together with the resolved address window.
    pub fn allocate_child_vmar_any(
        &self,
        parent_slot: u32,
        size: usize,
    ) -> Result<(u32, usize, usize), SyscallFailure> {
        let request =
            SyscallArgs::from_syscall(Syscall::VmarAllocate, [parent_slot as usize, 0, size, 0, 0]);
        let response = self.invoke_checked(request)?;
        let slot = response.values[0] as u32;
        let base = response.values[1];
        let len = response.values[2];
        Ok((slot, base, len))
    }

    /// Destroy one child VMAR and tear down all mappings underneath it.
    pub fn destroy_vmar(&self, slot: u32) -> Result<(), SyscallFailure> {
        let request = SyscallArgs::from_syscall(Syscall::VmarDestroy, [slot as usize, 0, 0, 0, 0]);
        self.invoke_checked(request)?;
        Ok(())
    }

    /// Remove one immediate child mapping identified by its start address.
    pub fn unmap_vmar(&self, slot: u32, start: usize) -> Result<usize, SyscallFailure> {
        let request =
            SyscallArgs::from_syscall(Syscall::VmarUnmap, [slot as usize, start, 0, 0, 0]);
        let response = self.invoke_checked(request)?;
        Ok(response.values[0])
    }

    /// Map one VMO range into the selected VMAR and return the resolved
    /// address window reported by the kernel.
    pub fn map_vmo(
        &self,
        vmar_slot: u32,
        vmo_slot: u32,
        base: usize,
        size: usize,
        vmo_offset: usize,
        flags: VmFlags,
        purpose: RegionPurpose,
    ) -> Result<(usize, usize), SyscallFailure> {
        let map_args = VmarMapArgs {
            base,
            size,
            vmo_offset,
            flags: flags.bits(),
            purpose: purpose as usize,
        };
        let request = SyscallArgs::from_syscall(
            Syscall::VmarMap,
            [
                vmar_slot as usize,
                vmo_slot as usize,
                &map_args as *const VmarMapArgs as usize,
                0,
                0,
            ],
        );
        let response = self.invoke_checked(request)?;
        let mapped_base = response.values[0];
        let mapped_len = response.values[1];
        Ok((mapped_base, mapped_len))
    }

    /// Terminate the current process. This function does not return.
    pub fn exit_process(&self, code: usize) -> ! {
        let args = SyscallArgs::from_syscall(Syscall::ProcessExit, [code, 0, 0, 0, 0]);
        let _ = self.invoke_checked(args);
        loop {
            core::hint::spin_loop();
        }
    }

    /// Terminate the current task. This function does not return.
    pub fn exit_task(&self, code: usize) -> ! {
        let args = SyscallArgs::from_syscall(Syscall::TaskExit, [code, 0, 0, 0, 0]);
        let _ = self.invoke_checked(args);
        loop {
            core::hint::spin_loop();
        }
    }
}

impl SyscallInvoker for RawSyscallInvoker {
    type Error = Infallible;

    fn invoke(&self, args: SyscallArgs) -> Result<SyscallResult, Self::Error> {
        let words = args.to_words();
        let mut result_words = [0usize; 6];
        unsafe {
            asm!(
                "syscall",
                inlateout("rax") words[0] => result_words[0],
                inlateout("rdi") words[1] => result_words[1],
                inlateout("rsi") words[2] => result_words[2],
                inlateout("rdx") words[3] => result_words[3],
                inlateout("r10") words[4] => result_words[4],
                inlateout("r8") words[5] => result_words[5],
                lateout("rcx") _,
                lateout("r11") _,
                options(nostack),
            );
        }
        Ok(SyscallResult::from(result_words))
    }
}

impl From<InvokeError<Infallible>> for SyscallFailure {
    fn from(value: InvokeError<Infallible>) -> Self {
        match value {
            InvokeError::Transport(never) => match never {},
            InvokeError::Kernel(result) => Self { result },
        }
    }
}
