use crate::twin::feature::{
    COMMAND_EVENT_DEBOUNCE, Command, CommandRequest, FsEventCommand, FsEventKind,
};
use anyhow::{Context, Result};
use futures::StreamExt;
use inotify::{WatchDescriptor, WatchMask};
use log::{debug, error, warn};
use std::{
    any::TypeId,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

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
    // `WatchDescriptor` implements `Eq`/`Hash`; using it as the key rather than
    // the raw `c_int` id documents the invariant that all descriptors come
    // from the same `Inotify` and removes the redundant `(wd, …)` value tuple.
    watch_info: HashMap<WatchDescriptor, Vec<WatchInfo>>,
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

    /// Watch a file for `CLOSE_WRITE` events (coalesced into one
    /// `FsEventKind::FileModified` command per debounce window).
    pub fn watch_file_modified<T: 'static>(&mut self, path: &Path) -> Result<()> {
        self.insert_watch::<T>(
            path,
            path,
            WatchMask::CLOSE_WRITE | WatchMask::MASK_ADD,
            FsEventKind::FileModified,
            false,
        )
    }

    /// Watch a directory for entry additions and deletions (coalesced into one
    /// `FsEventKind::DirModified` command per debounce window).
    pub fn watch_dir_modified<T: 'static>(&mut self, path: &Path) -> Result<()> {
        self.insert_watch::<T>(
            path,
            path,
            WatchMask::CREATE | WatchMask::DELETE | WatchMask::MASK_ADD,
            FsEventKind::DirModified,
            false,
        )
    }

    /// Watch for the appearance of a specific file (including atomic rename
    /// via `MOVED_TO`). Fires once, then the watch is removed.
    ///
    /// The watch is placed on the parent directory and filtered by filename in
    /// the event loop — kernel `ONESHOT` cannot be used because it would be
    /// consumed by any event in the parent dir before our target is seen.
    pub fn watch_file_created_oneshot<T: 'static>(&mut self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .context("watch_file_created_oneshot: path has no parent")?;
        self.insert_watch::<T>(
            parent,
            path,
            WatchMask::CREATE | WatchMask::MOVED_TO | WatchMask::MASK_ADD,
            FsEventKind::FileCreated,
            true,
        )
    }

    fn insert_watch<T: 'static>(
        &mut self,
        watch_path: &Path,
        target_path: &Path,
        mask: WatchMask,
        kind: FsEventKind,
        oneshot: bool,
    ) -> Result<()> {
        let Some(watches) = &mut self.watches else {
            return Ok(());
        };

        let wd = watches
            .add(watch_path, mask)
            .with_context(|| format!("insert_watch: failed to watch {watch_path:?}"))?;

        debug!(
            "insert_watch: {wd:?} on {watch_path:?} (target: {target_path:?}, oneshot: {oneshot})"
        );

        let info = WatchInfo {
            feature_id: TypeId::of::<T>(),
            path: target_path.to_path_buf(),
            kind,
            oneshot,
        };

        self.watch_info.entry(wd).or_default().push(info);

        Ok(())
    }

    /// Drop the kernel watch for `wd` if its user-space entry list is empty.
    /// No-op when the entry is missing, still populated, or the inotify backend
    /// is the noop variant.
    fn remove_kernel_watch_if_empty(&mut self, wd: &WatchDescriptor) {
        let is_empty = self
            .watch_info
            .get(wd)
            .is_some_and(|infos| infos.is_empty());
        if !is_empty {
            return;
        }
        let Some((wd, _)) = self.watch_info.remove_entry(wd) else {
            return;
        };
        let Some(watches) = self.watches.as_mut() else {
            return;
        };
        if let Err(e) = watches.remove(wd) {
            debug!("FsWatcher: failed to remove kernel watch: {e}");
        }
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
            // Race-condition handling for FileCreated watches: if the file
            // already exists at subscription time, send the event immediately
            // (with guaranteed delivery via .await) rather than relying on a
            // future inotify event that will never fire.
            //
            // Race: between try_exists() returning true and tx.send completing,
            // the file could be unlinked. In that case the oneshot watch is
            // still removed below, and a subsequent re-creation would be
            // missed. Acceptable in practice because the watched paths are
            // append-only state markers, not scratch files.
            //
            // Dispatched paths are tracked so their WatchInfo entries can be
            // removed in a single pass, avoiding a second try_exists call that
            // could itself race with deletion.
            let mut dispatched: HashSet<PathBuf> = HashSet::new();
            for infos in self.watch_info.values() {
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
            for infos in self.watch_info.values_mut() {
                infos.retain(|info| !(info.oneshot && dispatched.contains(&info.path)));
            }
            let wds: Vec<WatchDescriptor> = self.watch_info.keys().cloned().collect();
            for wd in &wds {
                self.remove_kernel_watch_if_empty(wd);
            }
            let mut buffer = [0; INOTIFY_EVENT_BUF_LEN];
            let mut stream = match inotify.into_event_stream(&mut buffer) {
                Ok(stream) => stream,
                Err(e) => {
                    error!("into_stream: failed to create event stream: {e}");
                    return;
                }
            };

            // Debounce is keyed by `WatchDescriptor`, not by individual
            // `WatchInfo` entry. Multiple non-oneshot watches sharing the same
            // inotify descriptor (e.g. two features watching the same path)
            // share a single deadline.
            let mut debounce_deadlines: HashMap<WatchDescriptor, tokio::time::Instant> =
                HashMap::new();
            let mut consecutive_inotify_errors: u32 = 0;

            loop {
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
                                    error!(
                                        "FsWatcher: {MAX_CONSECUTIVE_INOTIFY_ERRORS} consecutive inotify errors, exiting task; \
                                         service will terminate and be restarted by systemd"
                                    );
                                    return;
                                }
                                continue;
                            }
                        };

                        let wd = event.wd.clone();
                        let Some(infos) = self.watch_info.get(&wd) else {
                            debug!("FsWatcher: unknown wd {wd:?}");
                            continue;
                        };

                        // Collect all matching entries. For `FileCreated` we
                        // filter by filename; for others every entry for this
                        // wd applies. Multiple features registering the same
                        // target all receive notifications.
                        let matched: Vec<(usize, TypeId, PathBuf, FsEventKind, bool)> = infos
                            .iter()
                            .enumerate()
                            .filter(|(_, info)| {
                                if info.kind == FsEventKind::FileCreated {
                                    info.path
                                        .file_name()
                                        .is_some_and(|expected| event.name.as_deref() == Some(expected))
                                } else {
                                    true
                                }
                            })
                            .map(|(i, info)| {
                                (i, info.feature_id, info.path.clone(), info.kind, info.oneshot)
                            })
                            .collect();

                        if matched.is_empty() {
                            continue;
                        }

                        let mut oneshot_indices: Vec<usize> = Vec::new();
                        let mut has_non_oneshot = false;

                        for (idx, feature_id, path, kind, oneshot) in matched {
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
                                oneshot_indices.push(idx);
                            } else {
                                has_non_oneshot = true;
                            }
                        }

                        if !oneshot_indices.is_empty() {
                            if let Some(infos) = self.watch_info.get_mut(&wd) {
                                // Descending order keeps earlier indices valid
                                // as later ones are removed.
                                oneshot_indices.sort_unstable();
                                for idx in oneshot_indices.into_iter().rev() {
                                    infos.remove(idx);
                                }
                            }
                            self.remove_kernel_watch_if_empty(&wd);
                        }

                        if has_non_oneshot {
                            debounce_deadlines.insert(
                                wd,
                                tokio::time::Instant::now() + COMMAND_EVENT_DEBOUNCE,
                            );
                        }
                    }

                    // When no deadline is armed, `pending()` makes this arm never fire,
                    // avoiding the periodic wake-up of a far-future sentinel sleep.
                    _ = async {
                        match debounce_deadlines.values().min().copied() {
                            Some(d) => tokio::time::sleep_until(d).await,
                            None => std::future::pending().await,
                        }
                    } => {
                        let now = tokio::time::Instant::now();
                        let expired: Vec<WatchDescriptor> = debounce_deadlines
                            .iter()
                            .filter(|(_, deadline)| **deadline <= now)
                            .map(|(wd, _)| wd.clone())
                            .collect();

                        for wd in expired {
                            debounce_deadlines.remove(&wd);
                            if let Some(infos) = self.watch_info.get(&wd) {
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
            .watch_file_modified::<TestFeature>(tmp.path())
            .expect("watch_file_modified");
    }

    #[tokio::test]
    async fn add_watch_dir_modified() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .watch_dir_modified::<TestFeature>(tmp.path())
            .expect("watch_dir_modified");
    }

    #[tokio::test]
    async fn add_watch_file_created() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let target = tmp.path().join("not-yet-existing.txt");
        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .watch_file_created_oneshot::<TestFeature>(&target)
            .expect("watch_file_created_oneshot");
    }

    #[tokio::test]
    async fn into_stream_file_modified() {
        let mut tmp = tempfile::NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        let (tx, mut rx) = mpsc::channel(16);

        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .watch_file_modified::<TestFeature>(&path)
            .expect("watch_file_modified");
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
            .watch_dir_modified::<TestFeature>(&dir_path)
            .expect("watch_dir_modified");
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
            .watch_file_created_oneshot::<TestFeature>(&target)
            .expect("watch_file_created_oneshot");
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
            .watch_file_created_oneshot::<TestFeature>(&target)
            .expect("watch_file_created_oneshot");

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
            .watch_file_modified::<TestFeature>(&path)
            .expect("watch_file_modified");
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
            .watch_file_created_oneshot::<TestFeature>(&target)
            .expect("watch_file_created_oneshot");
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
            .watch_file_created_oneshot::<TestFeature>(&target)
            .expect("watch_file_created_oneshot");

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
            .watch_file_created_oneshot::<TestFeature>(&target_a)
            .expect("watch a");
        watcher
            .watch_file_created_oneshot::<TestFeature>(&target_b)
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
            .watch_file_created_oneshot::<TestFeature>(&target)
            .expect("watch_file_created_oneshot");

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
            .watch_file_modified::<TestFeature>(Path::new("/nonexistent"))
            .expect("noop watch_file_modified should succeed");
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
    async fn cancellation_stops_event_loop() {
        let tmp = tempfile::NamedTempFile::new().expect("create temp file");
        let path = tmp.path().to_path_buf();
        let (tx, mut rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();

        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .watch_file_modified::<TestFeature>(&path)
            .expect("watch_file_modified");
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
    async fn file_created_oneshot_fanout_to_all_registrants() {
        // Regression: when two features register the same file-created target
        // (same parent dir, same filename), both must be notified — not just
        // the first one matched by `position()`.
        struct FeatureA;
        struct FeatureB;

        let tmp = tempfile::tempdir().expect("create temp dir");
        let target = tmp.path().join("shared.txt");
        let (tx, mut rx) = mpsc::channel(16);

        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .watch_file_created_oneshot::<FeatureA>(&target)
            .expect("watch A");
        watcher
            .watch_file_created_oneshot::<FeatureB>(&target)
            .expect("watch B");
        watcher
            .into_stream(tx, CancellationToken::new())
            .expect("into_stream");

        tokio::time::sleep(Duration::from_millis(50)).await;
        std::fs::write(&target, "shared").expect("create target");

        let mut feature_ids = Vec::new();
        for _ in 0..2 {
            let req = timeout(Duration::from_secs(4), rx.recv())
                .await
                .expect("timeout waiting for fan-out event")
                .expect("channel closed");
            match req.command {
                Command::FsEvent(FsEventCommand {
                    kind: FsEventKind::FileCreated,
                    feature_id,
                    ..
                }) => feature_ids.push(feature_id),
                other => panic!("expected FsEvent(FileCreated), got {other:?}"),
            }
        }

        feature_ids.sort();
        let mut expected = vec![TypeId::of::<FeatureA>(), TypeId::of::<FeatureB>()];
        expected.sort();
        assert_eq!(feature_ids, expected);

        // No third event — both oneshot entries were consumed.
        let third = timeout(Duration::from_secs(1), rx.recv()).await;
        assert!(third.is_err(), "expected no third event");
    }

    #[tokio::test]
    async fn file_created_via_rename() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let target = tmp.path().join("atomically-created.txt");
        let (tx, mut rx) = mpsc::channel(16);

        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .watch_file_created_oneshot::<TestFeature>(&target)
            .expect("watch_file_created_oneshot");
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
