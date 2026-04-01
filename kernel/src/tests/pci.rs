use alloc::{format, sync::Arc, vec::Vec};
use core::mem::size_of;

use libakarin_core::memory::VmLayoutSegment;
use libakarin_object::{Capability, Handle, ObjectError, Payload};
use libakarin_syscall::{
    CpAccessMode, PciFunctionMethod, PciHostMethod, PciInterruptBindArgs, PciInterruptModeKind,
    PciUnderlyingErrorCode, SYSCALL_STATUS_OK, SYSCALL_STATUS_UNDERLYING_ERROR, SyscallContext,
    SyscallResult, UserCopyError,
};

use crate::{
    RuntimeServices, Scheduler,
    device::pci::{PciFunctionInfo, PciHostBridge},
    sched::process::{
        Process, UserStackAllocation, allocate_user_stack_for_process,
        release_user_stack_for_process,
    },
};

const PCI_TEST_USER_STACK_PAGES: usize = 1;

/// PCI object and binder self-tests.
pub struct PciSelfTests;

struct PciTestContext {
    control_process: Arc<Process>,
    user_stack: UserStackAllocation,
}

impl PciTestContext {
    fn new(control_process: Arc<Process>, user_stack: UserStackAllocation) -> Self {
        Self {
            control_process,
            user_stack,
        }
    }

    fn user_stack_window(&self, addr: usize, len: usize) -> Result<usize, UserCopyError> {
        let range = self.user_stack.mapped_range();
        let start = range.start().as_usize();
        let end = range.end().as_usize();
        let requested_end = addr.checked_add(len).ok_or(UserCopyError::Fault)?;
        if addr < start || requested_end > end {
            return Err(UserCopyError::Fault);
        }

        Ok(addr - start)
    }
}

impl SyscallContext for PciTestContext {
    type ObjectError = ObjectError;
    type UserError = UserCopyError;
    type Handle = Handle;
    type Payload = Payload;
    type Capability = Capability;

    fn current_process(&self) -> Result<Handle, ObjectError> {
        self.control_process.task_process_handle()
    }

    fn install_handle(&self, handle: Handle) -> Result<u32, ObjectError> {
        Ok(self.control_process.install_handle_auto(handle))
    }

    fn create_anonymous_object(
        &self,
        payload: Payload,
        capability: Capability,
        interface_caps: u32,
    ) -> Result<u32, ObjectError> {
        let handle =
            self.control_process
                .create_anonymous_object(payload, capability, interface_caps)?;
        self.install_handle(handle)
    }

    fn destroy_anonymous_object(&self, slot: u32) -> Result<(), ObjectError> {
        self.control_process.destroy_anonymous_object(slot)
    }

    fn close_handle(&self, slot: u32) -> Result<(), ObjectError> {
        self.control_process.close_handle(slot)
    }

    fn acquire_handle(&self, slot: u32) -> Result<Handle, ObjectError> {
        self.control_process.acquire_handle(slot)
    }

    fn take_handle(&self, slot: u32) -> Result<Handle, ObjectError> {
        self.control_process.take_handle(slot)
    }

    fn copy_from_user(&self, src: usize, out: &mut [u8]) -> Result<(), UserCopyError> {
        let offset = self.user_stack_window(src, out.len())?;
        self.user_stack
            .vmo()
            .read(offset, out)
            .then_some(())
            .ok_or(UserCopyError::Fault)
    }

    fn copy_to_user(&self, dst: usize, input: &[u8]) -> Result<(), UserCopyError> {
        let offset = self.user_stack_window(dst, input.len())?;
        self.user_stack
            .vmo()
            .write(offset, input)
            .then_some(())
            .ok_or(UserCopyError::Fault)
    }
}

