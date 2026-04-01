//! Local state machine for per-thread/per-handle memory reclamation state.
//!
//! Each `Local` instance holds the thread-local state needed for memory
//! reclamation, including the retirement batch and reservation list.

use alloc::boxed::Box;
use core::{
    cell::{Cell, UnsafeCell},
    mem::offset_of,
    ptr,
    sync::atomic::{self, AtomicPtr, AtomicUsize, Ordering},
};

use crossbeam_utils::CachePadded;
use libakarin_collections::intrusive::{ElememtOf, LinkedList, SingleLink};

use super::{membarrier, utils::FixedVec};
use crate::gc::{
    Collector, Entry, Guard, IsElement, LocalHandle, Owned, Shared, guard::unprotected,
};

/// A batch of nodes waiting to be retired.
struct Batch {
    /// A intrusive link.
    link: SingleLink,

    /// Nodes in this batch.
    entries: FixedVec<Node>,

    /// The reference count for any active threads.
    active: AtomicUsize,
}

impl Batch {
    /// Create a new batch with the specified capacity.
    #[inline]
    fn new(capacity: usize) -> Batch {
        Batch {
            link: SingleLink::new(),
            entries: FixedVec::new(capacity),
            active: AtomicUsize::new(0),
        }
    }
}

/// A `Send` container for `Batch` to be used in `LinkedList`.
#[repr(transparent)]
struct BatchElem(Batch);

impl BatchElem {
    #[inline]
    pub fn ptr_cast(batch: *mut Batch) -> *mut BatchElem {
        batch as *mut BatchElem
    }

    #[inline]
    pub fn ref_cast(batch: &Batch) -> &BatchElem {
        unsafe { &*(batch as *const Batch as *const BatchElem) }
    }

    #[inline]
    pub fn mut_cast(batch: &mut Batch) -> &mut BatchElem {
        unsafe { &mut *(batch as *mut Batch as *mut BatchElem) }
    }
}

/// Safety: Any access to `Batch` through `BatchElem` is unsafe.
unsafe impl Send for BatchElem {}

impl ElememtOf<BatchElem, SingleLink> for BatchElem {
    fn link(node: &BatchElem) -> &SingleLink {
        &node.0.link
    }

    fn link_mut(node: &mut BatchElem) -> &mut SingleLink {
        &mut node.0.link
    }

    fn element(link: &SingleLink) -> &BatchElem {
        let link_ptr = link as *const SingleLink;
        let batch_ptr = (link_ptr as usize - offset_of!(Batch, link)) as *const Batch;
        unsafe { BatchElem::ref_cast(&*batch_ptr) }
    }

    fn element_mut(link: &mut SingleLink) -> &mut BatchElem {
        let link_ptr = link as *mut SingleLink;
        let batch_ptr = (link_ptr as usize - offset_of!(Batch, link)) as *mut Batch;
        unsafe { BatchElem::mut_cast(&mut *batch_ptr) }
    }
}

/// A retired object.
struct Node {
    /// The pointer to the retired object.
    ptr: *mut (),

    /// The function used to reclaim the object.
    reclaim: unsafe fn(*mut (), Option<&Local>),

    /// The state of the retired object.
    state: NodeState,

    /// The batch that this node is a part of.
    batch: *mut Batch,
}

/// The state of a retired object.
#[repr(C)]
pub union NodeState {
    // While retiring: A temporary location for an active reservation list.
    head: *const AtomicPtr<Node>,

    // After retiring: The next node in the thread's reservation list.
    next: *mut Node,
}

impl Node {
    /// Represents an inactive thread.
    ///
    /// While null indicates an empty list, `INACTIVE` indicates the thread has
    /// no active guards and is not currently accessing any objects.
    pub const INACTIVE: *mut Node = usize::MAX as _;
}

/// A pointer to a batch, unique to the current Local.
pub struct LocalBatch {
    batch: *mut Batch,
}

impl Default for LocalBatch {
    fn default() -> Self {
        LocalBatch {
            batch: ptr::null_mut(),
        }
    }
}

