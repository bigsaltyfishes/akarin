use core::{
    pin::Pin,
    task::{Context, Poll},
};

use libakarin_core::clock::time::Duration;
use pin_project::pin_project;

use crate::{Sleep, sched::time::timer};

#[pin_project]
pub struct Timeout<F> {
    #[pin]
    future: F,
    #[pin]
    sleep: Sleep,
}

impl<F> Future for Timeout<F>
where
    F: Future,
{
    type Output = Option<F::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        if let Poll::Ready(output) = this.future.poll(cx) {
            return Poll::Ready(Some(output));
        }
        if let Poll::Ready(()) = this.sleep.poll(cx) {
            return Poll::Ready(None);
        }
        Poll::Pending
    }
}

pub trait TimeoutExt: Future + Sized {
    fn timeout(self, duration: Duration) -> Timeout<Self> {
        Timeout {
            future: self,
            sleep: timer().sleep(duration),
        }
    }
}

impl<F> TimeoutExt for F where F: Future {}
