#![allow(private_bounds)]

use core::{
    fmt::Debug,
    ptr::{self, NonNull},
};

/// A single-linked intrusive list link.
///
/// This link can be embedded within a struct to allow it to be part of
/// a single-linked intrusive list.
#[derive(Debug)]
pub struct SingleLink {
    next: *mut Self,
}

impl SingleLink {
    /// Creates a new `SingleLink`.
    pub const fn new() -> Self {
        SingleLink {
            next: ptr::null_mut(),
        }
    }

    /// Returns the next node in the list.
    pub fn next(&self) -> Option<NonNull<Self>> {
        NonNull::new(self.next)
    }

    /// Sets the next node in the list.
    pub fn set_next(&mut self, next: Option<NonNull<Self>>) {
        self.next = next.map_or(ptr::null_mut(), |n| n.as_ptr());
    }
}

unsafe impl Send for SingleLink {}
unsafe impl Sync for SingleLink {}

/// A Double-linked intrusive list link.
///
/// This link can be embedded within a struct to allow it to be part of
/// a double-linked intrusive list.
#[derive(Debug)]
pub struct DoubleLink {
    prev: *mut Self,
    next: *mut Self,
}

impl DoubleLink {
    /// Creates a new `DoubleLink`.
    pub const fn new() -> Self {
        DoubleLink {
            prev: ptr::null_mut(),
            next: ptr::null_mut(),
        }
    }

    /// Returns the previous node in the list.
    pub fn prev(&self) -> Option<NonNull<Self>> {
        NonNull::new(self.prev)
    }

    /// Returns the next node in the list.
    pub fn next(&self) -> Option<NonNull<Self>> {
        NonNull::new(self.next)
    }

    pub unsafe fn detach<T>(&mut self, list: &mut LinkedList<T, Self>)
    where
        T: ElememtOf<T, Self>,
    {
        if let Some(mut prev) = self.prev() {
            unsafe {
                prev.as_mut().set_next(self.next());

                if let Some(mut next) = self.next() {
                    let next = next.as_mut();
                    next.set_prev(self.prev());
                } else {
                    list.tail = prev.as_ptr();
                }
            }

            list.count -= 1;
        } else {
            // This is the head node
            unsafe {
                list.pop_front();
            }
        }
    }

    /// Sets the previous node in the list.
    pub fn set_prev(&mut self, prev: Option<NonNull<Self>>) {
        self.prev = prev.map_or(ptr::null_mut(), |n| n.as_ptr());
    }

    /// Sets the next node in the list.
    pub fn set_next(&mut self, next: Option<NonNull<Self>>) {
        self.next = next.map_or(ptr::null_mut(), |n| n.as_ptr());
    }
}

unsafe impl Send for DoubleLink {}
unsafe impl Sync for DoubleLink {}

/// A helper trait for identifying intrusive links.
trait Link {
    fn next(&self) -> Option<NonNull<Self>>;
}

impl Link for SingleLink {
    fn next(&self) -> Option<NonNull<Self>> {
        self.next()
    }
}
impl Link for DoubleLink {
    fn next(&self) -> Option<NonNull<Self>> {
        self.next()
    }
}

/// A trait for associating an intrusive link with its containing element.
pub trait ElememtOf<T, L: Link> {
    fn link(node: &T) -> &L;
    fn link_mut(node: &mut T) -> &mut L;
    fn element(link: &L) -> &T;
    fn element_mut(link: &mut L) -> &mut T;
}

/// An intrusive linked list.
pub struct LinkedList<T, L>
where
    T: ElememtOf<T, L>,
    L: Link,
{
    head: *mut L,
    tail: *mut L,
    count: usize,
    _marker: core::marker::PhantomData<T>,
}

