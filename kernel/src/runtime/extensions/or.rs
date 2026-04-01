use alloc::{boxed::Box, vec::Vec};
use core::{
    pin::Pin,
    task::{Context, Poll},
};

use pin_project::pin_project;

#[pin_project]
pub struct Or<A, B, R>
where
    A: Future<Output = R>,
    B: Future<Output = R>,
{
    #[pin]
    a: A,
    #[pin]
    b: B,
}

impl<A, B, R> Future for Or<A, B, R>
where
    A: Future<Output = R>,
    B: Future<Output = R>,
{
    type Output = R;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match this.a.poll(cx) {
            Poll::Ready(r) => Poll::Ready(r),
            Poll::Pending => this.b.poll(cx),
        }
    }
}

pub trait OrExt: Future + Sized {
    fn or<B>(self, other: B) -> Or<Self, B, Self::Output>
    where
        B: Future<Output = Self::Output>,
    {
        Or { a: self, b: other }
    }
}

impl<F: Future> OrExt for F {}

#[pin_project]
pub struct OrMany<R> {
    #[pin]
    futures: Vec<Pin<Box<dyn Future<Output = R>>>>,
}

impl<R> Future for OrMany<R> {
    type Output = R;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        for future in this.futures.iter_mut() {
            if let Poll::Ready(r) = future.as_mut().poll(cx) {
                return Poll::Ready(r);
            }
        }
        Poll::Pending
    }
}

impl<R> OrMany<R> {
    pub fn new() -> Self {
        Self {
            futures: Vec::new(),
        }
    }

    pub fn push<F>(&mut self, future: F)
    where
        F: Future<Output = R> + 'static,
    {
        self.futures.push(Box::pin(future));
    }
}

pub trait OrManyExt: Future + Sized + 'static {
    fn or_many(
        self,
        others: Vec<Pin<Box<dyn Future<Output = Self::Output>>>>,
    ) -> OrMany<Self::Output> {
        let mut or_many = OrMany::new();
        or_many.push(self);
        for other in others {
            or_many.push(other);
        }

        or_many
    }
}

impl<F> OrManyExt for F where F: Future + Sized + 'static {}
