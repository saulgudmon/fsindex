//! The engine ties everything together: it owns the index, runs the initial
//! scan, keeps per-volume inotify watchers in sync with the index, applies
//! debounced directory resyncs and periodic reconciliation, and serves
//! queries.

use crate::config::Config;
use crate::index::{now_secs, Index, VolumeIdx};
use crate::model::{
    CountResponse, EntryKind, NodeId, RootStatus, ScanState, SearchQuery, SearchResponse,
    StatusResponse, VolumeState, VolumeStatus,
};
use crate::query;
use crate::scan;
use crate::volume;
use crate::watch;
use parking_lot::{Mutex, RwLock};
use rustc_hash::{FxHashMap, FxHashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Shared, mutable engine state. Held behind an `RwLock`; readers (queries)
/// run concurrently, writers (scanner/watcher) take it exclusively and only
/// for short bursts.
pub struct Shared {
    pub index: Index,
    pub config: Config,
    /// Cached sorted result set for the last query, so pager/scroll requests
    /// don't re-filter and re-sort the whole index on every page. Own `Mutex`:
    /// the expensive rebuild runs on the maintenance thread while requests keep
    /// serving the previous snapshot under a read lock on the engine.
    pub query_cache: Mutex<crate::query::QueryCache>,
    pub pending: FxHashSet<NodeId>,
    pub dir_failures: FxHashMap<NodeId, u32>,
    pub watch_errors: FxHashMap<VolumeIdx, u64>,
    pub scanning: FxHashSet<VolumeIdx>,
    pub last_scan_finished: Option<i64>,
    pub overflowed: FxHashSet<VolumeIdx>,
    /// False until the first full scan of all roots has completed.
    pub initial_scan_done: bool,
    pub config_path: PathBuf,
}

impl Shared {
    pub fn new(config: Config, config_path: PathBuf) -> anyhow::Result<Self> {
        let exclusions = config.exclusions()?;
        Ok(Shared {
            index: Index::new(exclusions),
            config,
            query_cache: Mutex::new(crate::query::QueryCache::default()),
            pending: FxHashSet::default(),
            dir_failures: FxHashMap::default(),
            watch_errors: FxHashMap::default(),
            scanning: FxHashSet::default(),
            last_scan_finished: None,
            overflowed: FxHashSet::default(),
            initial_scan_done: false,
            config_path,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootHealth {
    Removed,
    MountLost,
    Ok,
}

pub struct Engine {
    shared: Arc<RwLock<Shared>>,
    start: Instant,
    pid: u32,
    shutdown: Arc<AtomicBool>,
    last_compact: Arc<Mutex<Instant>>,
    last_reconcile: Arc<Mutex<Instant>>,
    last_prune: Arc<Mutex<Instant>>,
    last_cache_refresh: Arc<Mutex<Instant>>,
}

impl Engine {
    /// Build an engine and perform the initial scan synchronously. Used by
    /// tests and `--check`.
    pub fn new(config: Config, config_path: PathBuf) -> anyhow::Result<Arc<Engine>> {
        let engine = Self::build(config, config_path)?;
        engine.initial_scan();
        Ok(engine)
    }

    /// Build an engine **without** scanning. The caller can bind/serve
    /// immediately and then call [`Engine::start_background_scan`].
    pub fn build(config: Config, config_path: PathBuf) -> anyhow::Result<Arc<Engine>> {
        let shared = Shared::new(config, config_path)?;
        Ok(Arc::new(Engine {
            shared: Arc::new(RwLock::new(shared)),
            start: Instant::now(),
            pid: std::process::id(),
            shutdown: Arc::new(AtomicBool::new(false)),
            last_compact: Arc::new(Mutex::new(Instant::now())),
            last_reconcile: Arc::new(Mutex::new(Instant::now())),
            last_prune: Arc::new(Mutex::new(Instant::now())),
            last_cache_refresh: Arc::new(Mutex::new(Instant::now())),
        }))
    }

    /// Kick off the initial scan on a background thread so the daemon can
    /// serve queries (and report scan progress) while it runs.
    pub fn start_background_scan(self: &Arc<Engine>) {
        let this = self.clone();
        std::thread::Builder::new()
            .name("fsindex-initial-scan".into())
            .spawn(move || this.initial_scan())
            .expect("spawning initial scan thread");
    }

    pub fn shared(&self) -> &Arc<RwLock<Shared>> {
        &self.shared
    }

    pub fn config_path(&self) -> PathBuf {
        self.shared.read().config_path.clone()
    }

    pub fn shutdown_flag(&self) -> Arc<AtomicBool> {
        self.shutdown.clone()
    }

    pub fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    // ---------------------------------------------------------------- scan

    /// Scan every configured root once, grouping roots by device so each
    /// volume's watcher covers all of its roots.
    pub fn initial_scan(&self) {
        let roots: Vec<PathBuf> = {
            let s = self.shared.read();
            s.config
                .enabled_roots()
                .iter()
                .map(|r| r.resolved())
                .collect()
        };
        info!("initial scan of {} root(s)", roots.len());
        for root in roots {
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            self.scan_root(&root);
        }
        {
            let mut s = self.shared.write();
            s.last_scan_finished = Some(now_secs());
            s.initial_scan_done = true;
            s.index.compact();
        }
        let totals = self.shared.read().index.totals;
        info!(
            "initial scan complete: {} entries ({} files, {} dirs) across {} volume(s)",
            totals.entries,
            totals.files,
            totals.dirs,
            self.shared.read().index.volumes.len()
        );
    }

    fn scan_root(&self, root: &Path) -> Option<VolumeIdx> {
        let info = match volume::probe(root) {
            Some(v) => v,
            None => {
                warn!("root {} is not available; skipping", root.display());
                return None;
            }
        };
        let root_path = std::fs::canonicalize(root)
            .unwrap_or_else(|_| root.to_path_buf())
            .to_string_lossy()
            .to_string();
        let (exclusions, vol, root_id) = {
            let mut s = self.shared.write();
            let vol = if let Some(&v) = s.index.volume_by_id.get(&info.id) {
                v
            } else {
                s.index.ensure_volume(info.id, &info.name, &info.mount)
            };
            // Drop any previous copy of this root before scanning it fresh
            // (also removes its RootRec + volume root_paths).
            if let Some(existing) = s
                .index
                .roots
                .iter()
                .find(|r| r.path == root_path)
                .map(|r| r.id)
            {
                s.index.remove_root(existing);
            }
            let root_id = s.index.ensure_root(vol, &root_path);
            (s.index.exclusions.clone(), vol, root_id)
        };
        self.sync_watches(vol);

        // Stream the tree into the live index in batches so queries see
        // partial results while the scan is running.
        scan::visit_root(root, &exclusions, |first_gid, batch| {
            if self.shutdown.load(Ordering::SeqCst) {
                return false;
            }
            let mut s = self.shared.write();
            s.index.insert_scan_batch(root_id, first_gid, batch);
            true
        });

        {
            let mut s = self.shared.write();
            s.index.volume_rec_mut(vol).last_scan_finished = Some(now_secs());
        }
        self.sync_watches(vol);
        Some(vol)
    }

    /// Full rescan of a single volume (correctness floor / overflow recovery).
    pub fn reconcile_volume(&self, vol: VolumeIdx) {
        let roots: Vec<PathBuf> = {
            let s = self.shared.read();
            s.index
                .roots_of_volume(vol)
                .iter()
                .map(PathBuf::from)
                .collect()
        };
        if roots.is_empty() {
            return;
        }
        {
            let mut s = self.shared.write();
            s.scanning.insert(vol);
            s.index.volume_rec_mut(vol).state = VolumeState::Scanning;
            // Clear the whole volume once, then merge every root back in.
            s.index.clear_volume(vol);
        }
        info!(
            "reconciling volume {} ({} root(s))",
            self.volume_label(vol),
            roots.len()
        );
        for root in &roots {
            let exclusions = self.shared.read().index.exclusions.clone();
            let frag = scan::scan_root(root, &exclusions);
            let root_path = frag.root_path.to_string_lossy().to_string();
            let mut s = self.shared.write();
            s.index.merge_fragment(frag, vol, &root_path);
            s.index.volume_rec_mut(vol).last_scan_finished = Some(now_secs());
            s.index.volume_rec_mut(vol).state = VolumeState::Online;
        }
        self.ensure_watches(vol);
        let mut s = self.shared.write();
        s.scanning.remove(&vol);
        s.overflowed.remove(&vol);
        s.last_scan_finished = Some(now_secs());
    }

    /// Reconcile every online volume.
    pub fn reconcile_all(&self) {
        let vols: Vec<VolumeIdx> = {
            let s = self.shared.read();
            (0..s.index.volumes.len())
                .map(|i| VolumeIdx(i as u16))
                .collect()
        };
        for vol in vols {
            let online = {
                let s = self.shared.read();
                s.index.volume_state(vol) != VolumeState::Offline
            };
            if online {
                self.reconcile_volume(vol);
            }
        }
    }

    /// Reload configuration from disk: apply exclusion changes, add newly
    /// configured roots, and drop roots that are gone. A reconcile runs for
    /// affected volumes.
    pub fn reload_config(&self) -> anyhow::Result<()> {
        let path = self.shared.read().config_path.clone();
        if path.as_os_str().is_empty() {
            anyhow::bail!("no config path");
        }
        let cfg = Config::load(&path)?;
        let new_exclusions = cfg.exclusions()?;
        // Exclusion-rule changes are not caught by a directory resync: a
        // newly-excluded subtree must be purged, a newly-included one restored.
        // Detect and reconcile affected volumes.
        let exclusions_changed = cfg.exclusions != self.shared.read().config.exclusions;
        let new_roots: Vec<PathBuf> = cfg.enabled_roots().iter().map(|r| r.resolved()).collect();
        let old_roots: Vec<PathBuf> = {
            let s = self.shared.read();
            s.config
                .enabled_roots()
                .iter()
                .map(|r| r.resolved())
                .collect()
        };
        // Apply exclusions + config snapshot.
        {
            let mut s = self.shared.write();
            s.config = cfg;
            s.index.exclusions = new_exclusions;
            // Exclusions/roots change the match set regardless of `generation`
            // churn, so force the cache to rebuild.
            s.query_cache.lock().invalidate();
        }
        // Remove roots that disappeared.
        for root in &old_roots {
            if !new_roots.iter().any(|r| r == root) {
                let mut s = self.shared.write();
                if let Some(id) = s
                    .index
                    .roots
                    .iter()
                    .find(|r| Path::new(&r.path) == root.as_path())
                    .map(|r| r.id)
                {
                    s.index.remove_root(id);
                }
            }
        }
        // Scan roots that are new.
        for root in &new_roots {
            if !old_roots.iter().any(|r| r == root) {
                self.scan_root(root);
            }
        }
        if exclusions_changed {
            info!("exclusions changed on reload; rescanning all volumes");
            self.reconcile_all();
        }
        info!("configuration reloaded from {}", path.display());
        Ok(())
    }

    /// Re-probe a volume's mount and mark it online/offline.
    pub fn check_volume(&self, vol: VolumeIdx) -> RootHealth {
        let (name, mount, roots) = {
            let s = self.shared.read();
            let v = s.index.volume_rec(vol);
            (v.name.clone(), v.mount.clone(), v.root_paths.clone())
        };
        if roots.is_empty() {
            return RootHealth::Removed;
        }
        if !volume::mount_point_present(&mount) {
            let mut s = self.shared.write();
            if s.index.volume_rec(vol).state != VolumeState::Offline {
                warn!("volume {name} at {mount} is offline");
            }
            s.index.volume_rec_mut(vol).state = VolumeState::Offline;
            s.index.volume_rec_mut(vol).watching = false;
            drop(s);
            watch::stop_prefix(Path::new(&mount));
            return RootHealth::MountLost;
        }
        // Mount present: verify at least one root still stats.
        let any_ok = roots.iter().any(|r| Path::new(r).metadata().is_ok());
        if !any_ok {
            let mut s = self.shared.write();
            s.index.volume_rec_mut(vol).state = VolumeState::Offline;
            s.index.volume_rec_mut(vol).watching = false;
            drop(s);
            watch::stop_prefix(Path::new(&mount));
            return RootHealth::MountLost;
        }
        let was_offline = self.shared.read().index.volume_state(vol) == VolumeState::Offline;
        if was_offline {
            info!(
                "volume {} is back online; rescanning",
                self.volume_label(vol)
            );
            self.reconcile_volume(vol);
        }
        RootHealth::Ok
    }

    fn volume_label(&self, vol: VolumeIdx) -> String {
        let s = self.shared.read();
        let v = s.index.volume_rec(vol);
        format!("{} ({})", v.name, v.mount)
    }

    /// Ensure inotify watches exist for every directory of a volume.
    fn ensure_watches(&self, vol: VolumeIdx) {
        self.sync_watches(vol);
    }

    // ------------------------------------------------------------- watcher

    /// Establish watches for a volume's directories. Deep, unwatched
    /// directories are reconciled periodically instead (periodic reconcile).
    fn sync_watches(&self, vol: VolumeIdx) {
        let (max_dirs, dirs): (usize, Vec<(NodeId, u32)>) = {
            let s = self.shared.read();
            let max = s.config.max_dirs_per_volume;
            let mut v: Vec<(NodeId, u32)> = s
                .index
                .nodes
                .iter()
                .enumerate()
                .filter(|(i, n)| !s.index.dead[*i] && n.volume == vol && n.kind == EntryKind::Dir)
                .map(|(i, _)| (i as NodeId, 0))
                .collect();
            // Cheap depth pass (children are linked to parents, so walk up).
            for (id, depth) in v.iter_mut() {
                *depth = s.index.depth(*id);
            }
            // Prefer shallow directories when over budget.
            v.sort_by_key(|(_, d)| *d);
            v.truncate(max.max(1));
            (max, v)
        };
        let mut ok = 0usize;
        let mut benign = 0u64;
        let mut fatal = 0u64;
        let mut first_err: Option<String> = None;
        for (id, _) in &dirs {
            let path = {
                let s = self.shared.read();
                s.index.path_of(*id)
            };
            match watch::start_watch_classified(Path::new(&path)) {
                Ok(()) => ok += 1,
                Err(e) => {
                    match e.failure {
                        watch::WatchFailure::Benign => benign += 1,
                        watch::WatchFailure::Fatal => fatal += 1,
                    }
                    if first_err.is_none() {
                        first_err = Some(e.message);
                    }
                }
            }
        }
        if let Some(msg) = &first_err {
            debug!("watch note ({} benign, {} fatal): {msg}", benign, fatal);
        }
        if dirs.len() < self.collect_dirs(vol).len() {
            debug!(
                "volume {} exceeds max_dirs_per_volume={}; relying on periodic reconcile for deeper directories",
                self.volume_label(vol),
                max_dirs
            );
        }
        {
            let mut s = self.shared.write();
            let v = s.index.volume_rec_mut(vol);
            v.watching = ok > 0;
            v.watch_errors = benign + fatal;
            if fatal > 0 {
                debug!(
                    "volume {}: {} fatal watch error(s), {} benign, {} ok",
                    v.name, fatal, benign, ok
                );
            }
            if v.state != VolumeState::Offline {
                // Benign failures (e.g. unreadable directories) are expected
                // and covered by periodic reconciliation; only fatal watch
                // failures degrade the volume.
                v.state = if fatal > 0 {
                    VolumeState::Degraded
                } else {
                    VolumeState::Online
                };
            }
            s.watch_errors.insert(vol, fatal);
        }
        // Drop queued new-dir markers for this volume.
        let stale: Vec<NodeId> = {
            let s = self.shared.read();
            s.index
                .new_dirs
                .iter()
                .copied()
                .filter(|id| s.index.volume_of(*id) == vol)
                .collect()
        };
        if !stale.is_empty() {
            let mut s = self.shared.write();
            s.index.new_dirs.retain(|id| !stale.contains(id));
        }
    }

    /// Stop watches for paths that no longer resolve to a live directory.
    fn prune_watches(&self) {
        let paths = watch::watched_paths();
        let mut dead: Vec<PathBuf> = Vec::new();
        {
            let s = self.shared.read();
            for p in paths {
                match s.index.lookup(&p) {
                    Some(id) if !s.index.is_dead(id) && s.index.kind_of(id) == EntryKind::Dir => {}
                    _ => dead.push(p),
                }
            }
        }
        if !dead.is_empty() {
            debug!("pruning {} stale watch(es)", dead.len());
        }
        for p in dead {
            watch::stop_watch(&p);
        }
    }

    fn collect_dirs(&self, vol: VolumeIdx) -> Vec<NodeId> {
        let s = self.shared.read();
        let mut out = Vec::new();
        for i in 0..s.index.nodes.len() {
            if s.index.dead[i] {
                continue;
            }
            let n = &s.index.nodes[i];
            if n.volume == vol && n.kind == EntryKind::Dir {
                out.push(i as NodeId);
            }
        }
        out
    }

    // ------------------------------------------------------------- updates

    /// Drain filesystem events from every watch and coalesce them into
    /// debounced directory resyncs.
    pub fn drain_events(&self) -> usize {
        let events = watch::drain_all();
        if events.is_empty() {
            return 0;
        }
        let mut vol_to_rescan: FxHashSet<VolumeIdx> = FxHashSet::default();
        let mut dirty_dirs: FxHashSet<NodeId> = FxHashSet::default();
        let mut unknown: Vec<PathBuf> = Vec::new();
        let now = now_secs();

        {
            let mut s = self.shared.write();
            for ev in &events {
                if ev.overflow {
                    if ev.path.as_os_str().is_empty() {
                        // Unattributed overflow: reconcile every online volume.
                        for (i, v) in s.index.volumes.iter().enumerate() {
                            if v.state == VolumeState::Online {
                                vol_to_rescan.insert(VolumeIdx(i as u16));
                            }
                        }
                        continue;
                    }
                    // Map the watch path to a volume by longest root prefix.
                    if let Some(vol) = volume_for_path(&s.index, &ev.path) {
                        warn!(
                            "inotify queue overflow on volume {}; scheduling reconcile",
                            s.index.volume_rec(vol).name
                        );
                        vol_to_rescan.insert(vol);
                    }
                    continue;
                }
                match s.index.lookup(&ev.path) {
                    Some(id) => {
                        let vol = s.index.volume_of(id);
                        if s.index.volume_state(vol) == VolumeState::Offline {
                            continue;
                        }
                        s.index.record_volume_event(vol, now);
                        // Events on a watched directory carry the changed
                        // child as `ev.path`, so the directory to resync is
                        // the parent. Fall back to the node itself for events
                        // about the watched directory.
                        let parent_id = ev
                            .path
                            .parent()
                            .and_then(|p| s.index.lookup(p))
                            .filter(|pid| s.index.kind_of(*pid) == EntryKind::Dir);
                        let dir_id = parent_id.unwrap_or(id);
                        let dir_id = if s.index.kind_of(dir_id) == EntryKind::Dir {
                            dir_id
                        } else {
                            let parent = s.index.nodes[dir_id as usize].parent;
                            if parent == crate::model::NO_PARENT {
                                dir_id
                            } else {
                                parent
                            }
                        };
                        dirty_dirs.insert(dir_id);
                    }
                    None => unknown.push(ev.path.clone()),
                }
            }
            // Apply the debounce bound.
            for id in dirty_dirs {
                s.pending.insert(id);
            }
            for p in unknown {
                // A path we don't know: if it lives under a root, resync its
                // parent directory if that parent is known, else schedule a
                // volume reconcile.
                if let Some(parent) = p.parent() {
                    if let Some(pid) = s.index.lookup(parent) {
                        s.pending.insert(pid);
                        continue;
                    }
                }
                if let Some(vol) = volume_for_path(&s.index, &p) {
                    vol_to_rescan.insert(vol);
                }
            }
            // A backlog this large means many directories are stale; a full
            // reconcile is cheaper and more correct than resyncing them all.
            let max_pending = s.config.max_pending_dirs.max(1);
            if s.pending.len() > max_pending {
                warn!(
                    "pending dirs ({}) exceed max_pending_dirs ({max_pending}); escalating to reconcile",
                    s.pending.len()
                );
                let vols: Vec<VolumeIdx> = (0..s.index.volumes.len())
                    .map(|i| VolumeIdx(i as u16))
                    .filter(|v| s.index.volume_state(*v) != VolumeState::Offline)
                    .collect();
                s.pending.clear();
                for v in vols {
                    vol_to_rescan.insert(v);
                }
            }
        }

        for vol in vol_to_rescan {
            self.reconcile_volume(vol);
        }
        events.len()
    }

    /// Apply pending directory resyncs (called on the debounce tick).
    pub fn apply_pending(&self) {
        let pending: Vec<NodeId> = {
            let s = self.shared.read();
            s.pending.iter().copied().collect()
        };
        if pending.is_empty() {
            return;
        }
        {
            let mut s = self.shared.write();
            for id in pending {
                s.pending.remove(&id);
                if s.index.is_dead(id) {
                    continue;
                }
                if s.index.kind_of(id) != EntryKind::Dir {
                    continue;
                }
                match s.index.resync_dir(id) {
                    Ok(()) => {
                        // Clear the failure counter on success.
                        s.dir_failures.remove(&id);
                    }
                    Err(e) => {
                        let count = s.dir_failures.entry(id).or_insert(0);
                        *count += 1;
                        if *count >= 3 {
                            // Directory vanished; remove it.
                            debug!("resync {e}; removing vanished dir");
                            s.index.remove_subtree(id);
                        }
                    }
                }
            }
        }
        // Watch newly discovered directories (queued during the resyncs).
        let new_dirs: Vec<NodeId> = {
            let mut s = self.shared.write();
            std::mem::take(&mut s.index.new_dirs)
        };
        let mut added = 0u64;
        for id in new_dirs {
            let (path, vol) = {
                let s = self.shared.read();
                if s.index.is_dead(id) {
                    continue;
                }
                (s.index.path_of(id), s.index.volume_of(id))
            };
            if watch::start_watch(Path::new(&path)).is_ok() {
                added += 1;
                let mut s = self.shared.write();
                if let Some(v) = s.index.volumes.get_mut(vol.0 as usize) {
                    v.watching = true;
                }
            }
        }
        if added > 0 {
            debug!("added {} new directory watch(es)", added);
        }
    }

    // -------------------------------------------------------------- upkeep

    pub fn maybe_compact(&self) {
        let interval = {
            let s = self.shared.read();
            Duration::from_secs(s.config.compact_interval_secs.max(1))
        };
        let mut last = self.last_compact.lock();
        if last.elapsed() < interval {
            return;
        }
        *last = Instant::now();
        let mut s = self.shared.write();
        let live = s.index.live_count().max(1) as u64;
        let dead = s.index.dead_count() as u64;
        let threshold = s.config.compact_dead_percent;
        if dead * 100 >= live * threshold {
            let before = s.index.len();
            s.index.compact();
            info!("compacted index: {} -> {} slots", before, s.index.len());
        }
    }

    pub fn maybe_reconcile(&self) {
        let interval = {
            let s = self.shared.read();
            Duration::from_secs(s.config.reconcile_interval_secs.max(30))
        };
        {
            let mut last = self.last_reconcile.lock();
            if last.elapsed() < interval {
                return;
            }
            *last = Instant::now();
        }
        let vols: Vec<VolumeIdx> = {
            let s = self.shared.read();
            (0..s.index.volumes.len())
                .map(|i| VolumeIdx(i as u16))
                .collect()
        };
        for vol in vols {
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            let online = {
                let s = self.shared.read();
                s.index.volume_state(vol) == VolumeState::Online
            };
            if online {
                self.reconcile_volume(vol);
            }
        }
    }

    /// Periodic online/offline sweep for all volumes.
    pub fn check_volumes(&self) {
        let vols: Vec<VolumeIdx> = {
            let s = self.shared.read();
            (0..s.index.volumes.len())
                .map(|i| VolumeIdx(i as u16))
                .collect()
        };
        for vol in vols {
            self.check_volume(vol);
        }
    }

    // ------------------------------------------------------------- queries

    pub fn scan_state(&self) -> ScanState {
        let s = self.shared.read();
        let initial = !s.initial_scan_done;
        let scanning = initial || !s.scanning.is_empty();
        let pending_dirs = s.pending.len();
        let mut offline = Vec::new();
        let mut degraded = false;
        for v in &s.index.volumes {
            if v.state == VolumeState::Offline {
                offline.push(v.id);
            }
            if v.state == VolumeState::Degraded {
                degraded = true;
            }
        }
        let overflowed = !s.overflowed.is_empty();
        let stale = scanning || pending_dirs > 0 || !offline.is_empty() || overflowed || degraded;
        let complete = !stale;
        let note = if initial {
            Some("initial scan in progress; results are partial".to_string())
        } else if !s.scanning.is_empty() {
            Some("a scan is in progress; results may be incomplete".to_string())
        } else if overflowed {
            Some("a watcher overflowed; a reconcile is scheduled".to_string())
        } else if pending_dirs > 0 {
            Some(format!("{pending_dirs} directory update(s) pending"))
        } else if degraded {
            Some("one or more directories could not be watched".to_string())
        } else if !offline.is_empty() {
            Some("one or more volumes are offline".to_string())
        } else {
            None
        };
        ScanState {
            complete,
            scanning,
            stale,
            offline_volumes: offline,
            pending_dirs,
            last_scan_finished: s.last_scan_finished,
            note,
        }
    }

    pub fn search(&self, q: &SearchQuery) -> anyhow::Result<SearchResponse> {
        let started = Instant::now();

        // Plan under a read lock (cheap), then act outside any index lock.
        let (plan, generation, default_limit) = {
            let s = self.shared.read();
            let plan = s.query_cache.lock().plan(q);
            (plan, s.index.generation, s.config.max_results)
        };

        let (hits, total) = match plan {
            query::CachePlan::Uncacheable => {
                let s = self.shared.read();
                query::search(&s.index, q, default_limit)?
            }
            query::CachePlan::Build => {
                let key = query::filter_key_for(q);
                // First request for this query: build once. No index lock is held
                // during the scan, so an in-progress scan can still make progress.
                let (ids, sorted_len) = {
                    let s = self.shared.read();
                    query::build_sorted(&s.index, &key, 0)
                };
                if ids.is_none() {
                    // Race: the query became uncacheable / invalid.
                    let s = self.shared.read();
                    query::search(&s.index, q, default_limit)?
                } else {
                    let s = self.shared.read();
                    {
                        let mut cache = s.query_cache.lock();
                        cache.store_built(key, generation, ids, sorted_len);
                    }
                    let mut cache = s.query_cache.lock();
                    match cache.ready_page(&s.index, q, default_limit) {
                        Some(page) => page,
                        None => {
                            drop(cache);
                            query::search(&s.index, q, default_limit)?
                        }
                    }
                }
            }
            query::CachePlan::Ready => {
                let s = self.shared.read();
                let mut cache = s.query_cache.lock();
                match cache.ready_page(&s.index, q, default_limit) {
                    Some(page) => page,
                    None => {
                        drop(cache);
                        query::search(&s.index, q, default_limit)?
                    }
                }
            }
        };

        let limit = if q.limit == 0 { default_limit } else { q.limit };
        Ok(SearchResponse {
            results: hits,
            total,
            offset: q.offset,
            limit,
            took_ms: started.elapsed().as_millis() as u64,
            scan_state: self.scan_state(),
        })
    }

    pub fn count(&self, q: &SearchQuery) -> anyhow::Result<CountResponse> {
        let started = Instant::now();
        let count = {
            let s = self.shared.read();
            query::count(&s.index, q)?
        };
        Ok(CountResponse {
            count,
            took_ms: started.elapsed().as_millis() as u64,
            scan_state: self.scan_state(),
        })
    }

    pub fn status(&self) -> StatusResponse {
        let s = self.shared.read();
        let totals = s.index.totals;
        let roots = s
            .config
            .roots
            .iter()
            .map(|r| {
                let path = r.resolved().to_string_lossy().to_string();
                let vol = volume::probe(&r.resolved()).map(|v| v.id).unwrap_or(0);
                let (state, exists) = match s.index.volume_by_id.get(&vol) {
                    Some(&vi) => (s.index.volume_state(vi), r.resolved().exists()),
                    None => (VolumeState::Offline, r.resolved().exists()),
                };
                RootStatus {
                    path,
                    volume: vol,
                    state,
                    exists,
                }
            })
            .collect();
        let volumes = s
            .index
            .volumes
            .iter()
            .map(|v| VolumeStatus {
                id: v.id,
                name: v.name.clone(),
                mount: v.mount.clone(),
                roots: v.root_paths.clone(),
                state: v.state,
                watching: v.watching,
                watch_errors: v.watch_errors,
                totals: v.totals,
                last_scan_finished: v.last_scan_finished,
                last_event: v.last_event,
            })
            .collect();
        drop(s);
        StatusResponse {
            version: VERSION.to_string(),
            pid: self.pid,
            uptime_secs: self.start.elapsed().as_secs(),
            totals,
            roots,
            volumes,
            scan_state: self.scan_state(),
            config_path: self.config_path().to_string_lossy().to_string(),
        }
    }

    /// Run the background maintenance loop until shutdown is requested.
    pub fn maintenance_loop(self: &Arc<Engine>) {
        let debounce = {
            let s = self.shared.read();
            Duration::from_millis(s.config.debounce_ms.max(20))
        };
        let tick = Duration::from_millis(debounce.as_millis().min(250) as u64)
            .max(Duration::from_millis(30));
        let mut last_check = Instant::now();
        while !self.shutdown.load(Ordering::SeqCst) {
            std::thread::sleep(tick);
            self.drain_events();
            self.apply_pending();
            if last_prune_elapsed(&self.last_prune, Duration::from_secs(5)) {
                self.prune_watches();
            }
            if last_check.elapsed() >= Duration::from_secs(15) {
                last_check = Instant::now();
                self.check_volumes();
            }
            self.maybe_compact();
            self.maybe_reconcile();
            self.refresh_query_cache();
        }
        debug!("maintenance loop exiting");
    }

    /// Rebuild the cached query's match set after index churn, off the request
    /// path. Keeping the snapshot within one maintenance tick of the index means
    /// a page request is never blocked by a full re-filter/re-sort.
    ///
    /// Held to a few rebuilds per second: a filename index does not need to be
    /// pixel-fresh, and the alternative (rebuilding on every churn) would burn
    /// CPU for no visible benefit.
    fn refresh_query_cache(&self) {
        if !last_prune_elapsed(&self.last_cache_refresh, Duration::from_millis(250)) {
            return;
        }
        // Skip while the initial scan is writing the index from its own thread;
        // once it is done the maintenance thread is the only writer, so holding
        // the read lock across the rebuild cannot stall scan progress.
        let (target, generation) = {
            let s = self.shared.read();
            if !s.initial_scan_done {
                return;
            }
            let cache = s.query_cache.lock();
            (cache.refresh_target(s.index.generation), s.index.generation)
        };
        let Some((key, sorted_len)) = target else {
            return;
        };

        // Build the fresh match set while holding no index lock; queries share
        // the read lock, and after startup no other thread writes.
        let (ids, sorted_len) = {
            let s = self.shared.read();
            query::build_sorted(&s.index, &key, sorted_len)
        };
        if ids.is_none() {
            return; // became uncacheable; the next request falls back
        }
        let s = self.shared.read();
        s.query_cache
            .lock()
            .store_built(key, generation, ids, sorted_len);
    }
}

/// Find the volume owning an absolute path by longest root prefix.
fn volume_for_path(index: &Index, path: &Path) -> Option<VolumeIdx> {
    let mut best: Option<(usize, VolumeIdx)> = None;
    for root in &index.roots {
        let rp = Path::new(&root.path);
        if path == rp || path.starts_with(rp) {
            let len = rp.as_os_str().len();
            if best.map(|(l, _)| len > l).unwrap_or(true) {
                best = Some((len, root.volume));
            }
        }
    }
    best.map(|(_, v)| v)
}

/// Shared counter used by clients (kept public for future metrics).
pub static QUERY_COUNT: AtomicU64 = AtomicU64::new(0);

/// True when at least `interval` has elapsed since `last`, updating it.
fn last_prune_elapsed(last: &Mutex<Instant>, interval: Duration) -> bool {
    let mut l = last.lock();
    if l.elapsed() >= interval {
        *l = Instant::now();
        true
    } else {
        false
    }
}

/// Build an engine without scanning (used by tests).
pub fn empty_engine() -> Arc<Engine> {
    let config = Config {
        roots: Vec::new(),
        ..Default::default()
    };
    Engine::new(config, PathBuf::new()).expect("engine")
}
