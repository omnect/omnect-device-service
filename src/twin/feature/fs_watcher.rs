use crate::twin::feature::{
    COMMAND_EVENT_DEBOUNCE, Command, CommandRequest, FsEventCommand, FsEventKind,
    command::wait_for_deadline,
};
use anyhow::{Context, Result};
use futures::StreamExt;
use inotify::{EventMask, WatchDescriptor, WatchMask};
use log::{debug, error, warn};
use std::{
    any::TypeId,
    collections::{HashMap, HashSet},
    ffi::OsString,
    ops::ControlFlow,
    path::{Path, PathBuf},
};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

const INOTIFY_EVENT_BUF_LEN: usize = 4096;

/// Persistent (non-oneshot) watch. Every inotify event on the owning
/// `WatchDescriptor` fires this entry; the kernel mask set at `Watches::add`
/// is the only filter.
struct ModifiedWatch {
    feature_id: TypeId,
    path: PathBuf,
    kind: FsEventKind,
}

/// Oneshot `FileCreated` watch. The kernel watch lives on the parent
/// directory; events are filtered by inotify mask (must intersect
/// `CREATE | MOVED_TO`) and by event name (must equal the target filename)
/// before dispatch, then the entry is removed.
struct CreatedWatch {
    feature_id: TypeId,
    target: PathBuf,
}

/// A `CreatedWatch` that matched the current inotify event and is about to
/// be dispatched. `idx` refers to the position inside the owning
/// `Vec<CreatedWatch>` so the entry can be removed after a successful send.
struct FiringOneshot {
    idx: usize,
    feature_id: TypeId,
    target: PathBuf,
}

struct Backend {
    inotify: inotify::Inotify,
    watches: inotify::Watches,
    modified: HashMap<WatchDescriptor, Vec<ModifiedWatch>>,
    created: HashMap<WatchDescriptor, Vec<CreatedWatch>>,
}

/// Centralized inotify-based file watcher.
///
/// Persistent watches (`ModifiedWatch`) and oneshot watches (`CreatedWatch`)
/// are stored in separate per-descriptor tables so the same
/// `WatchDescriptor` can legally hold both (e.g. a `DirModified` on a
/// directory and a `FileCreated` for a specific file in that same
/// directory). `MASK_ADD` unions kernel masks when registrations overlap;
/// the event handler therefore filters `CreatedWatch` dispatch on both the
/// event mask (`CREATE | MOVED_TO`) and the event name, while
/// `ModifiedWatch` dispatch relies on the mask set at registration time.
///
/// The inotify backend is absent (`backend == None`) in the `noop` variant
/// used by mock builds and tests.
pub struct FsWatcher {
    backend: Option<Backend>,
}

impl FsWatcher {
    pub fn new() -> Result<Self> {
        let inotify = inotify::Inotify::init().context("FsWatcher: failed to init inotify")?;
        let watches = inotify.watches();
        Ok(Self {
            backend: Some(Backend {
                inotify,
                watches,
                modified: HashMap::new(),
                created: HashMap::new(),
            }),
        })
    }

    #[cfg(any(test, feature = "mock"))]
    pub fn noop() -> Self {
        Self { backend: None }
    }

