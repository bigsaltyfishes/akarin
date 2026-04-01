//! Strategy helpers for event listeners.
//!
//! Source reference:
//! - https://github.com/smol-rs/event-listener-strategy.git
//!
//! This module currently provides the minimal strategy surface required by
//! kernel IPC/channel code.

use super::Listener;

pub trait Strategy {
    fn wait(&mut self, listener: Listener<'_>);
}

#[derive(Default)]
pub struct Blocking;

impl Strategy for Blocking {
    fn wait(&mut self, listener: Listener<'_>) {
        listener.wait_blocking();
    }
}

#[derive(Default)]
pub struct NonBlocking;

impl Strategy for NonBlocking {
    fn wait(&mut self, _listener: Listener<'_>) {}
}
