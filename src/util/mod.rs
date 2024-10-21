use futures::Stream;
use futures::StreamExt;
use std::any::TypeId;
use std::path::Path;
use std::pin::Pin;
use std::time::Duration;
use tokio::{
    sync::mpsc,
    time::{Instant, Interval},
};

pub type TypeIdStream = Pin<Box<dyn Stream<Item = TypeId> + Send>>;

pub fn interval_stream_option(interval: Option<Interval>) -> impl Stream {
    match interval {
        None => futures_util::stream::empty::<Instant>().boxed(),
        Some(interval) => tokio_stream::wrappers::IntervalStream::new(interval).boxed(),
    }
}

pub fn interval_stream_type_id<T>(
    interval: Interval,
) -> Pin<Box<dyn futures_util::Stream<Item = TypeId> + std::marker::Send>>
where
    T: 'static,
{
    tokio_stream::wrappers::IntervalStream::new(interval)
        .map(|_| TypeId::of::<T>())
        .boxed()
}

pub fn file_created_stream_type_id<T>(
    file_path: &Path,
) -> Pin<Box<dyn futures_util::Stream<Item = TypeId> + std::marker::Send>>
where
    T: 'static,
{
    let (tx, rx) = mpsc::channel(2);
    let file_path_inner = file_path.to_path_buf();

    tokio::task::spawn_blocking(move || loop {
        if matches!(file_path_inner.try_exists(), Ok(true)) {
            tx.blocking_send(()).unwrap();
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    });

    tokio_stream::wrappers::ReceiverStream::new(rx)
        .map(|_| TypeId::of::<T>())
        .boxed()
}