    /// Watch a file for `CLOSE_WRITE` events (coalesced into one
    /// `FsEventKind::FileModified` command per debounce window).
    pub fn watch_file_modified<T: 'static>(&mut self, path: &Path) -> Result<()> {
        self.register_modified::<T>(
            path,
            WatchMask::CLOSE_WRITE | WatchMask::MASK_ADD,
            FsEventKind::FileModified,
        )
    }

    /// Watch a directory for entry additions and deletions (coalesced into
    /// one `FsEventKind::DirModified` command per debounce window).
    pub fn watch_dir_modified<T: 'static>(&mut self, path: &Path) -> Result<()> {
        self.register_modified::<T>(
            path,
            WatchMask::CREATE | WatchMask::DELETE | WatchMask::MASK_ADD,
            FsEventKind::DirModified,
        )
    }

    /// Watch for the appearance of a specific file (including atomic rename
    /// via `MOVED_TO`). Fires once, then the watch is removed.
    ///
    /// The kernel watch is placed on the parent directory; filename and
    /// mask filtering happen in the event loop. Kernel `ONESHOT` cannot be
    /// used: it would be consumed by any event in the parent dir before
    /// our target is seen.
    pub fn watch_file_created_oneshot<T: 'static>(&mut self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .with_context(|| format!("watch_file_created_oneshot: {path:?} has no parent"))?;
        let Some(backend) = self.backend.as_mut() else {
            return Ok(());
        };
        let wd = backend
            .watches
            .add(
                parent,
                WatchMask::CREATE | WatchMask::MOVED_TO | WatchMask::MASK_ADD,
            )
            .with_context(|| format!("watch_file_created_oneshot: failed to watch {parent:?}"))?;
        debug!("watch_file_created_oneshot: {wd:?} on {parent:?} (target: {path:?})");
        backend.created.entry(wd).or_default().push(CreatedWatch {
            feature_id: TypeId::of::<T>(),
            target: path.to_path_buf(),
        });
        Ok(())
    }

    fn register_modified<T: 'static>(
        &mut self,
        path: &Path,
        mask: WatchMask,
        kind: FsEventKind,
    ) -> Result<()> {
        let Some(backend) = self.backend.as_mut() else {
            return Ok(());
        };
        let wd = backend
            .watches
            .add(path, mask)
            .with_context(|| format!("register_modified: failed to watch {path:?}"))?;
        debug!("register_modified: {wd:?} on {path:?} ({kind:?})");
        backend.modified.entry(wd).or_default().push(ModifiedWatch {
            feature_id: TypeId::of::<T>(),
            path: path.to_path_buf(),
            kind,
        });
        Ok(())
    }

    /// Consume the watcher and spawn its event loop. Returns `Ok(None)` for
    /// the `noop` variant (no task spawned, `tx` is dropped). Returns
    /// `Err(...)` when the synchronous event-stream setup fails — this is
    /// reported before the task is spawned so the caller can surface it
    /// alongside other initialisation errors, rather than observing a
    /// silently-exited `JoinHandle`.
    pub fn into_stream(
        self,
        tx: mpsc::Sender<CommandRequest>,
        cancel: CancellationToken,
    ) -> Result<Option<JoinHandle<()>>> {
        let Some(Backend {
            inotify,
            watches,
            modified,
            created,
        }) = self.backend
        else {
            return Ok(None);
        };
        let stream = inotify
            .into_event_stream(vec![0u8; INOTIFY_EVENT_BUF_LEN])
            .context("FsWatcher: failed to create event stream")?;
        Ok(Some(tokio::spawn(run_event_loop(
            stream, watches, modified, created, tx, cancel,
        ))))
    }
}

async fn run_event_loop(
    mut stream: inotify::EventStream<Vec<u8>>,
    mut watches: inotify::Watches,
    mut modified: HashMap<WatchDescriptor, Vec<ModifiedWatch>>,
    mut created: HashMap<WatchDescriptor, Vec<CreatedWatch>>,
    tx: mpsc::Sender<CommandRequest>,
    cancel: CancellationToken,
) {
    if dispatch_race_events(&mut created, &mut watches, &modified, &tx)
        .await
        .is_break()
    {
        return;
    }

    // Debounce is keyed by `WatchDescriptor`, not by individual watch entry.
    // Multiple persistent watches sharing a descriptor (e.g. two features
    // watching the same path) share a single deadline.
    let mut debounce_deadlines: HashMap<WatchDescriptor, tokio::time::Instant> = HashMap::new();

    loop {
        let min_deadline = debounce_deadlines.values().min().copied();

        tokio::select! {
            biased;

            _ = cancel.cancelled() => return,

            event = stream.next() => {
                if handle_inotify_event(
                    event,
                    &mut modified,
                    &mut created,
                    &mut watches,
                    &tx,
                    &mut debounce_deadlines,
                )
                .await
                .is_break()
                {
                    return;
                }
            }

            _ = wait_for_deadline(min_deadline) => {
                if flush_expired_debounces(&modified, &mut debounce_deadlines, &tx)
                    .await
                    .is_break()
                {
                    return;
                }
            }
        }
    }
}