impl<T, L> LinkedList<T, L>
where
    T: ElememtOf<T, L>,
    L: Link,
{
    /// Creates a new empty linked list.
    pub const fn new() -> Self {
        LinkedList {
            head: ptr::null_mut(),
            tail: ptr::null_mut(),
            count: 0,
            _marker: core::marker::PhantomData,
        }
    }

    /// Returns the number of elements in the list.
    pub fn len(&self) -> usize {
        self.count
    }

    /// Returns true if the list is empty.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Returns a reference to the head element of the list.
    pub unsafe fn head(&self) -> Option<*const T> {
        if self.head.is_null() {
            None
        } else {
            unsafe {
                let link = &*self.head;
                Some(T::element(link))
            }
        }
    }

    /// Returns a mutable reference to the head element of the list.
    pub unsafe fn head_mut(&mut self) -> Option<*mut T> {
        if self.head.is_null() {
            None
        } else {
            unsafe {
                let link = &mut *self.head;
                Some(T::element_mut(link))
            }
        }
    }

    /// Returns a reference to the tail element of the list.
    pub unsafe fn tail(&self) -> Option<*const T> {
        if self.tail.is_null() {
            None
        } else {
            unsafe {
                let link = &*self.tail;
                Some(T::element(link))
            }
        }
    }

    /// Returns a mutable reference to the tail element of the list.
    pub unsafe fn tail_mut(&mut self) -> Option<*mut T> {
        if self.tail.is_null() {
            None
        } else {
            unsafe {
                let link = &mut *self.tail;
                Some(T::element_mut(link))
            }
        }
    }
}

impl<T> LinkedList<T, SingleLink>
where
    T: ElememtOf<T, SingleLink>,
{
    /// Pushes an element to the front of the list.
    pub unsafe fn push_front(&mut self, element: *mut T) {
        let elem = unsafe { &mut *element };
        let link = T::link_mut(elem);
        link.set_next(NonNull::new(self.head));

        self.head = link as *mut SingleLink;

        if self.tail.is_null() {
            self.tail = link as *mut SingleLink;
        }

        self.count += 1;
    }

    /// Pops an element from the front of the list.
    pub unsafe fn pop_front(&mut self) -> Option<*mut T> {
        if self.head.is_null() {
            return None;
        }

        unsafe {
            let head_link = &mut *self.head;
            let next_link = head_link.next();

            let element = T::element_mut(head_link);

            self.head = next_link.map_or(ptr::null_mut(), |n| n.as_ptr());

            if self.head.is_null() {
                self.tail = ptr::null_mut();
            }

            self.count -= 1;

            Some(element)
        }
    }

    /// Push an element to the back of the list.
    pub unsafe fn push_back(&mut self, element: *mut T) {
        let elem = unsafe { &mut *element };
        let link = T::link_mut(elem);
        link.set_next(None);

        if self.tail.is_null() {
            self.head = link as *mut SingleLink;
            self.tail = link as *mut SingleLink;
        } else {
            unsafe {
                let tail_link = &mut *self.tail;
                tail_link.set_next(NonNull::new(link as *mut SingleLink));
            }
            self.tail = link as *mut SingleLink;
        }

        self.count += 1;
    }
}

