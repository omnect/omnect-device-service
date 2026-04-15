use crate::twin::feature::{Command, CommandRequest, FAR_FUTURE, FsEventCommand, FsEventKind};
use anyhow::{Context, Result, ensure};
use futures::StreamExt;
use log::{debug, error, warn};
use std::{
    any::TypeId,
    collections::{HashMap, HashSet},
    ffi::c_int,
    path::{Path, PathBuf},
};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

const DEBOUNCE_DURATION: std::time::Duration = std::time::Duration::from_secs(2);
const INOTIFY_EVENT_BUF_LEN: usize = 4096;
const MAX_CONSECUTIVE_INOTIFY_ERRORS: u32 = 10;

struct WatchInfo {
    feature_id: TypeId,
    path: PathBuf,
    kind: FsEventKind,
    oneshot: bool,
}

pub struct FsWatcher {
    inotify: Option<inotify::Inotify>,
    watches: Option<inotify::Watches>,
    watch_info: HashMap<c_int, (inotify::WatchDescriptor, Vec<WatchInfo>)>,
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

    #[cfg(any(test, feature = "mock"))]
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
                // FileCreated watches the parent dir and filters by filename —
                // non-oneshot would cause spurious events for unrelated files
                // in the debounce path, which dispatches without filename checks.
                ensure!(oneshot, "add_watch: FileCreated requires oneshot=true");
                let parent = path
                    .parent()
                    .context("add_watch: FileCreated path has no parent")?;
                // Include MOVED_TO so files that appear via atomic rename/move
                // are treated as created. Oneshot is enforced in user-space by
                // the event loop (kernel ONESHOT would be consumed by any event
                // in the parent dir before our target filename is seen).
                (
                    parent,
                    WatchMask::CREATE | WatchMask::MOVED_TO | WatchMask::MASK_ADD,
                )
            }
            FsEventKind::FileModified => (path, WatchMask::CLOSE_WRITE | WatchMask::MASK_ADD),
            FsEventKind::DirModified => (
                path,
                WatchMask::CREATE | WatchMask::DELETE | WatchMask::MASK_ADD,
            ),
        };

        let wd = watches
            .add(watch_path, mask)
            .with_context(|| format!("add_watch: failed to watch {watch_path:?}"))?;

        let wd_id = wd.get_watch_descriptor_id();
        debug!("add_watch: {wd_id:?} on {watch_path:?} (target: {path:?}, oneshot: {oneshot})");

        let info = WatchInfo {
            feature_id: TypeId::of::<T>(),
            path: path.to_path_buf(),
            kind,
            oneshot,
        };

        match self.watch_info.entry(wd_id) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert((wd, vec![info]));
            }
            std::collections::hash_map::Entry::Occupied(mut e) => {
                e.get_mut().1.push(info);
            }
        }

        Ok(())
    }

    pub fn into_stream(
        mut self,
        tx: mpsc::Sender<CommandRequest>,
        cancel: CancellationToken,
    ) -> Result<Option<JoinHandle<()>>> {
        let Some(inotify) = self.inotify.take() else {
            return Ok(None);
        };

        let handle = tokio::spawn(async move {
            // Race-condition handling for FileCreated watches: if file already
            // exists, send the event immediately with guaranteed delivery (.await).
            // Collect dispatched paths to remove their entries in a single pass
            // (avoids a second try_exists check that could race with deletion).
            let mut dispatched: HashSet<PathBuf> = HashSet::new();
            for (_, infos) in self.watch_info.values() {
                for info in infos {
                    if info.kind != FsEventKind::FileCreated {
                        continue;
                    }
                    match info.path.try_exists() {
                        Ok(false) => continue,
                        Err(e) => {
                            warn!("into_stream: cannot check {:?}: {e}", info.path);
                            continue;
                        }
                        Ok(true) => {}
                    }
                    debug!(
                        "into_stream: file already exists {:?}, sending immediately",
                        info.path
                    );
                    if let Err(e) = tx
                        .send(CommandRequest {
                            command: Command::FsEvent(FsEventCommand {
                                kind: FsEventKind::FileCreated,
                                feature_id: info.feature_id,
                                path: info.path.clone(),
                            }),
                            reply: None,
                        })
                        .await
                    {
                        error!("into_stream: receiver dropped during race-condition check: {e}");
                        return;
                    }
                    dispatched.insert(info.path.clone());
                }
            }

            // Remove dispatched oneshot entries and clean up empty kernel watches
            for (_, infos) in self.watch_info.values_mut() {
                infos.retain(|info| !(info.oneshot && dispatched.contains(&info.path)));
            }
            let empty_wds: Vec<c_int> = self
                .watch_info
                .iter()
                .filter(|(_, (_, infos))| infos.is_empty())
                .map(|(wd_id, _)| *wd_id)
                .collect();
            for wd_id in empty_wds {
                if let Some((wd, _)) = self.watch_info.remove(&wd_id)
                    && let Some(watches) = &mut self.watches
                    && let Err(e) = watches.remove(wd)
                {
                    debug!("into_stream: failed to remove kernel watch: {e}");
                }
            }
            let mut buffer = [0; INOTIFY_EVENT_BUF_LEN];
            let mut stream = match inotify.into_event_stream(&mut buffer) {
                Ok(stream) => stream,
                Err(e) => {
                    error!("into_stream: failed to create event stream: {e}");
                    return;
                }
            };

            // Debounce is keyed by wd_id, not by individual WatchInfo entry.
            // Multiple non-oneshot watches sharing the same inotify descriptor
            // (e.g. two features watching the same path) share a single deadline.
            let mut debounce_deadlines: HashMap<c_int, tokio::time::Instant> = HashMap::new();
            let mut consecutive_inotify_errors: u32 = 0;

            loop {
                let next_deadline = debounce_deadlines
                    .values()
                    .min()
                    .copied()
                    .unwrap_or_else(|| tokio::time::Instant::now() + FAR_FUTURE);

                tokio::select! {
                    biased;

                    _ = cancel.cancelled() => return,

                    event = stream.next() => {
                        let Some(event) = event else {
                            error!("FsWatcher: inotify stream ended, all file watches are now inactive");
                            return;
                        };
                        let event = match event {
                            Ok(e) => {
                                consecutive_inotify_errors = 0;
                                e
                            }
                            Err(e) => {
                                consecutive_inotify_errors += 1;
                                error!(
                                    "FsWatcher: inotify error ({consecutive_inotify_errors}/{MAX_CONSECUTIVE_INOTIFY_ERRORS}): {e:#}"
                                );
                                if consecutive_inotify_errors >= MAX_CONSECUTIVE_INOTIFY_ERRORS {
                                    error!("FsWatcher: too many consecutive errors, stopping");
                                    return;
                                }
                                continue;
                            }
                        };

                        let wd_id = event.wd.get_watch_descriptor_id();
                        let Some((_, infos)) = self.watch_info.get(&wd_id) else {
                            debug!("FsWatcher: unknown wd {wd_id}");
                            continue;
                        };

                        // Find the matching entry index by kind-specific criteria
                        let matched_idx = infos.iter().position(|info| {
                            if info.kind == FsEventKind::FileCreated {
                                let expected = info.path.file_name();
                                let actual = event.name.as_deref();
                                expected.is_some() && actual == expected
                            } else {
                                true
                            }
                        });

                        let Some(idx) = matched_idx else {
                            continue;
                        };

                        let kind = infos[idx].kind;
                        let feature_id = infos[idx].feature_id;
                        let path = infos[idx].path.clone();
                        let oneshot = infos[idx].oneshot;

                        debug!("FsWatcher: event for {path:?} ({kind:?})");

                        if oneshot {
                            if tx.send(CommandRequest {
                                command: Command::FsEvent(FsEventCommand {
                                    kind,
                                    feature_id,
                                    path,
                                }),
                                reply: None,
                            }).await.is_err() {
                                warn!("FsWatcher: receiver dropped, stopping");
                                return;
                            }

                            let Some((_, infos)) = self.watch_info.get_mut(&wd_id) else {
                                error!("FsWatcher: wd {wd_id} disappeared unexpectedly");
                                continue;
                            };
                            infos.remove(idx);
                            if infos.is_empty()
                                && let Some((wd, _)) = self.watch_info.remove(&wd_id)
                                && let Some(watches) = &mut self.watches
                                && let Err(e) = watches.remove(wd)
                            {
                                debug!("FsWatcher: failed to remove kernel watch: {e}");
                            }
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
                            if let Some((_, infos)) = self.watch_info.get(&wd_id) {
                                for info in infos {
                                    debug!("FsWatcher: debounce elapsed for {:?}", info.path);
                                    if tx.send(CommandRequest {
                                        command: Command::FsEvent(FsEventCommand {
                                            kind: info.kind,
                                            feature_id: info.feature_id,
                                            path: info.path.clone(),
                                        }),
                                        reply: None,
                                    }).await.is_err() {
                                        warn!("FsWatcher: receiver dropped, stopping");
                                        return;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });

        Ok(Some(handle))
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
        watcher
            .into_stream(tx, CancellationToken::new())
            .expect("into_stream");

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
        watcher
            .into_stream(tx, CancellationToken::new())
            .expect("into_stream");

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
        watcher
            .into_stream(tx, CancellationToken::new())
            .expect("into_stream");

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
        watcher
            .into_stream(tx, CancellationToken::new())
            .expect("into_stream");

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
        watcher
            .into_stream(tx, CancellationToken::new())
            .expect("into_stream");

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
    async fn file_created_oneshot_ignores_unrelated_files() {
        // Regression: kernel ONESHOT on the parent dir would be consumed by any
        // CREATE event, not just the target filename. With user-space oneshot,
        // unrelated files must not prevent the target from being detected.
        let tmp = tempfile::tempdir().expect("create temp dir");
        let target = tmp.path().join("target.txt");
        let (tx, mut rx) = mpsc::channel(16);

        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .add_watch::<TestFeature>(&target, FsEventKind::FileCreated, true)
            .expect("add_watch");
        watcher
            .into_stream(tx, CancellationToken::new())
            .expect("into_stream");

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Create an unrelated file first — must NOT consume the watch
        std::fs::write(tmp.path().join("unrelated.txt"), "noise").expect("create unrelated");
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Now create the target file
        std::fs::write(&target, "found").expect("create target");

        let req = timeout(Duration::from_secs(4), rx.recv())
            .await
            .expect("timeout: target event should arrive despite unrelated file")
            .expect("channel closed");

        match req.command {
            Command::FsEvent(FsEventCommand {
                kind: FsEventKind::FileCreated,
                ref path,
                ..
            }) => assert_eq!(path, &target),
            other => panic!("expected FsEvent(FileCreated) for target, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn file_created_race_no_duplicate() {
        // Regression: after into_stream dispatches an immediate event for a
        // pre-existing file, the watch_info must be cleaned up so the inotify
        // watch (still active on parent dir) doesn't produce a duplicate event.
        let tmp = tempfile::tempdir().expect("create temp dir");
        let target = tmp.path().join("exists.txt");

        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .add_watch::<TestFeature>(&target, FsEventKind::FileCreated, true)
            .expect("add_watch");

        // File exists before into_stream
        std::fs::write(&target, "pre-existing").expect("create file");

        let (tx, mut rx) = mpsc::channel(16);
        watcher
            .into_stream(tx, CancellationToken::new())
            .expect("into_stream");

        // First event: immediate dispatch from race-condition handling
        let req = timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("timeout: immediate event expected")
            .expect("channel closed");

        match req.command {
            Command::FsEvent(FsEventCommand {
                kind: FsEventKind::FileCreated,
                ..
            }) => {}
            other => panic!("expected FsEvent(FileCreated), got {other:?}"),
        }

        // No duplicate event should arrive (watch_info was cleaned up)
        let second = timeout(Duration::from_secs(1), rx.recv()).await;
        assert!(
            second.is_err(),
            "expected no duplicate event after race-condition dispatch"
        );
    }

    #[tokio::test]
    async fn file_created_multiple_targets_same_dir() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let target_a = tmp.path().join("a.txt");
        let target_b = tmp.path().join("b.txt");
        let (tx, mut rx) = mpsc::channel(16);

        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .add_watch::<TestFeature>(&target_a, FsEventKind::FileCreated, true)
            .expect("watch a");
        watcher
            .add_watch::<TestFeature>(&target_b, FsEventKind::FileCreated, true)
            .expect("watch b");
        watcher
            .into_stream(tx, CancellationToken::new())
            .expect("into_stream");

        tokio::time::sleep(Duration::from_millis(50)).await;

        std::fs::write(&target_a, "a").expect("create a");
        std::fs::write(&target_b, "b").expect("create b");

        let mut paths = Vec::new();
        for _ in 0..2 {
            let req = timeout(Duration::from_secs(4), rx.recv())
                .await
                .expect("timeout")
                .expect("channel closed");
            match req.command {
                Command::FsEvent(FsEventCommand {
                    kind: FsEventKind::FileCreated,
                    path,
                    ..
                }) => {
                    paths.push(path);
                }
                other => panic!("expected FileCreated, got {other:?}"),
            }
        }
        paths.sort();
        assert_eq!(paths, vec![target_a, target_b]);
    }

    #[tokio::test]
    async fn file_created_race_cleans_up_kernel_watch() {
        // Regression: after race-path dispatch for a pre-existing file, the
        // kernel watch on the parent dir must be removed. Otherwise, creating
        // new files in the same dir generates inotify events for a stale wd
        // with no watch_info entries.
        let tmp = tempfile::tempdir().expect("create temp dir");
        let target = tmp.path().join("pre-existing.txt");

        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .add_watch::<TestFeature>(&target, FsEventKind::FileCreated, true)
            .expect("add_watch");

        // File exists before into_stream
        std::fs::write(&target, "already here").expect("create file");

        let (tx, mut rx) = mpsc::channel(16);
        watcher
            .into_stream(tx, CancellationToken::new())
            .expect("into_stream");

        // Consume the immediate race-path event
        let _ = timeout(Duration::from_millis(500), rx.recv())
            .await
            .expect("timeout: immediate event expected")
            .expect("channel closed");

        // Create another file in the same parent dir — if the kernel watch
        // was not cleaned up, this would trigger an inotify event for the
        // stale wd. With proper cleanup, no event should arrive.
        std::fs::write(tmp.path().join("other.txt"), "noise").expect("create other file");

        let spurious = timeout(Duration::from_secs(1), rx.recv()).await;
        assert!(
            spurious.is_err(),
            "expected no event after kernel watch cleanup, but got one"
        );
    }

    #[tokio::test]
    async fn noop_does_nothing() {
        let (tx, mut rx) = mpsc::channel(16);
        let mut watcher = FsWatcher::noop();

        watcher
            .add_watch::<TestFeature>(Path::new("/nonexistent"), FsEventKind::FileModified, false)
            .expect("noop add_watch should succeed");
        let handle = watcher
            .into_stream(tx, CancellationToken::new())
            .expect("noop into_stream should succeed");

        assert!(handle.is_none(), "noop should not spawn a task");

        // tx is consumed and dropped (no task spawned), so rx.recv() returns None
        assert!(
            rx.recv().await.is_none(),
            "noop watcher should produce no events"
        );
    }

    #[tokio::test]
    async fn add_watch_file_created_rejects_non_oneshot() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let target = tmp.path().join("target.txt");
        let mut watcher = FsWatcher::new().expect("FsWatcher::new");

        let result = watcher.add_watch::<TestFeature>(&target, FsEventKind::FileCreated, false);
        assert!(
            result.is_err(),
            "FileCreated with oneshot=false must be rejected"
        );
    }

    #[tokio::test]
    async fn cancellation_stops_event_loop() {
        let tmp = tempfile::NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        let (tx, mut rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();

        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .add_watch::<TestFeature>(&path, FsEventKind::FileModified, false)
            .expect("add_watch");
        let handle = watcher
            .into_stream(tx, cancel.clone())
            .expect("into_stream")
            .expect("should spawn a task");

        tokio::time::sleep(Duration::from_millis(50)).await;

        cancel.cancel();

        // The spawned task should exit, completing the JoinHandle
        let result = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(result.is_ok(), "task should exit after cancellation");

        // Channel should close (tx dropped by the exiting task)
        assert!(
            rx.recv().await.is_none(),
            "channel should close after cancellation"
        );
    }

    #[tokio::test]
    async fn file_created_via_rename() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let target = tmp.path().join("atomically-created.txt");
        let (tx, mut rx) = mpsc::channel(16);

        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .add_watch::<TestFeature>(&target, FsEventKind::FileCreated, true)
            .expect("add_watch");
        watcher
            .into_stream(tx, CancellationToken::new())
            .expect("into_stream");

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Create via atomic rename (tempfile + rename), which produces MOVED_TO
        let staging = tmp.path().join(".staging.tmp");
        std::fs::write(&staging, "atomic content").expect("write staging");
        std::fs::rename(&staging, &target).expect("rename");

        let req = timeout(Duration::from_secs(4), rx.recv())
            .await
            .expect("timeout: MOVED_TO event should be detected")
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
}
