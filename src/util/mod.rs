use anyhow::{ensure, Context, Result};
use futures::Stream;
use log::{debug, error};
use notify::{Config, PollWatcher, RecursiveMode, Watcher};
use std::any::TypeId;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;
use tokio::{
    sync::mpsc,
    time::{Instant, Interval},
};

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

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<TypeId>> {
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

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Instant>> {
        if let Some(i) = self.inner.as_mut() {
            i.poll_tick(cx).map(Some)
        } else {
            Poll::Pending
        }
    }
}

#[derive(Debug)]
pub struct FileCreatedStream {
    watcher: PollWatcher,
    done: bool,
    rx: mpsc::Receiver<PathBuf>,
    id: TypeId,
}

impl FileCreatedStream {
    pub fn new(file_path: &Path, id: TypeId) -> Result<Self> {
        let dir_path = file_path
            .parent()
            .context(format!("cannot get parent of: {file_path:?}"))?;
        ensure!(dir_path.is_dir(), "directory doesn't exist: {dir_path:?}");
        let done = false;
        let (tx, rx) = mpsc::channel(1);
        let config = Config::default()
            .with_compare_contents(true)
            .with_poll_interval(Duration::from_millis(500));
        let file_path_inner = file_path.to_path_buf();
        let dir_path_inner = dir_path.to_path_buf();

        debug!("wait for {file_path:?} to be created");

        let mut watcher = PollWatcher::new(
            move |res| match res {
                Ok(event) => {
                    debug!("event_handler event: {:?}", event);
                    if matches!(file_path_inner.try_exists(), Ok(true)) {
                        if let Err(e) = tx.blocking_send(dir_path_inner.to_path_buf()) {
                            error!("event_handler send result: {e}")
                        }
                    }
                }
                Err(e) => error!("event_handler watch error: {:?}", e),
            },
            config,
        )
        .context("create PollWatcher")?;

        watcher
            .watch(dir_path, RecursiveMode::Recursive)
            .context(format!("watch: {dir_path:?}"))?;

        Ok(Self {
            watcher,
            done,
            rx,
            id,
        })
    }
}

impl Stream for FileCreatedStream {
    type Item = TypeId;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<TypeId>> {
        if !self.done {
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some(path)) => {
                    self.done = true;
                    if let Err(e) = self.watcher.unwatch(&path) {
                        error!("poll_next: unwatch {e}");
                    }
                    Poll::Ready(Some(self.id))
                }
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            }
        } else {
            Poll::Pending
        }
    }
}
