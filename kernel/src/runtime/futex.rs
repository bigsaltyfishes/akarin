use alloc::{sync::Arc, vec::Vec};
use core::{
    mem::offset_of,
    sync::atomic::{AtomicBool, Ordering},
};

use hashbrown::HashMap;
use libakarin_collections::intrusive::{DoubleLink, ElememtOf, LinkedList};
use libakarin_core::{clock::time::Duration, memory::VmoChildKind};
use libakarin_machine_core::memory::VirtAddr;
use libakarin_sync::{asynchronous::Event, spin::SpinLock};
use libakarin_syscall::{VmError, errno::FutexError};

use super::extensions::TimeoutExt;
use crate::{arch::guards::IrqSaveGuard, sched::process::Process, syscall::UserPtr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FutexKey {
    vmo_id: u64,
    vmo_offset: usize,
}

struct FutexWaiter {
    event: Event,
    woken: AtomicBool,
    queued: AtomicBool,
    link: DoubleLink,
}

impl FutexWaiter {
    fn new() -> Self {
        Self {
            event: Event::new(),
            woken: AtomicBool::new(false),
            queued: AtomicBool::new(false),
            link: DoubleLink::new(),
        }
    }
}

impl ElememtOf<FutexWaiter, DoubleLink> for FutexWaiter {
    fn link(node: &FutexWaiter) -> &DoubleLink {
        &node.link
    }

    fn link_mut(node: &mut FutexWaiter) -> &mut DoubleLink {
        &mut node.link
    }

    fn element(link: &DoubleLink) -> &FutexWaiter {
        let offset = offset_of!(FutexWaiter, link);
        unsafe { &*((link as *const DoubleLink).byte_sub(offset) as *const FutexWaiter) }
    }

    fn element_mut(link: &mut DoubleLink) -> &mut FutexWaiter {
        let offset = offset_of!(FutexWaiter, link);
        unsafe { &mut *((link as *mut DoubleLink).byte_sub(offset) as *mut FutexWaiter) }
    }
}

#[derive(Debug)]
struct FutexWaitQueue {
    list: LinkedList<FutexWaiter, DoubleLink>,
}

impl FutexWaitQueue {
    fn new() -> Self {
        Self {
            list: LinkedList::new(),
        }
    }
}

struct FutexRegistration<'a> {
    manager: &'a FutexManager,
    key: FutexKey,
    waiter: Arc<FutexWaiter>,
    armed: bool,
}

impl Drop for FutexRegistration<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.manager.remove_waiter(self.key, &self.waiter);
        }
    }
}

/// Runtime-owned futex wait/wake service used by userspace synchronization
/// syscalls.
pub struct FutexManager {
    waiters: SpinLock<HashMap<FutexKey, FutexWaitQueue>, IrqSaveGuard>,
}

impl FutexManager {
    /// Create one empty futex wait registry.
    pub fn new() -> Self {
        Self {
            waiters: SpinLock::new(HashMap::new()),
        }
    }

    fn insert_waiter(&self, key: FutexKey, waiter: &Arc<FutexWaiter>) {
        let mut waiters = self.waiters.lock();
        let queue = waiters.entry(key).or_insert_with(FutexWaitQueue::new);
        let waiter_ptr = Arc::as_ptr(waiter) as *mut FutexWaiter;
        waiter.queued.store(true, Ordering::Release);
        unsafe {
            queue.list.push_back(waiter_ptr);
        }
    }

    fn remove_waiter(&self, key: FutexKey, waiter: &Arc<FutexWaiter>) -> bool {
        let mut waiters = self.waiters.lock();
        let mut removed = false;
        let mut remove_key = false;
        if let Some(queue) = waiters.get_mut(&key) {
            if waiter.queued.swap(false, Ordering::AcqRel) {
                let waiter_ptr = Arc::as_ptr(waiter) as *mut FutexWaiter;
                unsafe {
                    (*waiter_ptr).link.detach(&mut queue.list);
                }
                removed = true;
            }
            remove_key = queue.list.is_empty();
        }
        if remove_key {
            let _ = waiters.remove(&key);
        }
        removed
    }