impl LocalBatch {
    /// This is set during a call to `reclaim_all`, signalling recursive calls
    /// to retire to reclaim immediately.
    const DROP: *mut Batch = usize::MAX as _;

    /// Returns a pointer to the batch, initializing the batch if it was null.
    #[inline]
    fn get_or_init(&mut self, capacity: usize) -> *mut Batch {
        if self.batch.is_null() {
            self.batch = Box::into_raw(Box::new(Batch::new(capacity)));
        }

        self.batch
    }

    /// Free the batch.
    ///
    /// # Safety
    ///
    /// The safety requirements of `Box::from_raw` apply.
    #[inline]
    unsafe fn free(batch: *mut Batch) {
        // Safety: Guaranteed by caller.
        unsafe { drop(Box::from_raw(batch)) }
    }
}

// Safety: Any access to the batch owned by `LocalBatch` is unsafe.
unsafe impl Send for LocalBatch {}

/// A per-thread reservation list.
///
/// Reservation lists are lists of retired entries, where each entry represents
/// a batch.
#[repr(C)]
pub struct Reservation {
    /// The head of the list
    head: AtomicPtr<Node>,
}

// Safety: Reservations are only accessed by the owning Local, or synchronized
// through a lock.
unsafe impl Sync for Reservation {}

impl Default for Reservation {
    fn default() -> Self {
        Reservation {
            head: AtomicPtr::new(Node::INACTIVE),
        }
    }
}

/// Local state machine for a single participant in memory reclamation.
///
/// Each `Local` instance contains all the per-participant state needed for
/// the Hyaline memory reclamation algorithm:
/// - A local batch of retired objects
/// - A reservation list for tracking active batches
/// - A guard count for tracking nested guard usage
///
/// `Local` instances are stored in a lock-free linked list in the `Collector`.
#[repr(C)]
pub struct Local {
    /// Entry for the intrusive linked list in Collector.
    pub(crate) entry: Entry,

    /// Reference to the global Collector.
    collector: Collector,

    /// Local retirement batch.
    batch: CachePadded<UnsafeCell<LocalBatch>>,

    /// Sealed retirement batch.
    sealed_batch: CachePadded<UnsafeCell<LinkedList<BatchElem, SingleLink>>>,

    /// Reservation list.
    reservation: CachePadded<Reservation>,

    /// Number of active guards for this Local.
    guard_count: Cell<u64>,

    /// Number of handles referencing this Local.
    pub(crate) handle_count: Cell<u32>,
}

// Safety: Locals are only accessed by the owning thread, or access is
// synchronized through atomic operations.
unsafe impl Sync for Local {}

impl Local {
    /// Creates a new Local and registers it with the Collector.
    ///
    /// Returns a `LocalHandle` that can be used to create guards.
    pub fn register(collector: &Collector) -> LocalHandle {
        unsafe {
            let guard = unprotected();

            let local = Owned::new(Local {
                entry: Entry::default(),
                collector: collector.clone(),
                batch: CachePadded::new(UnsafeCell::new(LocalBatch::default())),
                sealed_batch: CachePadded::new(UnsafeCell::new(LinkedList::new())),
                reservation: CachePadded::new(Reservation::default()),
                guard_count: Cell::new(0),
                handle_count: Cell::new(1),
            })
            .into_shared(guard);

            collector.raw.locals.insert(local, guard);
            collector.raw.local_count.fetch_add(1, Ordering::Relaxed);

            LocalHandle {
                local: local.as_raw(),
            }
        }
    }

    /// Returns the collector this Local is registered with.
    #[inline]
    pub fn collector(&self) -> &Collector {
        &self.collector
    }

    /// Pin this `Local`, returning a guard that protects loads.
    ///
    /// `pin` calls maintain a local reference count to allow reentrancy. The
    /// first call to `pin` will mark the `Local` as active. If the current
    /// thread is marked as active, this method simply increments the
    /// reference count. The last `Guard` dropped will mark the `Local` as
    /// inactive.
    ///
    /// # Returns
    ///
    /// A `Guard` that protects loads while held.
    #[inline]
    pub fn pin(&self) -> Guard {
        let guards = self.guard_count.get();
        self.guard_count.set(guards + 1);

        if guards == 0 {
            self.enter();
        }

        Guard::new(self)
    }

