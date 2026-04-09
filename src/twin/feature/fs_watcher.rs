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
                let _ = tx.blocking_send(CommandRequest {
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
