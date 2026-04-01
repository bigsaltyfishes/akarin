use alloc::sync::Arc;

use libakarin_core::memory::{VmFlags, VmLayoutSegment, VmRange, Vmo, VmoPagePurpose};
use libakarin_machine_core::memory::VirtAddr;
use libakarin_object::ObjectError;
use libakarin_syscall::ProcessSpawnError;

use crate::{
    RuntimeServices, TaskId,
    arch::Machine,
    sched::process::{
        BootstrapLoadError, ProcessVmError, allocate_user_stack_for_process,
        release_user_stack_for_process,
    },
};

/// Errors returned while starting the first bootstrap process.
#[derive(Debug)]
pub enum BootstrapProgramError {
    Object(ObjectError),
    Vm(ProcessVmError),
    Load(BootstrapLoadError),
    Spawn(ProcessSpawnError),
}

impl From<ObjectError> for BootstrapProgramError {
    fn from(value: ObjectError) -> Self {
        Self::Object(value)
    }
}

impl From<ProcessVmError> for BootstrapProgramError {
    fn from(value: ProcessVmError) -> Self {
        Self::Vm(value)
    }
}

impl From<BootstrapLoadError> for BootstrapProgramError {
    fn from(value: BootstrapLoadError) -> Self {
        Self::Load(value)
    }
}

impl From<ProcessSpawnError> for BootstrapProgramError {
    fn from(value: ProcessSpawnError) -> Self {
        Self::Spawn(value)
    }
}

/// Load the bootloader-provided bootstrap image and spawn it as one task.
pub fn spawn_bootstrap_program() -> Result<TaskId, BootstrapProgramError> {
    let image = bootstrap_image()?;
    let manager = RuntimeServices::global().namespaces().scheduler_manager();
    let (process, _bootstrap_handle) =
        manager.create_process_bootstrap("bootstrap", process_root_range())?;
    let image_vmo = bootstrap_image_vmo(image)?;
    let loaded = process.load_image_from_vmo(&image_vmo)?;
    process.stage_loaded_image(loaded);
    let user_stack = allocate_user_stack_for_process(&process, 16)?;
    let bootstrap_abi = process.install_bootstrap_handles(loaded.entry_ip)?;
    let stack_pointer = process.build_bootstrap_stack(&user_stack, &bootstrap_abi)?;
    let spawn = process.spawn_initial_task(loaded.entry_ip, stack_pointer, 0);
    if spawn.is_err() {
        let _ = release_user_stack_for_process(&process, user_stack);
    }
    let (task_id, _supervisor) = spawn?;
    Ok(task_id)
}

fn bootstrap_image() -> Result<&'static [u8], BootstrapProgramError> {
    let bytes = RuntimeServices::global()
        .namespaces()
        .bootloader_manager()
        .bootstrap_program_bytes()?;
    if bytes.is_empty() {
        return Err(BootstrapProgramError::Load(
            BootstrapLoadError::InvalidExecutable("bootstrap image is empty"),
        ));
    }
    Ok(bytes)
}

fn bootstrap_image_vmo(bytes: &'static [u8]) -> Result<Arc<Vmo>, BootstrapProgramError> {
    let image_capacity = bytes.len().next_multiple_of(0x1000);
    let image_vmo = Arc::new(Vmo::new(
        "bootstrap-image",
        image_capacity,
        0x1000,
        VmFlags::READ | VmFlags::WRITE | VmFlags::USER | VmFlags::MAP,
    ));
    image_vmo
        .commit_range::<Machine>(
            0,
            image_capacity,
            VmoPagePurpose::Anonymous,
            RuntimeServices::global().frame_allocator(),
        )
        .map_err(|_| {
            BootstrapLoadError::InvalidExecutable(
                "failed to commit bootstrap image VMO backing range",
            )
        })?;
    if !image_vmo.write(0, bytes) {
        return Err(BootstrapLoadError::InvalidExecutable(
            "failed to write bootstrap image into VMO",
        )
        .into());
    }
    image_vmo.set_stream_size(bytes.len());
    Ok(image_vmo)
}

fn process_root_range() -> VmRange {
    VmRange::new(
        VirtAddr::new(VmLayoutSegment::UserImage.start()),
        VirtAddr::new(
            VmLayoutSegment::UserStack
                .end_exclusive()
                .expect("bootstrap process root range must be bounded"),
        ),
    )
    .expect("bootstrap process root range must be valid")
}
