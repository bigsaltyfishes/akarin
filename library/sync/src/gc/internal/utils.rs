#![allow(dead_code)]

use alloc::{boxed::Box, vec::Vec};
use core::mem::MaybeUninit;

pub struct FixedVec<T> {
    entries: Box<[T]>,
    len: usize,
}

impl<T> FixedVec<T> {
    /// Create a new fixed-size vector with the specified capacity.
    pub fn new(capacity: usize) -> Self {
        let vec = Vec::from_iter(
            (0..capacity).map(|_| unsafe { MaybeUninit::<T>::uninit().assume_init() }),
        );
        FixedVec {
            entries: vec.into_boxed_slice(),
            len: 0,
        }
    }

    /// Get the current length of the vector.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Get the capacity of the vector.
    pub fn capacity(&self) -> usize {
        self.entries.len()
    }

    /// Push a new element into the vector.
    pub fn push(&mut self, value: T) {
        if self.len >= self.capacity() {
            panic!("FixedVec capacity exceeded");
        }
        self.entries[self.len] = value;
        self.len += 1;
    }

    /// Get a reference to an element at the specified index.
    pub fn get(&self, index: usize) -> Option<&T> {
        if index < self.len {
            Some(&self.entries[index])
        } else {
            None
        }
    }

    /// Get a mutable reference to an element at the specified index.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        if index < self.len {
            Some(&mut self.entries[index])
        } else {
            None
        }
    }

    /// Clear the vector.
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Get a slice of the current elements in the vector.
    pub fn as_slice(&self) -> &[T] {
        &self.entries[..self.len]
    }

    /// Get a mutable slice of the current elements in the vector.
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.entries[..self.len]
    }

    /// Remove a element at the specified index.
    pub fn remove(&mut self, index: usize) -> T {
        if index >= self.len {
            panic!("Index out of bounds");
        }
        let value = core::mem::replace(&mut self.entries[index], unsafe {
            MaybeUninit::<T>::uninit().assume_init()
        });
        self.entries[index..self.len].rotate_left(1);
        self.len -= 1;
        value
    }

    /// Iterate over the elements in the vector.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.as_slice().iter()
    }

    /// Iterate mutably over the elements in the vector.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.as_mut_slice().iter_mut()
    }
}
