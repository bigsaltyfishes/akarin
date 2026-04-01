use std::{
    sync::{Arc, Barrier},
    thread,
};

use criterion::{Criterion, criterion_group, criterion_main};

const THREADS: usize = 16;
const ITEMS: usize = 1000;

fn treiber_stack(c: &mut Criterion) {
    c.bench_function("trieber_stack-haphazard", |b| {
        b.iter(run::<haphazard_stack::TreiberStack<usize>>)
    });

    c.bench_function("trieber_stack-crossbeam", |b| {
        b.iter(run::<crossbeam_stack::TreiberStack<usize>>)
    });

    c.bench_function("trieber_stack-libakarin_sync::gc", |b| {
        b.iter(run::<seize_stack::TreiberStack<usize>>)
    });
}

trait Stack<T> {
    fn new() -> Self;
    fn push(&self, value: T);
    fn pop(&self) -> Option<T>;
    fn is_empty(&self) -> bool;
}

fn run<T>()
where
    T: Stack<usize> + Send + Sync + 'static,
{
    let stack = Arc::new(T::new());
    let barrier = Arc::new(Barrier::new(THREADS));

    let handles = (0..THREADS - 1)
        .map(|_| {
            let stack = stack.clone();
            let barrier = barrier.clone();

            thread::spawn(move || {
                barrier.wait();
                for i in 0..ITEMS {
                    stack.push(i);
                    assert!(stack.pop().is_some());
                }
            })
        })
        .collect::<Vec<_>>();

    barrier.wait();
    for i in 0..ITEMS {
        stack.push(i);
        assert!(stack.pop().is_some());
    }

    for handle in handles {
        handle.join().unwrap();
    }

    assert!(stack.pop().is_none());
    assert!(stack.is_empty());
}

criterion_group!(benches, treiber_stack);
criterion_main!(benches);

mod seize_stack {
    use std::{
        mem::ManuallyDrop,
        ptr::{self, NonNull},
        sync::atomic::{AtomicPtr, Ordering},
    };

    use lazy_static::lazy_static;
    use libakarin_sync::gc::{Collector, reclaim};

    use super::Stack;

    lazy_static! {
        static ref CPU_NUM: usize = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
    }

    lazy_static! {
        static ref COLLECTOR: Collector = Collector::new(*CPU_NUM).batch_size(32);
    }

    thread_local! {
        static LOCAL_HANDLE: libakarin_sync::gc::LocalHandle = COLLECTOR.register();
    }

    #[derive(Debug)]
    pub struct TreiberStack<T> {
        head: AtomicPtr<Node<T>>,
    }

    #[derive(Debug)]
    struct Node<T> {
        data: ManuallyDrop<T>,
        next: *mut Node<T>,
    }

    impl<T> Stack<T> for TreiberStack<T> {
        fn new() -> TreiberStack<T> {
            TreiberStack {
                head: AtomicPtr::new(ptr::null_mut()),
            }
        }

        fn push(&self, value: T) {
            let node = Box::into_raw(Box::new(Node {
                data: ManuallyDrop::new(value),
                next: ptr::null_mut(),
            }));

            let guard = LOCAL_HANDLE.with(|handle| handle.pin());

            loop {
                let head = guard.protect(&self.head, Ordering::Relaxed);
                unsafe { (*node).next = head }

                if self
                    .head
                    .compare_exchange(head, node, Ordering::Release, Ordering::Relaxed)
                    .is_ok()
                {
                    break;
                }
            }
        }

        fn pop(&self) -> Option<T> {
            let guard = LOCAL_HANDLE.with(|handle| handle.pin());

            loop {
                let head = NonNull::new(guard.protect(&self.head, Ordering::Acquire))?.as_ptr();

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

        fn is_empty(&self) -> bool {
            self.head.load(Ordering::Relaxed).is_null()
        }
    }

    impl<T> Drop for TreiberStack<T> {
        fn drop(&mut self) {
            while self.pop().is_some() {}
        }
    }
}

mod haphazard_stack {
    use std::{
        mem::ManuallyDrop,
        ptr,
        sync::atomic::{AtomicPtr, Ordering},
    };

    use haphazard::{Domain, HazardPointer};

    use super::Stack;

