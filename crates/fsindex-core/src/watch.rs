//! Filesystem watching via `notify` (inotify on Linux).
//!
//! One process-wide watcher manages a `NonRecursive` watch per indexed
//! directory. Events are buffered in a channel and drained by the engine's
//! maintenance loop, which debounces them into directory resyncs.

use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use rustc_hash::FxHashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Mutex, OnceLock};

/// A normalised event. `overflow` means the kernel queue overflowed and the
/// affected volume must be reconciled.
#[derive(Clone, Debug)]
pub struct CoreEvent {
    pub path: PathBuf,
    pub overflow: bool,
}

struct Inner {
    watcher: RecommendedWatcher,
    rx: Receiver<notify::Result<Event>>,
    watched: FxHashSet<PathBuf>,
}

static STATE: OnceLock<Mutex<Inner>> = OnceLock::new();

fn state() -> &'static Mutex<Inner> {
    STATE.get_or_init(|| {
        let (tx, rx) = channel();
        let handler = move |res: notify::Result<Event>| {
            let _ = tx.send(res);
        };
        let watcher = RecommendedWatcher::new(handler, Config::default())
            .expect("failed to initialise inotify watcher");
        Mutex::new(Inner {
            watcher,
            rx,
            watched: FxHashSet::default(),
        })
    })
}

/// Begin watching `path` (a directory) non-recursively. Idempotent.
pub fn start_watch(path: &Path) -> anyhow::Result<()> {
    start_watch_classified(path).map_err(|e| anyhow::anyhow!("{}", e.message))
}

/// Classification of watch failures: `Benign` failures (permissions, paths
/// that vanished, racey deletes) do not threaten index correctness, so they
/// must not mark a volume degraded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchFailure {
    Benign,
    Fatal,
}

#[derive(Clone, Debug)]
pub struct WatchError {
    pub failure: WatchFailure,
    pub message: String,
}

/// Begin watching `path` (a directory) non-recursively, classifying errors.
pub fn start_watch_classified(path: &Path) -> Result<(), WatchError> {
    let mut inner = state().lock().unwrap();
    if inner.watched.contains(path) {
        return Ok(());
    }
    match inner.watcher.watch(path, RecursiveMode::NonRecursive) {
        Ok(()) => {
            inner.watched.insert(path.to_path_buf());
            Ok(())
        }
        Err(err) => {
            let failure = classify(&err);
            Err(WatchError {
                failure,
                message: format!("watching {}: {err}", path.display()),
            })
        }
    }
}

fn classify(err: &notify::Error) -> WatchFailure {
    use notify::ErrorKind;
    match &err.kind {
        // A directory we cannot read (permissions) or that disappeared in a
        // race is not a correctness threat: it is excluded from watching and
        // any changes below it are caught by the periodic reconcile.
        ErrorKind::Io(io) => match io.kind() {
            std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::NotFound
            | std::io::ErrorKind::NotADirectory => WatchFailure::Benign,
            _ => WatchFailure::Fatal,
        },
        ErrorKind::PathNotFound | ErrorKind::WatchNotFound => WatchFailure::Benign,
        // Hitting the watch limit is real and must be surfaced.
        ErrorKind::MaxFilesWatch => WatchFailure::Fatal,
        ErrorKind::InvalidConfig(_) | ErrorKind::Generic(_) => WatchFailure::Fatal,
    }
}

/// Stop watching `path` and drop it from the registry.
pub fn stop_watch(path: &Path) {
    let mut inner = state().lock().unwrap();
    if inner.watched.remove(path) {
        let _ = inner.watcher.unwatch(path);
    }
}

/// Forget all watches (used when a volume goes offline).
pub fn stop_prefix(prefix: &Path) {
    let mut inner = state().lock().unwrap();
    let victims: Vec<PathBuf> = inner
        .watched
        .iter()
        .filter(|p| p.starts_with(prefix))
        .cloned()
        .collect();
    for p in victims {
        inner.watched.remove(&p);
        let _ = inner.watcher.unwatch(&p);
    }
}

pub fn watched_count() -> usize {
    state().lock().unwrap().watched.len()
}

/// Snapshot of every currently watched path.
pub fn watched_paths() -> Vec<PathBuf> {
    state().lock().unwrap().watched.iter().cloned().collect()
}

/// Drain all buffered events. Non-blocking.
pub fn drain_all() -> Vec<CoreEvent> {
    let inner = state().lock().unwrap();
    let mut out = Vec::new();
    while let Ok(res) = inner.rx.try_recv() {
        match res {
            Ok(ev) => {
                let overflow = ev.need_rescan();
                if ev.paths.is_empty() {
                    if overflow {
                        // No path to attribute; the engine reconciles all
                        // volumes for an empty overflow event.
                        out.push(CoreEvent {
                            path: PathBuf::new(),
                            overflow: true,
                        });
                    }
                    continue;
                }
                for p in ev.paths {
                    out.push(CoreEvent { path: p, overflow });
                }
            }
            Err(_) => break,
        }
    }
    out
}