    /// Returns true if this Local is currently pinned.
    #[inline]
    pub fn is_pinned(&self) -> bool {
        self.guard_count.get() > 0
    }

    /// Mark this Local as active.
    #[inline]
    fn enter(&self) {
        // Mark as active with null (empty list).
        self.reservation
            .head
            .store(ptr::null_mut(), membarrier::light_store());

        // Synchronize with the heavy barrier in `try_retire`.
        membarrier::light_barrier();
    }

    /// Mark this Local as inactive.
    ///
    /// # Safety
    ///
    /// Any previously protected pointers may be invalidated after calling this.
    #[inline]
    unsafe fn leave(&self) {
        // Release: Exit the critical section.
        let head = self
            .reservation
            .head
            .swap(Node::INACTIVE, Ordering::Release);

        if head != Node::INACTIVE {
            // Acquire any new entries in the reservation list.
            atomic::fence(Ordering::Acquire);

            // Decrement the reference counts of any batches that were retired.
            unsafe { self.traverse(head) }
        }
    }

    /// Clear the reservation list, keeping the thread marked as active.
    ///
    /// # Safety
    ///
    /// Any previously protected pointers may be invalidated after calling
    /// `leave`. Additionally, this method is not safe to call concurrently
    /// with the same reservation.
    #[inline]
    pub unsafe fn refresh(&self) {
        if self.guard_count.get() <= 1 {
            // SeqCst: Establish the ordering of a combined call to `leave` and `enter`.
            let head = self
                .reservation
                .head
                .swap(ptr::null_mut(), Ordering::SeqCst);

            if head != Node::INACTIVE {
                // Decrement the reference counts of any batches that were retired.
                unsafe { self.traverse(head) }
            }
        }
    }

    /// Called when a guard is dropped.
    #[inline]
    pub(crate) fn unpin(&self) {
        let guards = self.guard_count.get();
        self.guard_count.set(guards - 1);

        if guards <= 1 {
            // Safety: We have the last guard.
            unsafe { self.leave() };
        }
    }

    #[inline]
    pub(crate) fn repin(&self) {
        let guards = self.guard_count.get();

        if guards == 1 {
            // Safety: We have the only guard.
            unsafe { self.leave() };
            self.enter();
        }
    }

    /// Attempt to retire objects in the current `Local`'s batch.
    ///
    /// # Safety
    ///
    /// The current `Local` must have unique access to the batch.
    #[inline]
    pub(crate) unsafe fn try_retire_batch(&self, guard: &Guard) {
        // Safety: Guaranteed by caller.
        unsafe {
            self.seal_current_batch();
            self.try_retire_sealed(guard);
        };
    }

    /// Seal the current batch, moving it to the sealed list.
    ///
    /// # Safety
    ///
    /// The current `Local` must have unique access to the batch.
    #[inline]
    unsafe fn seal_current_batch(&self) {
        let local_batch = unsafe { &mut *self.batch.get() };
        let sealed = unsafe { &mut *self.sealed_batch.get() };
        if !local_batch.batch.is_null() {
            unsafe { sealed.push_back(BatchElem::ptr_cast(local_batch.batch)) };
            *local_batch = LocalBatch::default();
        }
    }

    /// Attempt to retire sealed batches.
    ///
    /// # Safety
    ///
    /// The current `Local` must have unique access to sealed batches.
    #[inline]
    unsafe fn try_retire_sealed(&self, guard: &Guard) {
        let sealed = self.sealed_batch.get();

        while let Some(sealed_batch) = unsafe { (*sealed).pop_front() } {
            // Safety: The caller guarantees that we have unique access to the batch, and we
            // are not holding on to any mutable references.
            unsafe {
                if !self.try_retire(sealed_batch as _, guard) {
                    // Failed to retire - re-add to the sealed list.
                    (*sealed).push_front(sealed_batch);
                    break;
                }
            }
        }
    }