    fn take_woken_waiters(&self, key: FutexKey, wake_count: usize) -> Vec<Arc<FutexWaiter>> {
        let mut ready = Vec::new();
        let mut waiters = self.waiters.lock();
        let mut remove_key = false;
        if let Some(queue) = waiters.get_mut(&key) {
            for _ in 0..wake_count {
                let Some(waiter_ptr) = (unsafe { queue.list.pop_front() }) else {
                    break;
                };
                let waiter_ptr = waiter_ptr as *const FutexWaiter;
                unsafe {
                    (*waiter_ptr).queued.store(false, Ordering::Release);
                    (*waiter_ptr).woken.store(true, Ordering::Release);
                    Arc::increment_strong_count(waiter_ptr);
                    ready.push(Arc::from_raw(waiter_ptr));
                }
            }
            remove_key = queue.list.is_empty();
        }
        if remove_key {
            let _ = waiters.remove(&key);
        }
        ready
    }

    fn addr_to_key(process: &Process, user_addr: usize) -> Result<FutexKey, FutexError> {
        let user_virt = VirtAddr::new(user_addr);
        let mapping = process
            .mapping_at(user_virt)
            .map_err(|error| match error.into_object_or_underlying() {
                Ok(_) => FutexError::Fault,
                Err(
                    VmError::InvalidRange
                    | VmError::AlreadyMapped
                    | VmError::PermissionDenied
                    | VmError::NotMapped
                    | VmError::BufferTooSmall
                    | VmError::Fault
                    | VmError::InvalidArgument,
                ) => FutexError::Fault,
            })?
            .ok_or(FutexError::Fault)?;
        let mut vmo_id = mapping.vmo.id();
        let mut vmo_offset = mapping
            .vmo_offset_for_addr(user_virt)
            .ok_or(FutexError::Fault)?;
        if let Some(child) = mapping.vmo.child_info()
            && child.kind == VmoChildKind::SharedView
        {
            vmo_id = child.parent_id;
            vmo_offset = child
                .parent_offset
                .checked_add(vmo_offset)
                .ok_or(FutexError::Fault)?;
        }
        Ok(FutexKey { vmo_id, vmo_offset })
    }

    /// Wait for one futex word to remain equal to `expected`
    /// and then sleep until one wake or timeout is observed.
    pub async fn wait(
        &self,
        process: &Process,
        user_addr: usize,
        expected: u32,
        timeout_ns: usize,
    ) -> Result<(), FutexError> {
        if !user_addr.is_multiple_of(core::mem::align_of::<u32>()) {
            return Err(FutexError::InvalidArgument);
        }

        let key = Self::addr_to_key(process, user_addr)?;
        let waiter = Arc::new(FutexWaiter::new());

        let observed = UserPtr::<u32>::new(user_addr)
            .read(process)
            .map_err(|_| FutexError::Fault)?;
        if observed != expected {
            return Err(FutexError::WouldBlock);
        }
        if timeout_ns == 0 {
            return Err(FutexError::TimedOut);
        }

        self.insert_waiter(key, &waiter);

        let mut registration = FutexRegistration {
            manager: self,
            key,
            waiter: waiter.clone(),
            armed: true,
        };
        let listener = waiter.event.listen();
        if waiter.woken.load(Ordering::Acquire) {
            registration.armed = false;
            return Ok(());
        }

        let woke = if timeout_ns == usize::MAX {
            listener.await;
            true
        } else {
            listener
                .timeout(Duration::from_nanos(
                    timeout_ns.min(u64::MAX as usize) as u64
                ))
                .await
                .is_some()
        };

        if waiter.woken.load(Ordering::Acquire) {
            registration.armed = false;
            return Ok(());
        }
        if woke {
            registration.armed = false;
            return Ok(());
        }

        let removed = self.remove_waiter(key, &waiter);
        registration.armed = false;
        if waiter.woken.load(Ordering::Acquire) {
            return Ok(());
        }
        if removed {
            return Err(FutexError::TimedOut);
        }
        Err(FutexError::TimedOut)
    }

    /// Wake up to `wake_count` waiters blocked on one futex
    /// word and return the actual wake count.
    pub fn wake(
        &self,
        process: &Process,
        user_addr: usize,
        wake_count: usize,
    ) -> Result<usize, FutexError> {
        if !user_addr.is_multiple_of(core::mem::align_of::<u32>()) {
            return Err(FutexError::InvalidArgument);
        }
        if wake_count == 0 {
            return Ok(0);
        }

        let key = Self::addr_to_key(process, user_addr)?;
        let ready = self.take_woken_waiters(key, wake_count);

        for waiter in &ready {
            waiter.event.notify_all();
        }
        Ok(ready.len())
    }
}
