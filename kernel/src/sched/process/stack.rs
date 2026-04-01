use super::*;
use crate::{arch::PerCpu, sched::stack::DEFAULT_STACK_PAGES};

/// Creator-side userspace stack allocation descriptor.
#[derive(Debug, Clone)]
pub struct UserStackAllocation {
    slot: Arc<Vmar>,
    vmo: Arc<Vmo>,
    mapped_range: VmRange,
}

impl UserStackAllocation {
    pub fn new(slot: Arc<Vmar>, vmo: Arc<Vmo>, mapped_range: VmRange) -> Self {
        Self {
            slot,
            vmo,
            mapped_range,
        }
    }

    pub fn slot(&self) -> &Arc<Vmar> {
        &self.slot
    }

    pub fn vmo(&self) -> &Arc<Vmo> {
        &self.vmo
    }

    pub fn mapped_range(&self) -> VmRange {
        self.mapped_range
    }

    pub fn top(&self) -> usize {
        self.mapped_range.end().as_usize()
    }
}

impl Process {
    const PROCESS_KERNEL_STACK_REGION_SIZE: usize = 64 * 1024 * 1024;

    pub(super) fn kernel_stack_root_range(pid: ProcessId) -> Option<VmRange> {
        let reserved_pages = (DEFAULT_STACK_PAGES + 1).checked_mul(PerCpu::count())?;
        let reserved_bytes = reserved_pages.checked_mul(0x1000)?;
        let process_index = pid.checked_sub(1)? as usize;
        let region_base = process_index.checked_mul(Self::PROCESS_KERNEL_STACK_REGION_SIZE)?;
        let start = VmLayoutSegment::KernelStack
            .start()
            .checked_add(reserved_bytes)?
            .checked_add(region_base)?;
        let end = start.checked_add(Self::PROCESS_KERNEL_STACK_REGION_SIZE)?;
        if end > VmLayoutSegment::KernelStack.end_exclusive()? {
            return None;
        }
        VmRange::new(VirtAddr::new(start), VirtAddr::new(end))
    }

    fn kernel_vmar(&self) -> Result<Arc<Vmar>, ObjectError> {
        self.kernel_vmar
            .lock()
            .as_ref()
            .cloned()
            .ok_or(ObjectError::ObjectNotFound)
    }

    fn allocate_stack_slot(
        &self,
        root: &Arc<Vmar>,
        mapped_pages: usize,
    ) -> Result<(Arc<Vmar>, GuardedStackLayout), ObjectError> {
        let mapped_size = mapped_pages
            .checked_mul(0x1000)
            .ok_or(ObjectError::InvalidArgument)?;
        let reserved_size = mapped_size
            .checked_add(0x1000)
            .ok_or(ObjectError::InvalidArgument)?;
        let slot = root
            .allocate_child_any(reserved_size)
            .map_err(|_| ObjectError::InvalidArgument)?;
        let layout = GuardedStackLayout::from_low_base(slot.range().start(), mapped_size)
            .ok_or(ObjectError::InvalidArgument)?;
        Ok((slot, layout))
    }

    fn install_stack_mapping(
        &self,
        slot: &Arc<Vmar>,
        vmo: &Arc<Vmo>,
        range: VmRange,
        flags: VmFlags,
        purpose: RegionPurpose,
    ) -> Result<VmarMapping, ObjectError> {
        let mapping = VmarMapping {
            range,
            flags,
            purpose,
            vmo: Arc::clone(vmo),
            vmo_offset: 0,
        };
        slot.map_vmo(range, Arc::clone(vmo), 0, flags, purpose)
            .map_err(|_| ObjectError::InvalidArgument)?;
        if let Err(err) = self.map_mapping_in_page_table(&mapping) {
            let _ = slot.unmap(range.start());
            return Err(err);
        }
        Ok(mapping)
    }