/// Dispatch `FileCreated` events for any oneshot targets that already exist
/// when the event loop starts. Prevents a lost event when the file appears
/// between `Feature::new` and the event loop consuming the first inotify
/// batch.
///
/// Between `try_exists() -> Ok(true)` and `tx.send` completing, the file
/// could be unlinked; the oneshot is still consumed. Acceptable because
/// watched paths are append-only state markers, not scratch files.
async fn dispatch_race_events(
    created: &mut HashMap<WatchDescriptor, Vec<CreatedWatch>>,
    watches: &mut inotify::Watches,
    modified: &HashMap<WatchDescriptor, Vec<ModifiedWatch>>,
    tx: &mpsc::Sender<CommandRequest>,
) -> ControlFlow<()> {
    let mut dispatched: HashSet<PathBuf> = HashSet::new();
    for list in created.values() {
        for w in list {
            match w.target.try_exists() {
                Ok(false) => continue,
                Err(e) => {
                    warn!("dispatch_race_events: cannot check {:?}: {e}", w.target);
                    continue;
                }
                Ok(true) => {}
            }
            debug!(
                "dispatch_race_events: {:?} exists, sending immediately",
                w.target
            );
            let req = CommandRequest {
                command: Command::FsEvent(FsEventCommand {
                    kind: FsEventKind::FileCreated,
                    feature_id: w.feature_id,
                    path: w.target.clone(),
                }),
                reply: None,
            };
            if tx.send(req).await.is_err() {
                error!("dispatch_race_events: receiver dropped");
                return ControlFlow::Break(());
            }
            dispatched.insert(w.target.clone());
        }
    }

    // Prune consumed oneshots; reclaim kernel watches whose descriptor has
    // no remaining user-space entries across both tables.
    let wds_to_reclaim: Vec<WatchDescriptor> = created
        .iter_mut()
        .filter_map(|(wd, list)| {
            list.retain(|w| !dispatched.contains(&w.target));
            list.is_empty().then(|| wd.clone())
        })
        .collect();
    for wd in wds_to_reclaim {
        created.remove(&wd);
        if modified.get(&wd).is_none_or(|l| l.is_empty())
            && let Err(e) = watches.remove(wd.clone())
        {
            debug!("FsWatcher: failed to remove kernel watch: {e}");
        }
    }
    ControlFlow::Continue(())
}

async fn handle_inotify_event(
    event: Option<Result<inotify::Event<OsString>, std::io::Error>>,
    modified: &mut HashMap<WatchDescriptor, Vec<ModifiedWatch>>,
    created: &mut HashMap<WatchDescriptor, Vec<CreatedWatch>>,
    watches: &mut inotify::Watches,
    tx: &mpsc::Sender<CommandRequest>,
    debounce_deadlines: &mut HashMap<WatchDescriptor, tokio::time::Instant>,
) -> ControlFlow<()> {
    let Some(event) = event else {
        error!("FsWatcher: inotify stream ended, all file watches are now inactive");
        return ControlFlow::Break(());
    };
    let event = match event {
        Ok(e) => e,
        // inotify-rs auto-retries EINTR/EAGAIN internally; any Err that
        // surfaces here (EBADF, EINVAL, …) is terminal. Exit immediately so
        // systemd restarts with a fresh inotify instance rather than
        // looping on a broken fd.
        Err(e) => {
            error!("FsWatcher: terminal inotify error: {e:#}");
            return ControlFlow::Break(());
        }
    };

    // Q_OVERFLOW: the kernel dropped events we cannot reconstruct. Bail out
    // so systemd restarts and rebuilds a consistent view rather than
    // silently missing file-state transitions.
    if event.mask.contains(EventMask::Q_OVERFLOW) {
        error!("FsWatcher: inotify queue overflow, exiting to let systemd restart");
        return ControlFlow::Break(());
    }

    let wd = event.wd.clone();
    let has_modified = modified.get(&wd).is_some_and(|l| !l.is_empty());
    let is_create_or_move = event
        .mask
        .intersects(EventMask::CREATE | EventMask::MOVED_TO);

    if has_modified {
        debug!("FsWatcher: event on {wd:?}, arming debounce");
        debounce_deadlines.insert(
            wd.clone(),
            tokio::time::Instant::now() + COMMAND_EVENT_DEBOUNCE,
        );
    }

    let mut created_changed = false;
    if is_create_or_move && let Some(list) = created.get_mut(&wd) {
        // Collect matches first so the `list` borrow is free across the
        // `.await` in the send loop below.
        let to_fire: Vec<FiringOneshot> = list
            .iter()
            .enumerate()
            .filter(|(_, w)| {
                w.target
                    .file_name()
                    .is_some_and(|expected| event.name.as_deref() == Some(expected))
            })
            .map(|(i, w)| FiringOneshot {
                idx: i,
                feature_id: w.feature_id,
                target: w.target.clone(),
            })
            .collect();

        let mut fired_indices: Vec<usize> = Vec::with_capacity(to_fire.len());
        for firing in to_fire {
            debug!("FsWatcher: oneshot match for {:?}", firing.target);
            let req = CommandRequest {
                command: Command::FsEvent(FsEventCommand {
                    kind: FsEventKind::FileCreated,
                    feature_id: firing.feature_id,
                    path: firing.target,
                }),
                reply: None,
            };
            if tx.send(req).await.is_err() {
                warn!("FsWatcher: receiver dropped, stopping");
                return ControlFlow::Break(());
            }
            fired_indices.push(firing.idx);
        }

        if !fired_indices.is_empty() {
            // Descending removal keeps earlier indices valid as later ones
            // are removed.
            for idx in fired_indices.into_iter().rev() {
                list.remove(idx);
            }
            created_changed = true;
        }
    }

    if created_changed {
        reclaim_if_unused(modified, created, watches, &wd);
    }

    ControlFlow::Continue(())
}

