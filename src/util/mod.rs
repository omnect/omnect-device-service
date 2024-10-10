use futures::Stream;
use std::any::TypeId;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::time::{Instant, Interval};

#[derive(Debug)]
pub struct IntervalStreamTypeId {
    inner: Interval,
    id: TypeId,
}

impl IntervalStreamTypeId {
    pub fn new(interval: Interval, id: TypeId) -> Self {
        Self {
            inner: interval,
            id,
        }
    }
}

impl Stream for IntervalStreamTypeId {
    type Item = TypeId;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<TypeId>> {
        self.inner.poll_tick(cx).map(|_| Some(self.id))
    }
}

#[derive(Debug)]
pub struct IntervalStreamOption {
    inner: Option<Interval>,
}

impl IntervalStreamOption {
    pub fn new(interval: Option<Interval>) -> Self {
        Self { inner: interval }
    }
}

impl Stream for IntervalStreamOption {
    type Item = Instant;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Instant>> {
        if let Some(i) = self.inner.as_mut() {
            i.poll_tick(cx).map(Some)
        } else {
            Poll::Pending
        }
    }
}