    fn uninstall_stack_mapping(
        &self,
        root: &Arc<Vmar>,
        slot: &Arc<Vmar>,
        range: VmRange,
        vmo: &Arc<Vmo>,
        flags: VmFlags,
        purpose: RegionPurpose,
    ) -> Result<(), ObjectError> {
        let mapping = VmarMapping {
            range,
            flags,
            purpose,
            vmo: Arc::clone(vmo),
            vmo_offset: 0,
        };
        self.unmap_entry_from_page_table(&VmarEntry::Mapping(mapping))?;
        slot.unmap(range.start())
            .map_err(|_| ObjectError::ObjectNotFound)?;
        root.unmap(slot.range().start())
            .map_err(|_| ObjectError::ObjectNotFound)?;
        Ok(())
    }

    /// Allocate and map one kernel stack inside the process kernel-stack
    /// segment.
    pub fn allocate_kernel_stack(&self, mapped_pages: usize) -> Result<KernelStack, ObjectError> {
        let root = self.kernel_vmar()?;
        let (slot, layout) = self.allocate_stack_slot(&root, mapped_pages)?;
        let vmo = Arc::new(Vmo::new(
            format!("proc{}-kstack", self.pid),
            layout.stack.len(),
            0x1000,
            VmFlags::READ | VmFlags::WRITE | VmFlags::MAP,
        ));
        self.install_stack_mapping(
            &slot,
            &vmo,
            layout.stack,
            VmFlags::READ | VmFlags::WRITE,
            RegionPurpose::KernelStack,
        )?;
        Ok(KernelStack::new(slot, vmo, layout.stack))
    }

    /// Unmap and release one kernel stack previously allocated by this process.
    pub fn release_kernel_stack(&self, stack: KernelStack) -> Result<(), ObjectError> {
        let root = self.kernel_vmar()?;
        self.uninstall_stack_mapping(
            &root,
            stack.slot(),
            stack.mapped_range(),
            stack.vmo(),
            VmFlags::READ | VmFlags::WRITE,
            RegionPurpose::KernelStack,
        )
    }
}

impl Process {}

fn user_stack_root_for_process(process: &Process) -> Result<Arc<Vmar>, ObjectError> {
    match process.segment_vmar(VmLayoutSegment::UserStack) {
        Ok(root) => Ok(root),
        Err(ProcessVmError::Object(error)) => Err(error),
        Err(ProcessVmError::Underlying(_)) => Err(ObjectError::InvalidArgument),
    }
}

/// Allocate one userspace stack inside the fixed `UserStack` segment for the
/// supplied process.
pub(crate) fn allocate_user_stack_for_process(
    process: &Process,
    mapped_pages: usize,
) -> Result<UserStackAllocation, ObjectError> {
    let root = user_stack_root_for_process(process)?;
    let (slot, layout) = process.allocate_stack_slot(&root, mapped_pages)?;
    let vmo = Arc::new(Vmo::new(
        format!("proc{}-ustack", process.pid()),
        layout.stack.len(),
        0x1000,
        VmFlags::READ | VmFlags::WRITE | VmFlags::USER | VmFlags::MAP,
    ));
    process.install_stack_mapping(
        &slot,
        &vmo,
        layout.stack,
        VmFlags::READ | VmFlags::WRITE | VmFlags::USER,
        RegionPurpose::User,
    )?;
    Ok(UserStackAllocation::new(slot, vmo, layout.stack))
}

/// Release one userspace stack previously allocated from `UserStack` for the
/// supplied process.
pub(crate) fn release_user_stack_for_process(
    process: &Process,
    stack: UserStackAllocation,
) -> Result<(), ObjectError> {
    let root = user_stack_root_for_process(process)?;
    process.uninstall_stack_mapping(
        &root,
        stack.slot(),
        stack.mapped_range(),
        stack.vmo(),
        VmFlags::READ | VmFlags::WRITE | VmFlags::USER,
        RegionPurpose::User,
    )
}