    #[derive(Debug)]
    pub struct TreiberStack<T: 'static> {
        head: AtomicPtr<Node<T>>,
    }

    #[derive(Debug)]
    struct Node<T> {
        data: ManuallyDrop<T>,
        next: *mut Node<T>,
    }

    unsafe impl<T> Send for Node<T> {}
    unsafe impl<T> Sync for Node<T> {}

    impl<T> Stack<T> for TreiberStack<T> {
        fn new() -> TreiberStack<T> {
            TreiberStack {
                head: AtomicPtr::default(),
            }
        }

        fn push(&self, value: T) {
            let node = Box::into_raw(Box::new(Node {
                data: ManuallyDrop::new(value),
                next: ptr::null_mut(),
            }));

            let mut h = HazardPointer::new();

            loop {
                let head = match h.protect_ptr(&self.head) {
                    Some((ptr, _)) => ptr.as_ptr(),
                    None => ptr::null_mut(),
                };

                unsafe { (*node).next = head }

                if self
                    .head
                    .compare_exchange(head, node, Ordering::Release, Ordering::Relaxed)
                    .is_ok()
                {
                    break;
                }
            }
        }

        fn pop(&self) -> Option<T> {
            let mut h = HazardPointer::new();

            loop {
                let (head, _) = h.protect_ptr(&self.head)?;
                let next = unsafe { head.as_ref().next };

                if self
                    .head
                    .compare_exchange(head.as_ptr(), next, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    unsafe {
                        let data = ptr::read(&head.as_ref().data);
                        Domain::global().retire_ptr::<_, Box<Node<T>>>(head.as_ptr());
                        return Some(ManuallyDrop::into_inner(data));
                    }
                }
            }
        }

        fn is_empty(&self) -> bool {
            let mut h = HazardPointer::new();
            unsafe { h.protect(&self.head) }.is_none()
        }
    }

    impl<T> Drop for TreiberStack<T> {
        fn drop(&mut self) {
            while self.pop().is_some() {}
        }
    }
}

mod crossbeam_stack {
    use std::{mem::ManuallyDrop, ptr, sync::atomic::Ordering};

    use crossbeam_epoch::{Atomic, Owned, Shared};

    use super::Stack;

    #[derive(Debug)]
    pub struct TreiberStack<T> {
        head: Atomic<Node<T>>,
    }

    unsafe impl<T: Send> Send for TreiberStack<T> {}
    unsafe impl<T: Sync> Sync for TreiberStack<T> {}

    #[derive(Debug)]
    struct Node<T> {
        data: ManuallyDrop<T>,
        next: *const Node<T>,
    }

    impl<T> Stack<T> for TreiberStack<T> {
        fn new() -> TreiberStack<T> {
            TreiberStack {
                head: Atomic::null(),
            }
        }

        fn push(&self, value: T) {
            let guard = crossbeam_epoch::pin();

            let mut node = Owned::new(Node {
                data: ManuallyDrop::new(value),
                next: ptr::null_mut(),
            });

            loop {
                let head = self.head.load(Ordering::Relaxed, &guard);
                node.next = head.as_raw();

                match self.head.compare_exchange(
                    head,
                    node,
                    Ordering::Release,
                    Ordering::Relaxed,
                    &guard,
                ) {
                    Ok(_) => break,
                    Err(err) => node = err.new,
                }
            }
        }

        fn pop(&self) -> Option<T> {
            let guard = crossbeam_epoch::pin();

            loop {
                let head = self.head.load(Ordering::Acquire, &guard);

                if head.is_null() {
                    return None;
                }

                let next = unsafe { head.deref().next };

                if self
                    .head
                    .compare_exchange(
                        head,
                        Shared::from(next),
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                        &guard,
                    )
                    .is_ok()
                {
                    unsafe {
                        let data = ptr::read(&head.deref().data);
                        guard.defer_destroy(head);
                        return Some(ManuallyDrop::into_inner(data));
                    }
                }
            }
        }

        fn is_empty(&self) -> bool {
            let guard = crossbeam_epoch::pin();
            self.head.load(Ordering::Relaxed, &guard).is_null()
        }
    }

    impl<T> Drop for TreiberStack<T> {
        fn drop(&mut self) {
            while self.pop().is_some() {}
        }
    }
}