    /// Attempt to retire objects in this batch.
    ///
    /// Note that if a guard on the current `Local` is active, the batch will
    /// also be added to the current reservation list for deferred
    /// reclamation.
    ///
    /// # Safety
    ///
    /// The current `Local` must have unique access to the provided batch.
    ///
    /// Additionally, the caller should not be holding on to any mutable
    /// references the the local batch, as they may be invalidated by
    /// recursive calls to `try_retire`.
    unsafe fn try_retire(&self, batch: *mut Batch, guard: &Guard) -> bool {
        // Establish a total order between the retirement of nodes in this batch and
        // light stores marking a thread as active:
        // - If the store comes first, we will see that the thread is active.
        // - If this barrier comes first, the thread will see the new values of any
        //   objects in this batch.
        //
        // This barrier also establishes synchronizes with the light store executed when
        // a thread is created:
        // - If our barrier comes first, they will see the new values of any objects in
        //   this batch.
        // - If their store comes first, we will see the new thread.
        membarrier::heavy();

        // There is nothing to retire.
        if batch.is_null() || batch == LocalBatch::DROP {
            return false;
        }

        // Safety: The caller guarantees we have unique access to the batch.
        let batch_entries = unsafe { (*batch).entries.as_mut_slice().as_mut_ptr() };

        let mut marked = 0;

        // Record all active threads, including the current thread.
        //
        // We need to do this in a separate step before actually retiring the batch to
        // ensure we have enough entries for reservation lists, as the number of
        // threads can grow dynamically.
        //
        // Safety: We only access `reservation.head`, which is an atomic pointer that is
        // sound to access from multiple threads.
        for local in self
            .collector()
            .raw
            .locals
            .iter(guard)
            .filter_map(|r| r.ok())
        {
            // If this thread is inactive, we can skip it. The heavy barrier above ensurse
            // that the next time it becomes active, it will see the new values
            // of any objects in this batch.
            //
            // Relaxed: See the Acquire fence below.
            if local.reservation.head.load(Ordering::Relaxed) == Node::INACTIVE {
                continue;
            }

            // If we don't have enough entries to insert into the reservation lists of all
            // active threads, try again later.
            //
            // Safety: The caller guarantees we have unique access to the batch.
            let Some(entry) = unsafe { &mut (*batch).entries }.get_mut(marked) else {
                return false;
            };

            // Temporarily store this reservation list in the batch.
            //
            // Safety: All nodes in a batch are valid and this batch has not yet been shared
            // to other threads.
            entry.state.head = &local.reservation.head;
            marked += 1;
        }

        // For any inactive threads we skipped above, synchronize with `leave` to ensure
        // any accesses happen-before we retire. We ensured with the heavy
        // barrier above that the thread will see the new values of any objects
        // in this batch the next time it becomes active.
        atomic::fence(Ordering::Acquire);

        let mut active = 0;

        // Add the batch to the reservation lists of any active `Local`s.
        'retire: for i in 0..marked {
            // Safety: The caller guarantees we have unique access to the batch, and we
            // ensure we have at least `marked` entries in the batch.
            let curr = unsafe { batch_entries.add(i) };

            // Safety: `curr` is a valid node in the batch, and we just initialized `head`
            // for all `marked` nodes in the previous loop.
            let head = unsafe { &*(*curr).state.head };

            // Relaxed: All writes to the `head` use RMW instructions, so the previous node
            // in the list is synchronized through the release sequence on
            // `head`.
            let mut prev = head.load(Ordering::Relaxed);

            loop {
                // The thread became inactive, skip it.
                //
                // As long as the thread became inactive at some point after the heavy barrier,
                // it can no longer access any objects in this batch. The next
                // time it becomes active it will load the new object values.
                if prev == Node::INACTIVE {
                    // Acquire: Synchronize with `leave` to ensure any accesses happen-before we
                    // retire.
                    atomic::fence(Ordering::Acquire);
                    continue 'retire;
                }

                // Link this node to the reservation list.
                unsafe { (*curr).state.next = prev };

                // Release: Ensure our access of the node, as well as the stores of new values
                // for any objects in the batch, are synchronized when this
                // thread calls `leave` and attempts to reclaim this batch.
                match head.compare_exchange_weak(prev, curr, Ordering::Release, Ordering::Relaxed) {
                    Ok(_) => break,
                    // Lost the race to another thread, retry.
                    Err(found) => prev = found,
                }
            }

            active += 1;
        }

