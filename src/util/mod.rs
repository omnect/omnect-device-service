use futures::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::time::{Instant, Interval};

#[derive(Debug)]
pub struct IntervalStream {
    inner: Option<Interval>,
}

impl IntervalStream {
    pub fn new(interval: Option<Interval>) -> Self {
        Self { inner: interval }
    }
}

impl Stream for IntervalStream {
    type Item = Instant;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Instant>> {
        if let Some(i) = self.inner.as_mut() {
            i.poll_tick(cx).map(Some)
        } else {
            Poll::Pending
        }
    }
}