impl PciSelfTests {
    pub async fn run() {
        if !RuntimeServices::global().has_pci_runtime() {
            info!("[kernel/tests] pci self-tests skipped: pci runtime unavailable");
            return;
        }

        let control_process = match Scheduler::current_process() {
            Ok(process) => process,
            Err(error) => {
                warn!(
                    "[kernel/tests] pci self-tests skipped: current process lookup failed: {:?}",
                    error
                );
                return;
            }
        };
        let manager = RuntimeServices::global().namespaces().scheduler_manager();
        let user_process =
            match manager.create_process("pci-self-test", VmLayoutSegment::user_space_range()) {
                Ok(process) => process,
                Err(error) => {
                    warn!(
                        "[kernel/tests] pci self-tests skipped: temp user process creation \
                         failed: {:?}",
                        error
                    );
                    return;
                }
            };
        let user_stack = allocate_user_stack_for_process(&user_process, PCI_TEST_USER_STACK_PAGES)
            .expect("pci self-test user stack allocation must succeed");
        let caller = PciTestContext::new(control_process, user_stack.clone());

        let host = match RuntimeServices::global()
            .namespaces()
            .device_manager()
            .lookup_handle("PCI")
        {
            Ok(handle) => handle,
            Err(error) => {
                warn!(
                    "[kernel/tests] pci self-tests skipped: host object lookup failed: {:?}",
                    error
                );
                let _ = manager.destroy_process(user_process.pid());
                return;
            }
        };

        let host_summary = Self::invoke(
            &host,
            &caller,
            CpAccessMode::Read,
            PciHostMethod::QuerySummary as usize,
            0,
            0,
        )
        .await
        .expect("pci host query summary invoke must succeed");
        assert!(
            host_summary.is_ok(),
            "pci host query summary must succeed: {:?}",
            host_summary
        );

        let functions: Vec<PciFunctionInfo> = host
            .read_cp_with::<PciHostBridge, _, _>(|guard| {
                Ok::<Vec<PciFunctionInfo>, ObjectError>(guard.function_snapshot().to_vec())
            })
            .expect("pci host read access must succeed")
            .expect("pci host function snapshot extraction must succeed");
        assert_eq!(
            host_summary.values[0],
            functions.len(),
            "pci host summary function count must match published host snapshot",
        );

        if functions.is_empty() {
            info!("[kernel/tests] pci self-tests skipped: no published pci function objects");
            return;
        }

        let first = functions[0];
        let first_handle = Self::lookup_function(first)
            .expect("first published pci function must be discoverable");
        let info_result = Self::invoke(
            &first_handle,
            &caller,
            CpAccessMode::Read,
            PciFunctionMethod::QueryInfo as usize,
            0,
            0,
        )
        .await
        .expect("pci function query info invoke must succeed");
        assert_eq!(info_result.status, SYSCALL_STATUS_OK);
        assert_eq!(info_result.values[0], usize::from(first.bdf.segment));
        assert_eq!(
            info_result.values[1],
            (usize::from(first.bdf.bus) << 16)
                | (usize::from(first.bdf.device) << 8)
                | usize::from(first.bdf.function)
        );

        let mode_result = Self::invoke(
            &first_handle,
            &caller,
            CpAccessMode::Read,
            PciFunctionMethod::QueryInterruptModes as usize,
            0,
            0,
        )
        .await
        .expect("pci function query interrupt modes invoke must succeed");
        assert_eq!(mode_result.status, SYSCALL_STATUS_OK);

        let bar_result = Self::invoke(
            &first_handle,
            &caller,
            CpAccessMode::Read,
            PciFunctionMethod::QueryBars as usize,
            0,
            0,
        )
        .await
        .expect("pci function query bars invoke must succeed");
        assert_eq!(bar_result.status, SYSCALL_STATUS_OK);

        let capability_result = Self::invoke(
            &first_handle,
            &caller,
            CpAccessMode::Read,
            PciFunctionMethod::QueryCapabilities as usize,
            0,
            0,
        )
        .await
        .expect("pci function query capabilities invoke must succeed");
        assert_eq!(capability_result.status, SYSCALL_STATUS_OK);

        let candidate = functions.iter().find_map(|info| {
            let summary = RuntimeServices::global().pci().capability_summary(info.bdf);
            if summary.max_msi_vectors > 0 {
                Some((*info, PciFunctionMethod::EnableMsi))
            } else if summary.max_msix_vectors > 0 {
                Some((*info, PciFunctionMethod::EnableMsix))
            } else {
                None
            }
        });

        let Some((candidate, enable_method)) = candidate else {
            info!(
                "[kernel/tests] pci self-tests: no MSI/MSI-X capable published function, \
                 query-only coverage complete"
            );
            return;
        };

        let function = Self::lookup_function(candidate)
            .expect("interrupt-capable pci function must be discoverable");
        let execute = function
            .derive_handle(
                Capability::EXECUTE,
                libakarin_syscall::PCI_FUNCTION_INTERRUPT,
            )
            .expect("pci function lookup must derive execute/interrupt handle");

        let user_base = user_stack.mapped_range().start().as_usize();
        let args_ptr = user_base;
        let slots_ptr = user_base + size_of::<PciInterruptBindArgs>();
        let slot_offset = slots_ptr - user_base;

        let invalid_args = PciInterruptBindArgs {
            count: 0,
            cpu_hint: PciInterruptBindArgs::NO_CPU_HINT,
            slots_ptr,
            slots_len: 1,
        };
        Self::write_user_bytes(user_stack.vmo(), 0, Self::as_bytes(&invalid_args))
            .expect("pci self-test must seed invalid bind args");
        let invalid = Self::invoke(
            &execute,
            &caller,
            CpAccessMode::Execute,
            enable_method as usize,
            args_ptr,
            0,
        )
        .await
        .expect("pci function invalid enable invoke must dispatch");
        assert_eq!(invalid.status, SYSCALL_STATUS_UNDERLYING_ERROR);
        assert_eq!(
            invalid.values[0],
            PciUnderlyingErrorCode::InvalidParameter as usize
        );

        let valid_args = PciInterruptBindArgs {
            count: 1,
            cpu_hint: PciInterruptBindArgs::NO_CPU_HINT,
            slots_ptr,
            slots_len: 1,
        };
        let zero_slot = [0u8; size_of::<u32>()];
        Self::write_user_bytes(user_stack.vmo(), 0, Self::as_bytes(&valid_args))
            .expect("pci self-test must seed bind args");
        Self::write_user_bytes(user_stack.vmo(), slot_offset, &zero_slot)
            .expect("pci self-test must clear slot buffer");

        let enable = Self::invoke(
            &execute,
            &caller,
            CpAccessMode::Execute,
            enable_method as usize,
            args_ptr,
            0,
        )
        .await
        .expect("pci function enable invoke must dispatch");
        assert_eq!(
            enable.status, SYSCALL_STATUS_OK,
            "enable result: {:?}",
            enable
        );
        assert_eq!(
            enable.values[0], 1,
            "enable must return exactly one session slot"
        );
        assert_eq!(
            PciInterruptModeKind::try_from(enable.values[1]).ok(),
            Some(match enable_method {
                PciFunctionMethod::EnableMsi => PciInterruptModeKind::Msi,
                PciFunctionMethod::EnableMsix => PciInterruptModeKind::Msix,
                _ => unreachable!(),
            })
        );

        let slot = Self::read_user_u32(user_stack.vmo(), slot_offset)
            .expect("pci self-test must read back irq session slot");
        let session_handle = caller
            .acquire_handle(slot)
            .expect("enable must install one irq session handle into caller table");
        drop(session_handle);

        let enabled_mode = Self::invoke(
            &function,
            &caller,
            CpAccessMode::Read,
            PciFunctionMethod::QueryInterruptModes as usize,
            0,
            0,
        )
        .await
        .expect("pci function query interrupt modes after enable must succeed");
        assert_eq!(enabled_mode.status, SYSCALL_STATUS_OK);
        assert_eq!(
            PciInterruptModeKind::try_from(enabled_mode.values[1]).ok(),
            Some(match enable_method {
                PciFunctionMethod::EnableMsi => PciInterruptModeKind::Msi,
                PciFunctionMethod::EnableMsix => PciInterruptModeKind::Msix,
                _ => unreachable!(),
            })
        );

        let disable = Self::invoke(
            &execute,
            &caller,
            CpAccessMode::Execute,
            PciFunctionMethod::DisableInterrupts as usize,
            0,
            0,
        )
        .await
        .expect("pci function disable invoke must dispatch");
        assert_eq!(
            disable.status, SYSCALL_STATUS_OK,
            "disable result: {:?}",
            disable
        );

        let disabled_mode = Self::invoke(
            &function,
            &caller,
            CpAccessMode::Read,
            PciFunctionMethod::QueryInterruptModes as usize,
            0,
            0,
        )
        .await
        .expect("pci function query interrupt modes after disable must succeed");
        assert_eq!(disabled_mode.status, SYSCALL_STATUS_OK);
        assert_eq!(
            PciInterruptModeKind::try_from(disabled_mode.values[1]).ok(),
            Some(PciInterruptModeKind::Intx)
        );

        let disable_again = Self::invoke(
            &execute,
            &caller,
            CpAccessMode::Execute,
            PciFunctionMethod::DisableInterrupts as usize,
            0,
            0,
        )
        .await
        .expect("pci function repeated disable invoke must dispatch");
        assert_eq!(
            disable_again.status, SYSCALL_STATUS_OK,
            "repeat disable result: {:?}",
            disable_again
        );

        caller
            .destroy_anonymous_object(slot)
            .expect("pci self-test must destroy created irq session handle");
        release_user_stack_for_process(&user_process, user_stack)
            .expect("pci self-test user stack release must succeed");
        manager
            .destroy_process(user_process.pid())
            .expect("pci self-test temp process destroy must succeed");
    }

