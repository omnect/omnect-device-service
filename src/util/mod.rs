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
    pub fn new(path: &Path, id: TypeId) -> Result<Self> {
        //ensure!(path.is_file(), "path is not a file: {path:?}");
        let parent = path
            .parent()
            .context(format!("cannot get parent of: {path:?}"))?;
        let done = false;
        let (tx, rx) = mpsc::channel(1);
        let config = Config::default()
            .with_compare_contents(true)
            .with_poll_interval(Duration::from_millis(500));
        let p = path.to_path_buf();

        debug!("wait for {path:?} to be created");

        let mut watcher = PollWatcher::new(
            move |res| match res {
                Ok(event) => {
                    debug!("event: {:?}", event);
                    if matches!(p.try_exists(), Ok(true)) {
                        if let Err(e) = tx.blocking_send(p.clone()) {
                            error!("event_handler send result: {e}")
                        }
                    }
                }
                Err(e) => error!("watch error: {:?}", e),
            },
            config,
        )
        .context("create PollWatcher")?;

        watcher
            .watch(parent, RecursiveMode::Recursive)
            .context(format!("watch: {parent:?}"))?;

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