async fn flush_expired_debounces(
    modified: &HashMap<WatchDescriptor, Vec<ModifiedWatch>>,
    debounce_deadlines: &mut HashMap<WatchDescriptor, tokio::time::Instant>,
    tx: &mpsc::Sender<CommandRequest>,
) -> ControlFlow<()> {
    let now = tokio::time::Instant::now();
    let expired: Vec<WatchDescriptor> = debounce_deadlines
        .iter()
        .filter(|(_, d)| **d <= now)
        .map(|(wd, _)| wd.clone())
        .collect();

    for wd in expired {
        debounce_deadlines.remove(&wd);
        let Some(list) = modified.get(&wd) else {
            continue;
        };
        for w in list {
            debug!(
                "FsWatcher: debounce elapsed for {:?} ({:?})",
                w.path, w.kind
            );
            let req = CommandRequest {
                command: Command::FsEvent(FsEventCommand {
                    kind: w.kind,
                    feature_id: w.feature_id,
                    path: w.path.clone(),
                }),
                reply: None,
            };
            if tx.send(req).await.is_err() {
                warn!("FsWatcher: receiver dropped, stopping");
                return ControlFlow::Break(());
            }
        }
    }
    ControlFlow::Continue(())
}

/// Remove the kernel watch for `wd` when both user-space tables are empty
/// for that descriptor. The kernel remove is attempted before user-space
/// cleanup; if it fails (typically because the kernel auto-removed the
/// descriptor, e.g. the watched dir was deleted) the user-space entries
/// are still cleaned up — the `wd` is gone either way.
fn reclaim_if_unused(
    modified: &mut HashMap<WatchDescriptor, Vec<ModifiedWatch>>,
    created: &mut HashMap<WatchDescriptor, Vec<CreatedWatch>>,
    watches: &mut inotify::Watches,
    wd: &WatchDescriptor,
) {
    let modified_empty = modified.get(wd).is_none_or(|l| l.is_empty());
    let created_empty = created.get(wd).is_none_or(|l| l.is_empty());
    if !(modified_empty && created_empty) {
        return;
    }
    if let Err(e) = watches.remove(wd.clone()) {
        debug!("FsWatcher: failed to remove kernel watch: {e}");
    }
    modified.remove(wd);
    created.remove(wd);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tokio::time::{Duration, timeout};

    const STREAM_STARTUP_DELAY: Duration = Duration::from_millis(50);
    // `COMMAND_EVENT_DEBOUNCE` plus a CI-safe margin.
    const DEBOUNCED_EVENT_TIMEOUT: Duration = Duration::from_secs(4);
    const IMMEDIATE_EVENT_TIMEOUT: Duration = Duration::from_millis(500);
    const NEGATIVE_WAIT: Duration = Duration::from_secs(1);

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
    async fn watch_file_created_oneshot_rejects_path_without_parent() {
        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        let err = watcher
            .watch_file_created_oneshot::<TestFeature>(Path::new("/"))
            .expect_err("root path must be rejected");
        assert!(
            format!("{err:#}").contains("no parent"),
            "unexpected error: {err:#}"
        );
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

        tokio::time::sleep(STREAM_STARTUP_DELAY).await;

        writeln!(tmp, "hello").expect("write");
        tmp.flush().expect("flush");
        drop(tmp);

        let req = timeout(DEBOUNCED_EVENT_TIMEOUT, rx.recv())
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

        tokio::time::sleep(STREAM_STARTUP_DELAY).await;

        std::fs::write(dir_path.join("new-file.txt"), "content").expect("create file");

        let req = timeout(DEBOUNCED_EVENT_TIMEOUT, rx.recv())
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

        tokio::time::sleep(STREAM_STARTUP_DELAY).await;

        std::fs::write(&target, "created").expect("create file");

        let req = timeout(DEBOUNCED_EVENT_TIMEOUT, rx.recv())
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

        std::fs::write(&target, "pre-existing").expect("create file");

        let (tx, mut rx) = mpsc::channel(16);
        watcher
            .into_stream(tx, CancellationToken::new())
            .expect("into_stream");

        let req = timeout(IMMEDIATE_EVENT_TIMEOUT, rx.recv())
            .await
            .expect("timeout: race-condition event should arrive immediately")
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

        tokio::time::sleep(STREAM_STARTUP_DELAY).await;

        for i in 0..3 {
            std::fs::write(&path, format!("write {i}")).expect("write");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let req = timeout(DEBOUNCED_EVENT_TIMEOUT, rx.recv())
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

        let second = timeout(Duration::from_secs(3), rx.recv()).await;
        assert!(
            second.is_err(),
            "expected no second event, but got one (debounce failed)"
        );
    }

    #[tokio::test]
    async fn file_created_oneshot_ignores_unrelated_files() {
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

        tokio::time::sleep(STREAM_STARTUP_DELAY).await;

        std::fs::write(tmp.path().join("unrelated.txt"), "noise").expect("create unrelated");
        tokio::time::sleep(Duration::from_millis(100)).await;

        std::fs::write(&target, "found").expect("create target");

        let req = timeout(DEBOUNCED_EVENT_TIMEOUT, rx.recv())
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
        let tmp = tempfile::tempdir().expect("create temp dir");
        let target = tmp.path().join("exists.txt");

        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .watch_file_created_oneshot::<TestFeature>(&target)
            .expect("watch_file_created_oneshot");

        std::fs::write(&target, "pre-existing").expect("create file");

        let (tx, mut rx) = mpsc::channel(16);
        watcher
            .into_stream(tx, CancellationToken::new())
            .expect("into_stream");

        let req = timeout(IMMEDIATE_EVENT_TIMEOUT, rx.recv())
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

        let second = timeout(NEGATIVE_WAIT, rx.recv()).await;
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

        tokio::time::sleep(STREAM_STARTUP_DELAY).await;

        std::fs::write(&target_a, "a").expect("create a");
        std::fs::write(&target_b, "b").expect("create b");

        let mut paths = Vec::new();
        for _ in 0..2 {
            let req = timeout(DEBOUNCED_EVENT_TIMEOUT, rx.recv())
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
        let tmp = tempfile::tempdir().expect("create temp dir");
        let target = tmp.path().join("pre-existing.txt");

        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .watch_file_created_oneshot::<TestFeature>(&target)
            .expect("watch_file_created_oneshot");

        std::fs::write(&target, "already here").expect("create file");

        let (tx, mut rx) = mpsc::channel(16);
        watcher
            .into_stream(tx, CancellationToken::new())
            .expect("into_stream");

        let _ = timeout(IMMEDIATE_EVENT_TIMEOUT, rx.recv())
            .await
            .expect("timeout: immediate event expected")
            .expect("channel closed");

        std::fs::write(tmp.path().join("other.txt"), "noise").expect("create other file");

        let spurious = timeout(NEGATIVE_WAIT, rx.recv()).await;
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

        tokio::time::sleep(STREAM_STARTUP_DELAY).await;

        cancel.cancel();

        let result = timeout(Duration::from_secs(2), handle).await;
        assert!(result.is_ok(), "task should exit after cancellation");

        assert!(
            rx.recv().await.is_none(),
            "channel should close after cancellation"
        );
    }

    #[tokio::test]
    async fn file_created_oneshot_fanout_to_all_registrants() {
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

        tokio::time::sleep(STREAM_STARTUP_DELAY).await;
        std::fs::write(&target, "shared").expect("create target");

        let mut feature_ids = Vec::new();
        for _ in 0..2 {
            let req = timeout(DEBOUNCED_EVENT_TIMEOUT, rx.recv())
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

        let third = timeout(NEGATIVE_WAIT, rx.recv()).await;
        assert!(third.is_err(), "expected no third event");
    }

    #[tokio::test]
    async fn file_created_race_fanout_to_all_registrants() {
        // Regression: when two features register the same pre-existing
        // target, the race-path dispatch at startup must deliver to both —
        // not only the first one enumerated.
        struct FeatureA;
        struct FeatureB;

        let tmp = tempfile::tempdir().expect("create temp dir");
        let target = tmp.path().join("pre-existing.txt");
        std::fs::write(&target, "pre").expect("create target");

        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .watch_file_created_oneshot::<FeatureA>(&target)
            .expect("watch A");
        watcher
            .watch_file_created_oneshot::<FeatureB>(&target)
            .expect("watch B");

        let (tx, mut rx) = mpsc::channel(16);
        watcher
            .into_stream(tx, CancellationToken::new())
            .expect("into_stream");

        let mut feature_ids = Vec::new();
        for _ in 0..2 {
            let req = timeout(IMMEDIATE_EVENT_TIMEOUT, rx.recv())
                .await
                .expect("timeout: race-path fan-out event")
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

        let third = timeout(NEGATIVE_WAIT, rx.recv()).await;
        assert!(third.is_err(), "expected no third event after race fan-out");
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

        tokio::time::sleep(STREAM_STARTUP_DELAY).await;

        let staging = tmp.path().join(".staging.tmp");
        std::fs::write(&staging, "atomic content").expect("write staging");
        std::fs::rename(&staging, &target).expect("rename");

        let req = timeout(DEBOUNCED_EVENT_TIMEOUT, rx.recv())
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

    #[tokio::test]
    async fn dir_modified_and_file_created_coexist_on_same_parent() {
        // Kind-split tables allow a persistent DirModified and a oneshot
        // FileCreated to share the same parent-directory descriptor. A
        // CREATE event must fire both: FileCreated immediately (oneshot
        // consumed), DirModified after the debounce.
        struct DirFeature;
        struct FileFeature;

        let tmp = tempfile::tempdir().expect("create temp dir");
        let target = tmp.path().join("target.txt");
        let (tx, mut rx) = mpsc::channel(16);

        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .watch_dir_modified::<DirFeature>(tmp.path())
            .expect("watch dir");
        watcher
            .watch_file_created_oneshot::<FileFeature>(&target)
            .expect("watch file");
        watcher
            .into_stream(tx, CancellationToken::new())
            .expect("into_stream");

        tokio::time::sleep(STREAM_STARTUP_DELAY).await;
        std::fs::write(&target, "x").expect("create target");

        let first = timeout(IMMEDIATE_EVENT_TIMEOUT, rx.recv())
            .await
            .expect("timeout: FileCreated event")
            .expect("channel closed");
        let first_id = match first.command {
            Command::FsEvent(FsEventCommand {
                kind: FsEventKind::FileCreated,
                feature_id,
                ..
            }) => feature_id,
            other => panic!("expected FsEvent(FileCreated), got {other:?}"),
        };
        assert_eq!(first_id, TypeId::of::<FileFeature>());

        let second = timeout(DEBOUNCED_EVENT_TIMEOUT, rx.recv())
            .await
            .expect("timeout: DirModified event")
            .expect("channel closed");
        let second_id = match second.command {
            Command::FsEvent(FsEventCommand {
                kind: FsEventKind::DirModified,
                feature_id,
                ..
            }) => feature_id,
            other => panic!("expected FsEvent(DirModified), got {other:?}"),
        };
        assert_eq!(second_id, TypeId::of::<DirFeature>());
    }

    #[tokio::test]
    async fn delete_event_does_not_fire_created_oneshot() {
        // Regression: MASK_ADD on a parent directory with both
        // watch_file_created_oneshot (CREATE|MOVED_TO) and a prior DELETE
        // from another registration (e.g. none here; we simulate by
        // pre-creating then removing the target before into_stream).
        // Without the event-mask filter, the queued DELETE event for the
        // target filename would erroneously fire the still-armed oneshot.
        let tmp = tempfile::tempdir().expect("create temp dir");
        let target = tmp.path().join("target.txt");
        std::fs::write(&target, "pre").expect("create target");

        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        // Register DirModified first so the kernel mask gains DELETE on
        // the parent dir, which is the scenario that produces a queued
        // DELETE event for the target filename.
        watcher
            .watch_dir_modified::<TestFeature>(tmp.path())
            .expect("watch dir");
        watcher
            .watch_file_created_oneshot::<TestFeature>(&target)
            .expect("watch oneshot");

        // Remove target before into_stream. `try_exists` in dispatch_race
        // will see false, so the oneshot is NOT race-fired. The inotify
        // queue holds a DELETE event with name="target.txt".
        std::fs::remove_file(&target).expect("remove target");

        let (tx, mut rx) = mpsc::channel(16);
        watcher
            .into_stream(tx, CancellationToken::new())
            .expect("into_stream");

        // Give the event loop time to read the queued DELETE.
        tokio::time::sleep(IMMEDIATE_EVENT_TIMEOUT).await;

        // The only event we expect is the DirModified, debounced. A
        // FileCreated would indicate the mask filter failed.
        let req = timeout(DEBOUNCED_EVENT_TIMEOUT, rx.recv())
            .await
            .expect("timeout: DirModified should still fire")
            .expect("channel closed");
        match req.command {
            Command::FsEvent(FsEventCommand {
                kind: FsEventKind::DirModified,
                ..
            }) => {}
            Command::FsEvent(FsEventCommand {
                kind: FsEventKind::FileCreated,
                ..
            }) => panic!("DELETE event erroneously fired FileCreated oneshot"),
            other => panic!("unexpected command: {other:?}"),
        }

        let extra = timeout(NEGATIVE_WAIT, rx.recv()).await;
        assert!(extra.is_err(), "expected only DirModified, got extra event");
    }

    #[tokio::test]
    async fn debounce_handles_multiple_wds_independently() {
        let tmp1 = tempfile::NamedTempFile::new().expect("tmp1");
        let tmp2 = tempfile::NamedTempFile::new().expect("tmp2");
        let p1 = tmp1.path().to_path_buf();
        let p2 = tmp2.path().to_path_buf();

        let (tx, mut rx) = mpsc::channel(16);
        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .watch_file_modified::<TestFeature>(&p1)
            .expect("watch p1");
        watcher
            .watch_file_modified::<TestFeature>(&p2)
            .expect("watch p2");
        watcher
            .into_stream(tx, CancellationToken::new())
            .expect("into_stream");
        tokio::time::sleep(STREAM_STARTUP_DELAY).await;

        std::fs::write(&p1, "a").expect("write p1");
        std::fs::write(&p2, "b").expect("write p2");

        let mut paths = Vec::new();
        for _ in 0..2 {
            let req = timeout(DEBOUNCED_EVENT_TIMEOUT, rx.recv())
                .await
                .expect("timeout waiting for debounced event")
                .expect("channel closed");
            match req.command {
                Command::FsEvent(FsEventCommand {
                    kind: FsEventKind::FileModified,
                    path,
                    ..
                }) => paths.push(path),
                other => panic!("expected FileModified, got {other:?}"),
            }
        }
        paths.sort();
        let mut expected = vec![p1, p2];
        expected.sort();
        assert_eq!(paths, expected);
    }

    #[tokio::test]
    async fn receiver_drop_stops_event_loop_during_race_dispatch() {
        // The race-path dispatch runs before the inotify event loop; a
        // receiver already gone at spawn time must terminate the task.
        let tmp = tempfile::tempdir().expect("create temp dir");
        let target = tmp.path().join("pre-existing.txt");
        std::fs::write(&target, "pre").expect("create file");

        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .watch_file_created_oneshot::<TestFeature>(&target)
            .expect("watch_file_created_oneshot");

        let (tx, rx) = mpsc::channel(1);
        drop(rx);

        let handle = watcher
            .into_stream(tx, CancellationToken::new())
            .expect("into_stream")
            .expect("should spawn");

        let result = timeout(Duration::from_secs(2), handle).await;
        assert!(
            result.is_ok(),
            "task should exit when receiver is gone before race dispatch"
        );
    }

    #[tokio::test]
    async fn receiver_drop_stops_event_loop_during_oneshot_dispatch() {
        // The inotify-driven oneshot dispatch sends per-match; a receiver
        // dropped after the event loop is alive must terminate the task on
        // the first send attempt.
        let tmp = tempfile::tempdir().expect("create temp dir");
        let target = tmp.path().join("target.txt");

        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .watch_file_created_oneshot::<TestFeature>(&target)
            .expect("watch_file_created_oneshot");

        let (tx, rx) = mpsc::channel(16);
        let handle = watcher
            .into_stream(tx, CancellationToken::new())
            .expect("into_stream")
            .expect("should spawn");

        tokio::time::sleep(STREAM_STARTUP_DELAY).await;
        drop(rx);

        std::fs::write(&target, "now").expect("create target");

        let result = timeout(Duration::from_secs(2), handle).await;
        assert!(
            result.is_ok(),
            "task should exit when oneshot dispatch fails to send"
        );
    }

    #[tokio::test]
    async fn receiver_drop_stops_event_loop_during_debounce_flush() {
        let tmp = tempfile::NamedTempFile::new().expect("tmp");
        let path = tmp.path().to_path_buf();
        let (tx, rx) = mpsc::channel(16);
        let mut watcher = FsWatcher::new().expect("FsWatcher::new");
        watcher
            .watch_file_modified::<TestFeature>(&path)
            .expect("watch");
        let handle = watcher
            .into_stream(tx, CancellationToken::new())
            .expect("into_stream")
            .expect("should spawn");

        tokio::time::sleep(STREAM_STARTUP_DELAY).await;

        drop(rx);

        std::fs::write(&path, "x").expect("write");

        let result = timeout(Duration::from_secs(5), handle).await;
        assert!(
            result.is_ok(),
            "task should exit after receiver drop during debounce flush"
        );
    }

    /// Get a live `WatchDescriptor` to use when fabricating synthetic
    /// `inotify::Event` values for the unit tests below. `WatchDescriptor`
    /// fields are `pub(crate)` in the `inotify` crate, so we cannot build
    /// one directly.
    fn live_watch_descriptor(
        watches: &mut inotify::Watches,
        path: &Path,
    ) -> WatchDescriptor {
        watches
            .add(path, WatchMask::CREATE)
            .expect("add live watch")
    }

    #[tokio::test]
    async fn handle_event_q_overflow_breaks_loop() {
        let tmp = tempfile::tempdir().expect("tmp");
        let inotify = inotify::Inotify::init().expect("inotify init");
        let mut watches = inotify.watches();
        let wd = live_watch_descriptor(&mut watches, tmp.path());

        let mut modified: HashMap<WatchDescriptor, Vec<ModifiedWatch>> = HashMap::new();
        let mut created: HashMap<WatchDescriptor, Vec<CreatedWatch>> = HashMap::new();
        let mut deadlines: HashMap<WatchDescriptor, tokio::time::Instant> = HashMap::new();
        let (tx, _rx) = mpsc::channel(1);

        let synthetic: inotify::Event<OsString> = inotify::Event {
            wd: wd.clone(),
            mask: EventMask::Q_OVERFLOW,
            cookie: 0,
            name: None,
        };

        let flow = handle_inotify_event(
            Some(Ok(synthetic)),
            &mut modified,
            &mut created,
            &mut watches,
            &tx,
            &mut deadlines,
        )
        .await;

        assert!(flow.is_break(), "Q_OVERFLOW must break the event loop");
        assert!(
            deadlines.is_empty(),
            "Q_OVERFLOW must not arm a debounce deadline"
        );
    }

    #[tokio::test]
    async fn handle_event_terminal_error_breaks_loop() {
        let inotify = inotify::Inotify::init().expect("inotify init");
        let mut watches = inotify.watches();

        let mut modified: HashMap<WatchDescriptor, Vec<ModifiedWatch>> = HashMap::new();
        let mut created: HashMap<WatchDescriptor, Vec<CreatedWatch>> = HashMap::new();
        let mut deadlines: HashMap<WatchDescriptor, tokio::time::Instant> = HashMap::new();
        let (tx, _rx) = mpsc::channel(1);

        let err = std::io::Error::other("synthetic terminal error");
        let flow = handle_inotify_event(
            Some(Err(err)),
            &mut modified,
            &mut created,
            &mut watches,
            &tx,
            &mut deadlines,
        )
        .await;

        assert!(flow.is_break(), "terminal inotify error must break the loop");
    }

    #[tokio::test]
    async fn handle_event_stream_end_breaks_loop() {
        let inotify = inotify::Inotify::init().expect("inotify init");
        let mut watches = inotify.watches();

        let mut modified: HashMap<WatchDescriptor, Vec<ModifiedWatch>> = HashMap::new();
        let mut created: HashMap<WatchDescriptor, Vec<CreatedWatch>> = HashMap::new();
        let mut deadlines: HashMap<WatchDescriptor, tokio::time::Instant> = HashMap::new();
        let (tx, _rx) = mpsc::channel(1);

        let flow = handle_inotify_event(
            None,
            &mut modified,
            &mut created,
            &mut watches,
            &tx,
            &mut deadlines,
        )
        .await;

        assert!(flow.is_break(), "stream end must break the loop");
    }
}