    async fn invoke(
        handle: &Handle,
        caller: &PciTestContext,
        mode: CpAccessMode,
        method_id: usize,
        arg1: usize,
        arg2: usize,
    ) -> Result<SyscallResult, ObjectError> {
        handle
            .invoke_cp(caller, mode, method_id, arg1, arg2)
            .await
            .map(SyscallResult::from)
    }

    fn lookup_function(info: PciFunctionInfo) -> Result<Handle, ObjectError> {
        RuntimeServices::global()
            .namespaces()
            .resource_manager()
            .lookup_handle(&format!("PCI/{}", info.object_name()))
    }

    fn write_user_bytes(
        vmo: &libakarin_core::memory::Vmo,
        offset: usize,
        bytes: &[u8],
    ) -> Result<(), ObjectError> {
        vmo.write(offset, bytes)
            .then_some(())
            .ok_or(ObjectError::InvalidArgument)
    }

    fn read_user_u32(vmo: &libakarin_core::memory::Vmo, offset: usize) -> Result<u32, ObjectError> {
        let mut bytes = [0u8; size_of::<u32>()];
        vmo.read(offset, &mut bytes)
            .then_some(())
            .ok_or(ObjectError::InvalidArgument)?;
        Ok(u32::from_ne_bytes(bytes))
    }

    fn as_bytes<T>(value: &T) -> &[u8] {
        unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
    }
}
