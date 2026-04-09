use crate::twin::feature::{Command, CommandRequest, FsEventCommand, FsEventKind};
use anyhow::{Context, Result};
use futures::StreamExt;
use log::{debug, error, warn};
use std::{
    any::TypeId,
    collections::HashMap,
    ffi::c_int,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::mpsc;

const DEBOUNCE_DURATION: Duration = Duration::from_secs(2);

struct WatchInfo {
    feature_id: TypeId,
    path: PathBuf,
    kind: FsEventKind,
    oneshot: bool,
}

pub struct FsWatcher {
    inotify: Option<inotify::Inotify>,
    watches: Option<inotify::Watches>,
    watch_info: HashMap<c_int, WatchInfo>,
}

impl FsWatcher {
    pub fn new() -> Result<Self> {
        let inotify = inotify::Inotify::init().context("FsWatcher: failed to init inotify")?;
        let watches = inotify.watches();
        Ok(Self {
            inotify: Some(inotify),
            watches: Some(watches),
            watch_info: HashMap::new(),
        })
    }

    pub fn noop() -> Self {
        Self {
            inotify: None,
            watches: None,
            watch_info: HashMap::new(),
        }
    }

    pub fn add_watch<T: 'static>(
        &mut self,
        path: &Path,
        kind: FsEventKind,
        oneshot: bool,
    ) -> Result<()> {
        use inotify::WatchMask;

        let Some(watches) = &mut self.watches else {
            return Ok(());
        };

        let (watch_path, mask) = match kind {
            FsEventKind::FileCreated => {
                let parent = path
                    .parent()
                    .context("add_watch: FileCreated path has no parent")?;
                let mut mask = WatchMask::CREATE;
                if oneshot {
                    mask |= WatchMask::ONESHOT;
                }
                (parent, mask)
            }
            FsEventKind::FileModified => (path, WatchMask::CLOSE_WRITE),
            FsEventKind::DirModified => (path, WatchMask::CREATE | WatchMask::DELETE),
        };

        let wd = watches
            .add(watch_path, mask)
            .with_context(|| format!("add_watch: failed to watch {watch_path:?}"))?;

        debug!(
            "add_watch: {:?} on {watch_path:?} (target: {path:?}, oneshot: {oneshot})",
            wd.get_watch_descriptor_id()
        );

        self.watch_info.insert(
            wd.get_watch_descriptor_id(),
            WatchInfo {
                feature_id: TypeId::of::<T>(),
                path: path.to_path_buf(),
                kind,
                oneshot,
            },
        );

        Ok(())
    }

    pub fn into_stream(mut self, tx: mpsc::Sender<CommandRequest>) -> Result<()> {
        let Some(inotify) = self.inotify.take() else {
            return Ok(());
        };

        // Race-condition handling for FileCreated watches: if file already exists,
        // send the event immediately. Watch was set up on parent dir first, so any
        // creation between watch-setup and this check will also fire an inotify event
        // (duplicate is harmless).
        for info in self.watch_info.values() {
            if info.kind == FsEventKind::FileCreated && matches!(info.path.try_exists(), Ok(true)) {
                debug!(
                    "into_stream: file already exists {:?}, sending immediately",
                    info.path
                );
                let _ = tx.try_send(CommandRequest {
                    command: Command::FsEvent(FsEventCommand {
                        kind: FsEventKind::FileCreated,
                        feature_id: info.feature_id,
                        path: info.path.clone(),
                    }),
                    reply: None,
                });
            }
        }

        tokio::spawn(async move {
            let mut buffer = [0; 1024];
            let mut stream = inotify
                .into_event_stream(&mut buffer)
                .expect("into_stream: failed to create event stream");

            let mut debounce_deadlines: HashMap<c_int, tokio::time::Instant> = HashMap::new();

            loop {
                let next_deadline = debounce_deadlines
                    .values()
                    .min()
                    .copied()
                    .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(3600));

                tokio::select! {
                    biased;

                    event = stream.next() => {
                        let Some(event) = event else {
                            warn!("FsWatcher: inotify stream ended");
                            return;
                        };
                        let event = match event {
                            Ok(e) => e,
                            Err(e) => {
                                error!("FsWatcher: inotify error: {e:#}");
                                continue;
                            }
                        };

                        let wd_id = event.wd.get_watch_descriptor_id();
                        let Some(info) = self.watch_info.get(&wd_id) else {
                            debug!("FsWatcher: unknown wd {wd_id}");
                            continue;
                        };

                        if info.kind == FsEventKind::FileCreated {
                            let expected_name = info.path.file_name();
                            let event_name = event.name.as_deref();
                            if expected_name.is_none() || event_name != expected_name {
                                continue;
                            }
                        }

                        debug!("FsWatcher: event for {:?} ({:?})", info.path, info.kind);

                        if info.oneshot {
                            let _ = tx.send(CommandRequest {
                                command: Command::FsEvent(FsEventCommand {
                                    kind: info.kind.clone(),
                                    feature_id: info.feature_id,
                                    path: info.path.clone(),
                                }),
                                reply: None,
                            }).await;
                            self.watch_info.remove(&wd_id);
                        } else {
                            debounce_deadlines.insert(
                                wd_id,
                                tokio::time::Instant::now() + DEBOUNCE_DURATION,
                            );
                        }
                    }

                    _ = tokio::time::sleep_until(next_deadline), if !debounce_deadlines.is_empty() => {
                        let now = tokio::time::Instant::now();
                        let expired: Vec<c_int> = debounce_deadlines
                            .iter()
                            .filter(|(_, deadline)| **deadline <= now)
                            .map(|(wd_id, _)| *wd_id)
                            .collect();

                        for wd_id in expired {
                            debounce_deadlines.remove(&wd_id);
                            if let Some(info) = self.watch_info.get(&wd_id) {
                                debug!("FsWatcher: debounce elapsed for {:?}", info.path);
                                let _ = tx.send(CommandRequest {
                                    command: Command::FsEvent(FsEventCommand {
                                        kind: info.kind.clone(),
                                        feature_id: info.feature_id,
                                        path: info.path.clone(),
                                    }),
                                    reply: None,
                                }).await;
                            }
                        }
                    }
                }
            }
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tokio::time::{Duration, timeout};

    struct TestFeature;

    #[tokio::test]
    async fn add_watch_file_modified() {
        let tmp = tempfile::NamedTempFile::new().expect("create temp file");
        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .add_watch::<TestFeature>(tmp.path(), FsEventKind::FileModified, false)
            .expect("add_watch FileModified");
    }

    #[tokio::test]
    async fn add_watch_dir_modified() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .add_watch::<TestFeature>(tmp.path(), FsEventKind::DirModified, false)
            .expect("add_watch DirModified");
    }

    #[tokio::test]
    async fn add_watch_file_created() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let target = tmp.path().join("not-yet-existing.txt");
        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .add_watch::<TestFeature>(&target, FsEventKind::FileCreated, true)
            .expect("add_watch FileCreated");
    }

    #[tokio::test]
    async fn into_stream_file_modified() {
        let mut tmp = tempfile::NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        let (tx, mut rx) = mpsc::channel(16);

        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .add_watch::<TestFeature>(&path, FsEventKind::FileModified, false)
            .expect("add_watch");
        watcher.into_stream(tx).expect("into_stream");

        // Allow event loop to start
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Trigger CLOSE_WRITE
        writeln!(tmp, "hello").expect("write");
        tmp.flush().expect("flush");
        drop(tmp);

        // Wait for debounced event (2s debounce + margin)
        let req = timeout(Duration::from_secs(4), rx.recv())
            .await
            .expect("timeout waiting for event")
            .expect("channel closed");

        match req.command {
            Command::FsEvent(FsEventCommand {
                kind: FsEventKind::FileModified,
                ..
            }) => {}
            other => panic!("expected FsEvent(FileModified), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn into_stream_dir_modified() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let dir_path = tmp.path().to_path_buf();
        let (tx, mut rx) = mpsc::channel(16);

        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .add_watch::<TestFeature>(&dir_path, FsEventKind::DirModified, false)
            .expect("add_watch");
        watcher.into_stream(tx).expect("into_stream");

        tokio::time::sleep(Duration::from_millis(50)).await;

        std::fs::write(dir_path.join("new-file.txt"), "content").expect("create file");

        let req = timeout(Duration::from_secs(4), rx.recv())
            .await
            .expect("timeout waiting for event")
            .expect("channel closed");

        match req.command {
            Command::FsEvent(FsEventCommand {
                kind: FsEventKind::DirModified,
                ..
            }) => {}
            other => panic!("expected FsEvent(DirModified), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn into_stream_file_created_oneshot() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let target = tmp.path().join("will-appear.txt");
        let (tx, mut rx) = mpsc::channel(16);

        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .add_watch::<TestFeature>(&target, FsEventKind::FileCreated, true)
            .expect("add_watch");
        watcher.into_stream(tx).expect("into_stream");

        tokio::time::sleep(Duration::from_millis(50)).await;

        std::fs::write(&target, "created").expect("create file");

        let req = timeout(Duration::from_secs(4), rx.recv())
            .await
            .expect("timeout waiting for event")
            .expect("channel closed");

        match req.command {
            Command::FsEvent(FsEventCommand {
                kind: FsEventKind::FileCreated,
                ref path,
                ..
            }) => assert_eq!(path, &target),
            other => panic!("expected FsEvent(FileCreated), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn into_stream_file_created_race() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let target = tmp.path().join("already-exists.txt");

        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .add_watch::<TestFeature>(&target, FsEventKind::FileCreated, true)
            .expect("add_watch");

        // Create file BEFORE into_stream — race condition path
        std::fs::write(&target, "pre-existing").expect("create file");

        let (tx, mut rx) = mpsc::channel(16);
        watcher.into_stream(tx).expect("into_stream");

        // Should receive the event immediately (no debounce for race-detected oneshot)
        let req = timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("timeout: race condition event should arrive immediately")
            .expect("channel closed");

        match req.command {
            Command::FsEvent(FsEventCommand {
                kind: FsEventKind::FileCreated,
                ref path,
                ..
            }) => assert_eq!(path, &target),
            other => panic!("expected FsEvent(FileCreated), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn into_stream_debounce() {
        let tmp = tempfile::NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        let (tx, mut rx) = mpsc::channel(16);

        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .add_watch::<TestFeature>(&path, FsEventKind::FileModified, false)
            .expect("add_watch");
        watcher.into_stream(tx).expect("into_stream");

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Write 3 times in quick succession (each open+write+close triggers CLOSE_WRITE)
        for i in 0..3 {
            std::fs::write(&path, format!("write {i}")).expect("write");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // First event should arrive after debounce (~2s after last write)
        let req = timeout(Duration::from_secs(4), rx.recv())
            .await
            .expect("timeout waiting for debounced event")
            .expect("channel closed");

        match req.command {
            Command::FsEvent(FsEventCommand {
                kind: FsEventKind::FileModified,
                ..
            }) => {}
            other => panic!("expected FsEvent(FileModified), got {other:?}"),
        }

        // No second event should arrive (debounce coalesced all three writes)
        let second = timeout(Duration::from_secs(3), rx.recv()).await;
        assert!(
            second.is_err(),
            "expected no second event, but got one (debounce failed)"
        );
    }

    #[tokio::test]
    async fn noop_does_nothing() {
        let (tx, mut rx) = mpsc::channel(16);
        let mut watcher = FsWatcher::noop();

        watcher
            .add_watch::<TestFeature>(Path::new("/nonexistent"), FsEventKind::FileModified, false)
            .expect("noop add_watch should succeed");
        watcher
            .into_stream(tx)
            .expect("noop into_stream should succeed");

        // tx is consumed and dropped (no task spawned), so rx.recv() returns None
        assert!(
            rx.recv().await.is_none(),
            "noop watcher should produce no events"
        );
    }
}