        // Release: If we don't free the list, ensure our access of the batch is
        // synchronized with the thread that eventually will.
        //
        // Safety: The caller guarantees we have unique access to the batch.
        if unsafe { &*batch }
            .active
            .fetch_add(active, Ordering::Release)
            .wrapping_add(active)
            == 0
        {
            // Acquire: Ensure any access of objects in the batch, by threads that were
            // active and decremented the reference count, happen-before we free
            // it.
            atomic::fence(Ordering::Acquire);

            // Safety: The reference count is zero, meaning that either no threads were
            // active, or they have all already decremented the reference count.
            //
            // Additionally, the local batch has been reset and we are not holding on to any
            // mutable references, so any recursive calls to `retire` during
            // reclamation are valid.
            unsafe { self.free_batch(batch) }
        }

        true
    }

    /// Add a node to the retirement batch, retiring the batch if `batch_size`
    /// nodes are reached.
    ///
    /// # Safety
    ///
    /// The given pointer must no longer be accessible to any `Local` that
    /// enters after it is removed. It also cannot be accessed by the
    /// current `Local` after `add` is called.
    ///
    /// The pointer also be valid to pass to the provided reclaimer once it is
    /// safe to reclaim.
    ///
    /// Additionally, current `Local` must have unique access to the batch.
    #[inline]
    pub unsafe fn retire<T>(
        &self,
        ptr: *mut T,
        guard: &Guard,
        reclaim: unsafe fn(*mut T, Option<&Local>),
    ) {
        let local_batch = self.batch.get();

        // Safety: We have unique access to our own batch.
        let batch = unsafe { (*local_batch).get_or_init(self.collector.raw.batch_size) };

        // If we are in a recursive call during `drop` or `reclaim_all`, reclaim
        // immediately.
        if batch == LocalBatch::DROP {
            // Safety: `LocalBatch::DROP` means we have unique access to the collector.
            // Additionally, the caller guarantees that the pointer is valid for the
            // provided reclaimer.
            unsafe { reclaim(ptr, Some(self)) }
            return;
        }

        // Safety: `fn(*mut T) and fn(*mut U)` are ABI compatible if `T, U: Sized`.
        let reclaim: unsafe fn(*mut (), Option<&Local>) = unsafe { core::mem::transmute(reclaim) };

        // Safety: We have unique access to the batch.
        let len = unsafe {
            (*batch).entries.push(Node {
                batch,
                reclaim,
                ptr: ptr.cast::<()>(),
                state: NodeState { head: ptr::null() },
            });

            (*batch).entries.len()
        };

        // Attempt to seal the batch if we have enough entries.
        if len >= self.collector.raw.batch_size {
            unsafe { self.seal_current_batch() };
        }

        // Attempt to retire sealed batches.
        unsafe { self.try_retire_sealed(guard) };
    }

    /// Traverse the reservation list, decrementing the reference count of each
    /// batch.
    ///
    /// # Safety
    ///
    /// `list` must be a valid reservation list.
    #[cold]
    #[inline(never)]
    unsafe fn traverse(&self, mut list: *mut Node) {
        while !list.is_null() {
            let curr = list;

            // Advance the cursor.
            // Safety: `curr` is a valid, non-null node in the list.
            list = unsafe { (*curr).state.next };
            let batch = unsafe { (*curr).batch };

            // Safety: Batch pointers are valid for reads until they are reclaimed.
            unsafe {
                // Release: If we don't free the list, ensure our access of the batch is
                // synchronized with the thread that eventually will.
                if (*batch).active.fetch_sub(1, Ordering::Release) == 1 {
                    // Ensure any access of objects in the batch by other active threads
                    // happen-before we free it.
                    atomic::fence(Ordering::Acquire);

                    // Safety: We have the last reference to the batch and it has been removed from
                    // our reservation list.
                    self.free_batch(batch)
                }
            }
        }
    }

    /// Free a batch of objects.
    ///
    /// # Safety
    ///
    /// The batch reference count must be zero.
    #[inline]
    unsafe fn free_batch(&self, batch: *mut Batch) {
        for entry in unsafe { (*batch).entries.iter_mut() } {
            unsafe { (entry.reclaim)(entry.ptr.cast(), Some(self)) };
        }

        unsafe { LocalBatch::free(batch) };
    }

    /// Reclaim all values in the `Local`, including recursive calls to
    /// retire.
    ///
    /// # Safety
    ///
    /// No threads may be accessing the `Local` or any values that have been
    /// retired. This is equivalent to having a unique reference to the data
    /// structure containing the `Local`.
    #[inline]
    pub(super) unsafe fn reclaim(&self) {
        let local_batch = self.batch.get();

        // Safety: The caller guarantees we have unique access to the batch.
        let batch = unsafe { (*local_batch).batch };

        // Tell any recursive calls to `retire` to reclaim immediately.
        //
        // Safety: The caller guarantees we have unique access to the batch.
        unsafe { (*local_batch).batch = LocalBatch::DROP };

        if !batch.is_null() {
            // Safety: The caller guarantees we have unique access to the batch, and we
            // ensured it is non-null. Additionally, the local batch was reset
            // above, so the batch is inaccessible through recursive calls to
            // `retire`.
            unsafe { self.free_batch(batch) };
        }

        // Safety: The caller guarantees we have unique access to the sealed batches.
        while let Some(sealed_batch) = unsafe { (*self.sealed_batch.get()).pop_front() } {
            // Safety: The caller guarantees we have unique access to the batch, and we
            // ensured it is non-null. Additionally, the local batch was reset
            // above, so the batch is inaccessible through recursive calls to
            // `retire`.
            unsafe { self.free_batch(sealed_batch as _) };
        }

        // Reset the batch.
        //
        // Safety: The caller guarantees we have unique access to the batch.
        unsafe { (*local_batch).batch = ptr::null_mut() };
    }

    /// Reclaim all values that have been retired across all Locals.
    ///
    /// # Safety
    ///
    /// No threads may be accessing the collector or any values that have been
    /// retired. This is equivalent to having a unique reference to the data
    /// structure containing the collector.
    #[inline]
    pub unsafe fn reclaim_all(&self, guard: &Guard) {
        unsafe { self.collector.raw.reclaim_all(guard) };
    }
}

// Implement IsElement for Local so it can be stored in the intrusive list.
impl IsElement<Local> for Local {
    fn entry_of(local: &Local) -> &Entry {
        &local.entry
    }

    unsafe fn element_of(entry: &Entry) -> &Local {
        // Calculate the offset of `entry` within `Local` and subtract it.
        let entry_ptr = entry as *const Entry;
        let local_ptr = (entry_ptr as usize - offset_of!(Local, entry)) as *const Local;
        unsafe { &*local_ptr }
    }

    unsafe fn finalize(entry: &Entry, guard: &Guard) {
        // Safety: The delete process in `List` is atomic, ensuring no other threads can
        // access this `Local` after it was marked for deletion.
        unsafe {
            guard.defer_retire(entry as *const Entry as *mut Entry, |ptr, _collector| {
                let ptr = Shared::from(Self::element_of(&*ptr) as *const Local);
                drop(ptr.into_owned());
            });
        }
    }
}

impl Drop for Local {
    fn drop(&mut self) {
        // Safety: The drop of `Local` means no other references exist to it,
        // so we have unique access.
        unsafe { self.reclaim() };
    }
}
