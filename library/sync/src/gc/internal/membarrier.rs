//! Memory barriers optimized for RCU, inspired by <https://github.com/jeehoonkang/membarrier-rs>.
//!
//! # Semantics
//!
//! There is a total order over all memory barriers provided by this module:
//! - Light store barriers, created by a pair of [`light_store`] and
//!   [`light_barrier`].
//! - Light load barriers, created by a pair of [`light_barrier`] and
//!   [`light_load`].
//! - Sequentially consistent barriers, or cumulative light barriers.
//! - Heavy barriers, created by [`heavy`].
//!
//! If thread A issues barrier X and thread B issues barrier Y and X occurs
//! before Y in the total order, X is ordered before Y with respect to coherence
//! only if either X or Y is a heavy barrier. In other words, there is no way to
//! establish an ordering between light barriers without the presence of a heavy
//! barrier.
#![allow(dead_code)]

use core::sync::atomic::{Ordering, fence};

/// The ordering for a store operation that synchronizes with heavy
/// barriers.
///
/// Must be followed by a light barrier.
#[inline]
pub fn light_store() -> Ordering {
    // Synchronize with `SeqCst` heavy barriers.
    Ordering::SeqCst
}

/// Issues a light memory barrier for a preceding store or subsequent load
/// operation.
#[inline]
pub fn light_barrier() {
    // This is a no-op due to strong loads and stores.
}

/// The ordering for a load operation that synchronizes with heavy barriers.
#[inline]
pub fn light_load() -> Ordering {
    // Participate in the total order established by light and heavy `SeqCst`
    // barriers.
    Ordering::SeqCst
}

/// Issues a heavy memory barrier for slow path that synchronizes with light
/// stores.
#[inline]
pub fn heavy() {
    // Synchronize with `SeqCst` light stores.
    fence(Ordering::SeqCst);
}