impl<T> LinkedList<T, DoubleLink>
where
    T: ElememtOf<T, DoubleLink>,
{
    /// Pushes an element to the front of the list.
    pub unsafe fn push_front(&mut self, element: *mut T) {
        let elem = unsafe { &mut *element };
        let link = T::link_mut(elem);
        link.set_next(NonNull::new(self.head));
        link.set_prev(None);

        if !self.head.is_null() {
            unsafe {
                let head_link = &mut *self.head;
                head_link.set_prev(NonNull::new(link as *mut DoubleLink));
            }
        } else {
            self.tail = link as *mut DoubleLink;
        }

        self.head = link as *mut DoubleLink;

        self.count += 1;
    }

    /// Pops an element from the front of the list.
    pub unsafe fn pop_front(&mut self) -> Option<*mut T> {
        if self.head.is_null() {
            return None;
        }

        unsafe {
            let head_link = &mut *self.head;
            let next_link = head_link.next();

            let element = T::element_mut(head_link);

            self.head = next_link.map_or(ptr::null_mut(), |n| n.as_ptr());

            if !self.head.is_null() {
                let new_head_link = &mut *self.head;
                new_head_link.set_prev(None);
            } else {
                self.tail = ptr::null_mut();
            }

            self.count -= 1;

            Some(element)
        }
    }

    /// Push an element to the back of the list.
    pub unsafe fn push_back(&mut self, element: *mut T) {
        let elem = unsafe { &mut *element };
        let link = T::link_mut(elem);
        link.set_prev(NonNull::new(self.tail));
        link.set_next(None);

        if !self.tail.is_null() {
            unsafe {
                let tail_link = &mut *self.tail;
                tail_link.set_next(NonNull::new(link as *mut DoubleLink));
            }
        } else {
            self.head = link as *mut DoubleLink;
        }

        self.tail = link as *mut DoubleLink;

        self.count += 1;
    }

    /// Pops an element from the back of the list.
    pub unsafe fn pop_back(&mut self) -> Option<*mut T> {
        if self.tail.is_null() {
            return None;
        }

        unsafe {
            let tail_link = &mut *self.tail;
            let prev_link = tail_link.prev();

            let element = T::element_mut(tail_link);

            self.tail = prev_link.map_or(ptr::null_mut(), |n| n.as_ptr());

            if !self.tail.is_null() {
                let new_tail_link = &mut *self.tail;
                new_tail_link.set_next(None);
            } else {
                self.head = ptr::null_mut();
            }

            self.count -= 1;

            Some(element)
        }
    }

    /// Returns an iterator over the list.
    pub unsafe fn iter(&self) -> ListIterator<'_, T, DoubleLink> {
        ListIterator {
            current: self.head,
            _marker: core::marker::PhantomData,
        }
    }

    /// Returns a mutable iterator over the list.
    pub unsafe fn iter_mut(&mut self) -> ListIteratorMut<'_, T, DoubleLink> {
        ListIteratorMut {
            current: self.head,
            _marker: core::marker::PhantomData,
        }
    }
}

impl<T, L> Debug for LinkedList<T, L>
where
    T: ElememtOf<T, L>,
    L: Link,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LinkedList")
            .field("count", &self.count)
            .finish()
    }
}

/// An iterator over an intrusive linked list.
pub struct ListIterator<'a, T, L>
where
    T: ElememtOf<T, L>,
    L: Link + 'a,
{
    current: *mut L,
    _marker: core::marker::PhantomData<&'a T>,
}

impl<'a, T, L> Iterator for ListIterator<'a, T, L>
where
    T: ElememtOf<T, L>,
    L: Link + 'a,
{
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current.is_null() {
            return None;
        }

        unsafe {
            let link = &*self.current;
            let element = T::element(link);
            self.current = match link.next() {
                Some(n) => n.as_ptr(),
                None => ptr::null_mut(),
            };
            Some(element)
        }
    }
}

/// A mutable iterator over an intrusive linked list.
pub struct ListIteratorMut<'a, T, L>
where
    T: ElememtOf<T, L>,
    L: Link + 'a,
{
    current: *mut L,
    _marker: core::marker::PhantomData<&'a mut T>,
}

impl<'a, T, L> Iterator for ListIteratorMut<'a, T, L>
where
    T: ElememtOf<T, L>,
    L: Link + 'a,
{
    type Item = &'a mut T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current.is_null() {
            return None;
        }

        unsafe {
            let link = &mut *self.current;
            self.current = match link.next() {
                Some(n) => n.as_ptr(),
                None => ptr::null_mut(),
            };

            let element = T::element_mut(link);
            Some(element)
        }
    }
}

unsafe impl<T, L> Send for LinkedList<T, L>
where
    T: ElememtOf<T, L> + Send,
    L: Link,
{
}

unsafe impl<T, L> Sync for LinkedList<T, L>
where
    T: ElememtOf<T, L> + Sync,
    L: Link,
{
}
