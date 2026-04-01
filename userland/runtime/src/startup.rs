use core::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    ptr,
    sync::atomic::{AtomicBool, Ordering},
};

use libakarin_syscall::{
    AT_AK_HEAP_BASE, AT_AK_HEAP_LIMIT, AT_AK_HEAP_VMAR, AT_AK_MAPPED_VMAR, AT_AK_PAGE_SIZE,
    AT_AK_PROCESS_SELF, AT_AK_ROOT_NS, AT_AK_ROOT_VMAR, AT_AK_SYSCALL_TABLE, AT_ENTRY, AT_NULL,
    AT_PAGESZ,
};

use crate::allocator::{GLOBAL_ALLOCATOR, HeapAllocatorError};

/// Parsed startup information taken from the initial userspace stack.
#[derive(Debug, Clone, Copy)]
pub struct StartupInfo {
    pub initial_stack_pointer: usize,
    pub argc: usize,
    pub argv: *const usize,
    pub envp: *const usize,
    pub entry_point: usize,
    pub process_self_slot: u32,
    pub root_ns_slot: u32,
    pub root_vmar_slot: u32,
    pub heap_vmar_slot: u32,
    pub mapped_vmar_slot: u32,
    pub syscall_table_slot: u32,
    pub page_size: usize,
    pub heap_base: usize,
    pub heap_limit: usize,
}

/// Runtime initialization failures detected before user code starts running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeInitError {
    AlreadyInitialized,
    MissingAuxv(&'static str),
    InvalidAuxv(&'static str),
    Heap(HeapAllocatorError),
}

/// Global userspace runtime state installed from the initial stack image.
pub struct Runtime {
    initialized: AtomicBool,
    startup: UnsafeCell<MaybeUninit<StartupInfo>>,
}

unsafe impl Sync for Runtime {}

pub static RUNTIME: Runtime = Runtime {
    initialized: AtomicBool::new(false),
    startup: UnsafeCell::new(MaybeUninit::uninit()),
};

impl StartupInfo {
    /// Parse the SysV-style initial userspace stack installed by the kernel.
    pub unsafe fn from_initial_stack(
        initial_stack_pointer: usize,
    ) -> Result<Self, RuntimeInitError> {
        unsafe {
            let argc = ptr::read(initial_stack_pointer as *const usize);
            let argv = (initial_stack_pointer as *const usize).add(1);
            let mut cursor = argv;
            let mut seen_argv = 0usize;
            while seen_argv < argc {
                if ptr::read(cursor) == 0 {
                    return Err(RuntimeInitError::InvalidAuxv(
                        "argv terminated before argc entries were present",
                    ));
                }
                cursor = cursor.add(1);
                seen_argv += 1;
            }

            if ptr::read(cursor) != 0 {
                return Err(RuntimeInitError::InvalidAuxv(
                    "argv must be terminated by one NULL entry",
                ));
            }
            cursor = cursor.add(1);
            let envp = cursor;
            while ptr::read(cursor) != 0 {
                cursor = cursor.add(1);
            }
            cursor = cursor.add(1);

            let mut entry_point = None;
            let mut page_size = None;
            let mut process_self_slot = None;
            let mut root_ns_slot = None;
            let mut root_vmar_slot = None;
            let mut heap_vmar_slot = None;
            let mut mapped_vmar_slot = None;
            let mut syscall_table_slot = None;
            let mut heap_base = None;
            let mut heap_limit = None;

            loop {
                let tag = ptr::read(cursor);
                let value = ptr::read(cursor.add(1));
                cursor = cursor.add(2);
                if tag == AT_NULL {
                    break;
                }
                if tag == AT_ENTRY {
                    entry_point = Some(value);
                    continue;
                }
                if tag == AT_PAGESZ || tag == AT_AK_PAGE_SIZE {
                    page_size = Some(value);
                    continue;
                }
                if tag == AT_AK_PROCESS_SELF {
                    process_self_slot = Some(value as u32);
                    continue;
                }
                if tag == AT_AK_ROOT_NS {
                    root_ns_slot = Some(value as u32);
                    continue;
                }
                if tag == AT_AK_ROOT_VMAR {
                    root_vmar_slot = Some(value as u32);
                    continue;
                }
                if tag == AT_AK_HEAP_VMAR {
                    heap_vmar_slot = Some(value as u32);
                    continue;
                }
                if tag == AT_AK_MAPPED_VMAR {
                    mapped_vmar_slot = Some(value as u32);
                    continue;
                }
                if tag == AT_AK_SYSCALL_TABLE {
                    syscall_table_slot = Some(value as u32);
                    continue;
                }
                if tag == AT_AK_HEAP_BASE {
                    heap_base = Some(value);
                    continue;
                }
                if tag == AT_AK_HEAP_LIMIT {
                    heap_limit = Some(value);
                    continue;
                }
            }

            let page_size = page_size.ok_or(RuntimeInitError::MissingAuxv("AT_PAGESZ"))?;
            let heap_base = heap_base.ok_or(RuntimeInitError::MissingAuxv("AT_AK_HEAP_BASE"))?;
            let heap_limit = heap_limit.ok_or(RuntimeInitError::MissingAuxv("AT_AK_HEAP_LIMIT"))?;
            if page_size == 0 || heap_base >= heap_limit {
                return Err(RuntimeInitError::InvalidAuxv(
                    "heap metadata must describe one non-empty aligned range",
                ));
            }

            Ok(Self {
                initial_stack_pointer,
                argc,
                argv,
                envp,
                entry_point: entry_point.ok_or(RuntimeInitError::MissingAuxv("AT_ENTRY"))?,
                process_self_slot: process_self_slot
                    .ok_or(RuntimeInitError::MissingAuxv("AT_AK_PROCESS_SELF"))?,
                root_ns_slot: root_ns_slot.ok_or(RuntimeInitError::MissingAuxv("AT_AK_ROOT_NS"))?,
                root_vmar_slot: root_vmar_slot
                    .ok_or(RuntimeInitError::MissingAuxv("AT_AK_ROOT_VMAR"))?,
                heap_vmar_slot: heap_vmar_slot
                    .ok_or(RuntimeInitError::MissingAuxv("AT_AK_HEAP_VMAR"))?,
                mapped_vmar_slot: mapped_vmar_slot
                    .ok_or(RuntimeInitError::MissingAuxv("AT_AK_MAPPED_VMAR"))?,
                syscall_table_slot: syscall_table_slot
                    .ok_or(RuntimeInitError::MissingAuxv("AT_AK_SYSCALL_TABLE"))?,
                page_size,
                heap_base,
                heap_limit,
            })
        }
    }
}

impl Runtime {
    /// Initialize the userspace runtime exactly once and publish the parsed
    /// startup information for later consumers.
    pub unsafe fn initialize(
        &'static self,
        initial_stack_pointer: usize,
    ) -> Result<&'static StartupInfo, RuntimeInitError> {
        if self.initialized.load(Ordering::Acquire) {
            return Err(RuntimeInitError::AlreadyInitialized);
        }

        let startup = unsafe { StartupInfo::from_initial_stack(initial_stack_pointer)? };
        GLOBAL_ALLOCATOR
            .initialize(&startup)
            .map_err(RuntimeInitError::Heap)?;
        unsafe {
            ptr::write((*self.startup.get()).as_mut_ptr(), startup);
        }
        self.initialized.store(true, Ordering::Release);
        unsafe { Ok((&*self.startup.get()).assume_init_ref()) }
    }
}
