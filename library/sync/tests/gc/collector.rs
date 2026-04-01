use std::{
    mem::ManuallyDrop,
    ptr,
    sync::{
        Arc,
        atomic::{AtomicPtr, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
};

use lazy_static::lazy_static;
use libakarin_sync::gc::{Collector, Guard, reclaim};

lazy_static! {
    static ref CPU_NUM: usize = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
}

#[test]
fn is_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Collector>();
}

struct DropTrack(Arc<AtomicUsize>);

impl Drop for DropTrack {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn boxed<T>(value: T) -> *mut T {
    Box::into_raw(Box::new(value))
}

struct UnsafeSend<T>(T);
unsafe impl<T> Send for UnsafeSend<T> {}

#[test]
fn single_thread() {
    let collector = Collector::new(*CPU_NUM).batch_size(2);
    let handle = collector.register();
    let dropped = Arc::new(AtomicUsize::new(0));

    // multiple of 2
    let items = cfg::ITEMS & !1;

    for _ in 0..items {
        let zero = AtomicPtr::new(boxed(DropTrack(dropped.clone())));

        {
            let guard = handle.pin();
            let _ = guard.protect(&zero, Ordering::Relaxed);
        }

        {
            let guard = handle.pin();
            let value = guard.protect(&zero, Ordering::Acquire);
            unsafe { guard.defer_retire(value, reclaim::boxed) }
        }
    }

    assert_eq!(dropped.load(Ordering::Relaxed), items);
}

#[test]
fn two_threads() {
    let collector = Collector::new(*CPU_NUM).batch_size(3);

    let a_dropped = Arc::new(AtomicUsize::new(0));
    let b_dropped = Arc::new(AtomicUsize::new(0));

    let (tx, rx) = mpsc::channel();

    let one = Arc::new(AtomicPtr::new(boxed(DropTrack(a_dropped.clone()))));

    let h = thread::spawn({
        let one = one.clone();
        let collector = collector.clone();

        move || {
            let handle = collector.register();
            let guard = handle.pin();
            let _value = guard.protect(&one, Ordering::Acquire);
            tx.send(()).unwrap();
            drop(guard);
            tx.send(()).unwrap();
        }
    });

    let handle = collector.register();

    for _ in 0..2 {
        let zero = AtomicPtr::new(boxed(DropTrack(b_dropped.clone())));
        let guard = handle.pin();
        let value = guard.protect(&zero, Ordering::Acquire);
        unsafe { guard.defer_retire(value, reclaim::boxed) }
    }

    rx.recv().unwrap(); // wait for thread to access value
    let guard = handle.pin();
    let value = guard.protect(&one, Ordering::Acquire);
    unsafe { guard.defer_retire(value, reclaim::boxed) }

    rx.recv().unwrap(); // wait for thread to drop guard
    h.join().unwrap();

    drop(guard);

    assert_eq!(
        (
            b_dropped.load(Ordering::Acquire),
            a_dropped.load(Ordering::Acquire)
        ),
        (2, 1)
    );
}

#[test]
fn refresh() {
    let collector = Collector::new(*CPU_NUM).batch_size(3);

    let items = (0..cfg::ITEMS)
        .map(|i| AtomicPtr::new(boxed(i)))
        .collect::<Arc<[_]>>();

    let handles = (0..cfg::THREADS)
        .map(|_| {
            thread::spawn({
                let items = items.clone();
                let collector = collector.clone();

                move || {
                    let handle = collector.register();
                    let mut guard = handle.pin();

                    for _ in 0..cfg::ITER {
                        for item in items.iter() {
                            let item = guard.protect(item, Ordering::Acquire);
                            unsafe { assert!(*item < cfg::ITEMS) }
                        }

                        guard.refresh();
                    }
                }
            })
        })
        .collect::<Vec<_>>();

    let handle = collector.register();
    for i in 0..cfg::ITER {
        for item in items.iter() {
            let old = item.swap(Box::into_raw(Box::new(i)), Ordering::AcqRel);
            let guard = handle.pin();
            unsafe { guard.defer_retire(old, reclaim::boxed) }
        }
    }

    for h in handles {
        h.join().unwrap()
    }

    // cleanup
    let guard = handle.pin();
    for item in items.iter() {
        let old = item.swap(ptr::null_mut(), Ordering::Acquire);
        unsafe { guard.defer_retire(old, reclaim::boxed) }
    }
}

#[test]
fn recursive_retire() {
    struct Recursive {
        _value: usize,
        pointers: Vec<*mut usize>,
    }

    let collector = Collector::new(*CPU_NUM).batch_size(1);
    let handle = collector.register();

    let ptr = boxed(Recursive {
        _value: 0,
        pointers: (0..cfg::ITEMS).map(boxed).collect(),
    });

    unsafe {
        let guard = handle.pin();
        guard.defer_retire(ptr, |ptr: *mut Recursive, collector| {
            let value = Box::from_raw(ptr);
            let collector = collector.expect("Local should be present");
            let mut guard = collector.pin();

            for pointer in value.pointers {
                collector.retire(pointer, &guard, reclaim::boxed);

                guard.flush();
                guard.refresh();
            }
        });

        handle.pin().flush();
    }
}

#[test]
fn reclaim_all() {
    let collector = Collector::new(*CPU_NUM).batch_size(2);
    let handle = collector.register();

    for _ in 0..cfg::ITER {
        let guard = handle.pin();
        let dropped = Arc::new(AtomicUsize::new(0));

        let items = (0..cfg::ITEMS)
            .map(|_| AtomicPtr::new(boxed(DropTrack(dropped.clone()))))
            .collect::<Vec<_>>();

        for item in items {
            unsafe { handle.retire(item.load(Ordering::Relaxed), &guard, reclaim::boxed) };
        }

        drop(guard);
        let guard = handle.pin();
        unsafe { collector.reclaim_all(&guard) };
        assert_eq!(dropped.load(Ordering::Relaxed), cfg::ITEMS);
    }
}

#[test]
fn recursive_retire_reclaim_all() {
    struct Recursive {
        _value: usize,
        pointers: Vec<*mut DropTrack>,
    }

    unsafe {
        let collector = Collector::new(*CPU_NUM).batch_size(cfg::ITEMS * 2);
        let dropped = Arc::new(AtomicUsize::new(0));

        let handle = collector.register();

        let ptr = boxed(Recursive {
            _value: 0,
            pointers: (0..cfg::ITEMS)
                .map(|_| boxed(DropTrack(dropped.clone())))
                .collect(),
        });

        let guard = handle.pin();
        handle.retire(ptr, &guard, |ptr: *mut Recursive, local| {
            let value = Box::from_raw(ptr);
            let local = local.expect("Local should be present");
            let guard = local.pin();
            for pointer in value.pointers {
                local.retire(pointer, &guard, reclaim::boxed);
            }
        });

        collector.reclaim_all(&guard);
        assert_eq!(dropped.load(Ordering::Relaxed), cfg::ITEMS);
    }
}

#[test]
fn defer_retire() {
    let collector = Collector::new(*CPU_NUM).batch_size(5);
    let handle = collector.register();
    let dropped = Arc::new(AtomicUsize::new(0));

    let objects: Vec<_> = (0..30).map(|_| boxed(DropTrack(dropped.clone()))).collect();

    let guard = handle.pin();

    for object in objects {
        unsafe { guard.defer_retire(object, reclaim::boxed) }
        guard.flush();
    }

    // guard is still active
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
    drop(guard);
    // now the objects should have been dropped
    assert_eq!(dropped.load(Ordering::Relaxed), 30);
}

#[test]
fn reentrant() {
    let collector = Collector::new(*CPU_NUM).batch_size(5);
    let handle = collector.register();
    let dropped = Arc::new(AtomicUsize::new(0));

    let objects: UnsafeSend<Vec<_>> =
        UnsafeSend((0..5).map(|_| boxed(DropTrack(dropped.clone()))).collect());

    assert_eq!(dropped.load(Ordering::Relaxed), 0);

    let guard1 = handle.pin();
    let guard2 = handle.pin();
    let guard3 = handle.pin();

    thread::spawn({
        let collector = collector.clone();

        move || {
            let handle = collector.register();
            let guard = handle.pin();
            for object in { objects }.0 {
                unsafe { guard.defer_retire(object, reclaim::boxed) }
            }
        }
    })
    .join()
    .unwrap();

    assert_eq!(dropped.load(Ordering::Relaxed), 0);
    drop(guard1);
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
    drop(guard2);
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
    drop(guard3);
    assert_eq!(dropped.load(Ordering::Relaxed), 5);

    let dropped = Arc::new(AtomicUsize::new(0));

    let objects: UnsafeSend<Vec<_>> =
        UnsafeSend((0..5).map(|_| boxed(DropTrack(dropped.clone()))).collect());

    assert_eq!(dropped.load(Ordering::Relaxed), 0);

    let mut guard1 = handle.pin();
    let mut guard2 = handle.pin();
    let mut guard3 = handle.pin();

    thread::spawn({
        let collector = collector.clone();

        move || {
            let handle = collector.register();
            let guard = handle.pin();
            for object in { objects }.0 {
                unsafe { guard.defer_retire(object, reclaim::boxed) }
            }
        }
    })
    .join()
    .unwrap();

    assert_eq!(dropped.load(Ordering::Relaxed), 0);
    guard1.refresh();
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
    drop(guard1);
    guard2.refresh();
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
    drop(guard2);
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
    guard3.refresh();
    assert_eq!(dropped.load(Ordering::Relaxed), 5);
}

#[test]
fn swap_stress() {
    for _ in 0..cfg::ITER {
        let collector = Collector::new(*CPU_NUM);
        let entries = [const { AtomicPtr::new(ptr::null_mut()) }; cfg::ITEMS];

        thread::scope(|s| {
            for _ in 0..cfg::THREADS {
                s.spawn(|| {
                    let handle = collector.register();
                    for i in 0..cfg::ITEMS {
                        let guard = handle.pin();
                        let new = Box::into_raw(Box::new(i));
                        let old = guard.swap(&entries[i], new, Ordering::AcqRel);
                        if !old.is_null() {
                            unsafe { assert_eq!(*old, i) }
                            unsafe { guard.defer_retire(old, reclaim::boxed) }
                        }
                    }
                });
            }
        });

        for i in 0..cfg::ITEMS {
            let val = entries[i].load(Ordering::Relaxed);
            let _ = unsafe { Box::from_raw(val) };
        }
    }
}

#[test]
fn cas_stress() {
    for _ in 0..cfg::ITER {
        let collector = Collector::new(*CPU_NUM);
        let entries = [const { AtomicPtr::new(ptr::null_mut()) }; cfg::ITEMS];

        thread::scope(|s| {
            for _ in 0..cfg::THREADS {
                s.spawn(|| {
                    let handle = collector.register();
                    for i in 0..cfg::ITEMS {
                        let guard = handle.pin();
                        let new = Box::into_raw(Box::new(i));

                        loop {
                            let old = entries[i].load(Ordering::Relaxed);

                            let result = guard.compare_exchange(
                                &entries[i],
                                old,
                                new,
                                Ordering::AcqRel,
                                Ordering::Relaxed,
                            );

                            let Ok(old) = result else {
                                continue;
                            };

                            if !old.is_null() {
                                unsafe { assert_eq!(*old, i) }
                                unsafe { guard.defer_retire(old, reclaim::boxed) }
                            }

                            break;
                        }
                    }
                });
            }
        });

        for i in 0..cfg::ITEMS {
            let val = entries[i].load(Ordering::Relaxed);
            let _ = unsafe { Box::from_raw(val) };
        }
    }
}

#[test]
fn collector_equality() {
    let a = Collector::new(*CPU_NUM);
    let b = Collector::new(*CPU_NUM);

    assert_eq!(a, a);
    assert_eq!(b, b);
    assert_ne!(a, b);

    let ha = a.register();
    let hb = b.register();

    assert_eq!(ha.pin().collector(), Some(&a));
    assert_ne!(ha.pin().collector(), Some(&b));

    assert_eq!(hb.pin().collector(), Some(&b));
    assert_ne!(hb.pin().collector(), Some(&a));
}

#[test]
fn stress() {
    // stress test with operation on a shared stack
    for _ in 0..cfg::ITER {
        let stack = Arc::new(Stack::new(1));

        thread::scope(|s| {
            let handle = stack.collector.register();
            for i in 0..cfg::ITEMS {
                stack.push(i, &handle.pin());
                stack.pop(&handle.pin());
            }

            for _ in 0..cfg::THREADS {
                let stack = stack.clone();
                s.spawn(move || {
                    let handle = stack.collector.register();
                    for i in 0..cfg::ITEMS {
                        stack.push(i, &handle.pin());
                        stack.pop(&handle.pin());
                    }
                });
            }
        });

        let handle = stack.collector.register();
        assert!(stack.pop(&handle.pin()).is_none());
        assert!(stack.is_empty());
    }
}

#[derive(Debug)]
pub struct Stack<T> {
    head: AtomicPtr<Node<T>>,
    collector: Collector,
}

#[derive(Debug)]
struct Node<T> {
    data: ManuallyDrop<T>,
    next: *mut Node<T>,
}

impl<T> Stack<T> {
    pub fn new(batch_size: usize) -> Stack<T> {
        Stack {
            head: AtomicPtr::new(ptr::null_mut()),
            collector: Collector::new(*CPU_NUM).batch_size(batch_size),
        }
    }

    pub fn push(&self, value: T, guard: &Guard) {
        let new = boxed(Node {
            data: ManuallyDrop::new(value),
            next: ptr::null_mut(),
        });

        loop {
            let head = guard.protect(&self.head, Ordering::Relaxed);
            unsafe { (*new).next = head }

            if self
                .head
                .compare_exchange(head, new, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
    }

    pub fn pop(&self, guard: &Guard) -> Option<T> {
        loop {
            let head = guard.protect(&self.head, Ordering::Acquire);

            if head.is_null() {
                return None;
            }

            let next = unsafe { (*head).next };

            if self
                .head
                .compare_exchange(head, next, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                unsafe {
                    let data = ptr::read(&(*head).data);
                    guard.defer_retire(head, reclaim::boxed);
                    return Some(ManuallyDrop::into_inner(data));
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.head.load(Ordering::Relaxed).is_null()
    }
}

impl<T> Drop for Stack<T> {
    fn drop(&mut self) {
        let handle = self.collector.register();
        let guard = handle.pin();
        while self.pop(&guard).is_some() {}
    }
}

#[cfg(miri)]
mod cfg {
    pub const THREADS: usize = 4;
    pub const ITEMS: usize = 100;
    pub const ITER: usize = 4;
}

#[cfg(not(miri))]
mod cfg {
    pub const THREADS: usize = 32;
    pub const ITEMS: usize = 10_000;
    pub const ITER: usize = 50;
}
