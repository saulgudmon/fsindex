//! Shared data model: entries, queries, results and status payloads.
//!
//! Serializable for configuration, diagnostics and future headless consumers.

use serde::{Deserialize, Serialize};

/// A volume is identified by its device id (`st_dev`). Stable for the life of
/// a boot; the mount point/name is carried alongside for humans.
pub type VolumeId = u64;

/// Index into `Index::entries`.
pub type NodeId = u32;

/// Sentinel parent for volume roots.
pub const NO_PARENT: NodeId = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

impl EntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::File => "file",
            EntryKind::Dir => "dir",
            EntryKind::Symlink => "symlink",
            EntryKind::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "file" | "files" | "f" => Some(EntryKind::File),
            "dir" | "dirs" | "directory" | "directories" | "folder" | "d" => Some(EntryKind::Dir),
            "symlink" | "link" | "l" => Some(EntryKind::Symlink),
            "other" => Some(EntryKind::Other),
            _ => None,
        }
    }

    /// Rank used for stable `kind` sorting.
    pub fn rank(self) -> u8 {
        match self {
            EntryKind::Dir => 0,
            EntryKind::File => 1,
            EntryKind::Symlink => 2,
            EntryKind::Other => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortKey {
    #[default]
    Name,
    Path,
    Size,
    Mtime,
    Kind,
    Extension,
}

impl SortKey {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "name" => Some(SortKey::Name),
            "path" => Some(SortKey::Path),
            "size" => Some(SortKey::Size),
            "mtime" | "modified" | "time" => Some(SortKey::Mtime),
            "kind" | "type" => Some(SortKey::Kind),
            "ext" | "extension" => Some(SortKey::Extension),
            _ => None,
        }
    }
}

/// A search request. All filters are ANDed. Empty filter collections mean
/// "no constraint".
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchQuery {
    /// Case-insensitive substring match against the file name.
    pub name: Option<String>,
    /// Case-insensitive substring match against the full path.
    pub path: Option<String>,
    /// Interpret `name`/`path` as a regular expression instead of a substring.
    pub regex: bool,
    /// Match `name`/`path` case-sensitively (default is case-insensitive).
    pub case_sensitive: bool,
    /// Match the whole path instead of just the file name (`name`/`path`
    /// both apply to the full path).
    pub search_path: bool,
    /// Restrict to these volume ids.
    pub volumes: Vec<VolumeId>,
    /// Restrict to volumes whose name/mount contains any of these strings.
    pub volume_names: Vec<String>,
    /// Restrict by entry kind.
    pub kind: Option<EntryKind>,
    /// Restrict to files with one of these extensions (with or without a dot).
    pub extensions: Vec<String>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    /// Unix seconds. `mtime >= modified_after`.
    pub modified_after: Option<i64>,
    /// Unix seconds. `mtime <= modified_before`.
    pub modified_before: Option<i64>,
    pub sort: SortKey,
    pub desc: bool,
    pub offset: u64,
    /// 0 means "engine default" (`Config::max_results`).
    pub limit: u64,
    /// Include entries from volumes currently marked offline.
    pub include_offline: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchHit {
    pub path: String,
    pub name: String,
    pub volume: VolumeId,
    pub volume_name: String,
    pub kind: EntryKind,
    pub size: u64,
    /// Unix seconds.
    pub mtime: i64,
    /// Unix nanoseconds (kept for a future history/diff layer).
    pub mtime_ns: i64,
    pub inode: u64,
    /// Depth below the configured root (0 = the root itself).
    pub depth: u32,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Totals {
    pub entries: u64,
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    pub bytes: u64,
}

impl Totals {
    pub fn add_kind(&mut self, kind: EntryKind, size: u64) {
        self.entries += 1;
        match kind {
            EntryKind::File => {
                self.files += 1;
                self.bytes += size;
            }
            EntryKind::Dir => self.dirs += 1,
            EntryKind::Symlink => self.symlinks += 1,
            EntryKind::Other => {}
        }
    }

    pub fn sub_kind(&mut self, kind: EntryKind, size: u64) {
        self.entries = self.entries.saturating_sub(1);
        match kind {
            EntryKind::File => {
                self.files = self.files.saturating_sub(1);
                self.bytes = self.bytes.saturating_sub(size);
            }
            EntryKind::Dir => self.dirs = self.dirs.saturating_sub(1),
            EntryKind::Symlink => self.symlinks = self.symlinks.saturating_sub(1),
            EntryKind::Other => {}
        }
    }

    pub fn merge(&mut self, other: &Totals) {
        self.entries += other.entries;
        self.files += other.files;
        self.dirs += other.dirs;
        self.symlinks += other.symlinks;
        self.bytes += other.bytes;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VolumeState {
    Online,
    Offline,
    Scanning,
    Degraded,
}

impl VolumeState {
    pub fn as_str(self) -> &'static str {
        match self {
            VolumeState::Online => "online",
            VolumeState::Offline => "offline",
            VolumeState::Scanning => "scanning",
            VolumeState::Degraded => "degraded",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VolumeStatus {
    pub id: VolumeId,
    pub name: String,
    pub mount: String,
    pub roots: Vec<String>,
    pub state: VolumeState,
    pub watching: bool,
    pub watch_errors: u64,
    pub totals: Totals,
    pub last_scan_finished: Option<i64>,
    pub last_event: Option<i64>,
}

/// Overall freshness of the index. Agents should consult this before trusting
/// an empty result set.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScanState {
    /// True only when nothing is scanning, no updates are pending, and every
    /// volume is online and watched.
    pub complete: bool,
    pub scanning: bool,
    /// The index may lag the filesystem (pending updates or a recent overflow).
    pub stale: bool,
    pub offline_volumes: Vec<VolumeId>,
    pub pending_dirs: usize,
    pub last_scan_finished: Option<i64>,
    pub note: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchResponse {
    pub results: Vec<SearchHit>,
    pub total: u64,
    pub offset: u64,
    pub limit: u64,
    pub took_ms: u64,
    pub scan_state: ScanState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CountResponse {
    pub count: u64,
    pub took_ms: u64,
    pub scan_state: ScanState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RootStatus {
    pub path: String,
    pub volume: VolumeId,
    pub state: VolumeState,
    pub exists: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StatusResponse {
    pub version: String,
    pub pid: u32,
    pub uptime_secs: u64,
    pub totals: Totals,
    pub roots: Vec<RootStatus>,
    pub volumes: Vec<VolumeStatus>,
    pub scan_state: ScanState,
    pub config_path: String,
}
