//! The in-memory index: a compact node arena plus an interned name table.
//!
//! Each entry stores only `(parent, name)` plus metadata; full paths are
//! reconstructed on demand. Children of a directory are kept in a sorted list
//! keyed by interned name so watching can resolve a changed path to a node in
//! `O(depth * log k)` without a global path map.

use crate::model::{EntryKind, NodeId, Totals, VolumeId, VolumeState, NO_PARENT};
use crate::paths::{normalize_ext, Exclusions};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

/// Index into `Index::volumes`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VolumeIdx(pub u16);

pub type NameId = u32;

#[derive(Clone, Debug)]
pub struct NameEntry {
    pub original: std::sync::Arc<str>,
    pub lower: std::sync::Arc<str>,
}

/// String interner whose entries are never removed for the life of the index.
#[derive(Default)]
pub struct Interner {
    map: FxHashMap<&'static str, NameId>,
    entries: Vec<NameEntry>,
}

impl Interner {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn intern(&mut self, s: &str) -> NameId {
        if let Some(&id) = self.map.get(s) {
            return id;
        }
        let lower = s.to_lowercase();
        self.entries.push(NameEntry {
            original: s.into(),
            lower: lower.into(),
        });
        let id = (self.entries.len() - 1) as NameId;
        // SAFETY: `entries` is append-only, so the heap allocation behind this
        // `Arc<str>` remains valid and stable for the lifetime of the interner.
        let stored: &'static str = unsafe {
            let s: &str = &self.entries[id as usize].original;
            std::mem::transmute::<&str, &'static str>(s)
        };
        self.map.insert(stored, id);
        id
    }

    pub fn get(&self, s: &str) -> Option<NameId> {
        self.map.get(s).copied()
    }

    pub fn original(&self, id: NameId) -> &str {
        &self.entries[id as usize].original
    }

    pub fn lower(&self, id: NameId) -> &str {
        &self.entries[id as usize].lower
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Node {
    pub parent: NodeId,
    pub name: NameId,
    /// Lowercased extension without the dot; empty when there is none.
    pub ext: NameId,
    pub volume: VolumeIdx,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime: i64,
    pub mtime_ns: i64,
    pub inode: u64,
}

#[derive(Clone, Debug)]
pub struct RootRec {
    pub id: NodeId,
    pub volume: VolumeIdx,
    pub path: String,
}

#[derive(Clone, Debug)]
pub struct VolumeRec {
    pub id: VolumeId,
    pub name: String,
    pub mount: String,
    pub state: VolumeState,
    pub watching: bool,
    pub watch_errors: u64,
    pub totals: Totals,
    pub last_scan_finished: Option<i64>,
    pub last_event: Option<i64>,
    pub roots: Vec<NodeId>,
    pub root_paths: Vec<String>,
}

/// A freshly scanned, single-root index fragment awaiting merge.
pub struct Fragment {
    pub index: Index,
    pub root: NodeId,
    pub root_path: PathBuf,
}

#[derive(Clone, Copy, Debug)]
pub struct EntryMeta {
    pub kind: EntryKind,
    pub size: u64,
    pub mtime: i64,
    pub mtime_ns: i64,
    pub inode: u64,
}

impl EntryMeta {
    pub fn from_metadata(meta: &std::fs::Metadata) -> Self {
        let ft = meta.file_type();
        let kind = if ft.is_dir() {
            EntryKind::Dir
        } else if ft.is_file() {
            EntryKind::File
        } else if ft.is_symlink() {
            EntryKind::Symlink
        } else {
            EntryKind::Other
        };
        let (mtime, mtime_ns) = system_time_parts(meta);
        EntryMeta {
            kind,
            size: meta.len(),
            mtime,
            mtime_ns,
            inode: inode_of(meta),
        }
    }
}

fn system_time_parts(meta: &std::fs::Metadata) -> (i64, i64) {
    match meta.modified() {
        Ok(t) => match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => (d.as_secs() as i64, d.subsec_nanos() as i64),
            Err(e) => {
                let d = e.duration();
                (-(d.as_secs() as i64), -(d.subsec_nanos() as i64))
            }
        },
        Err(_) => (0, 0),
    }
}

#[cfg(unix)]
fn inode_of(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}

#[cfg(not(unix))]
fn inode_of(_meta: &std::fs::Metadata) -> u64 {
    0
}

#[cfg(unix)]
pub fn dev_of(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.dev()
}

#[cfg(not(unix))]
pub fn dev_of(_meta: &std::fs::Metadata) -> u64 {
    0
}

pub struct Index {
    pub names: Interner,
    pub nodes: Vec<Node>,
    pub dead: Vec<bool>,
    pub children: FxHashMap<NodeId, Vec<(NameId, NodeId)>>,
    pub roots: Vec<RootRec>,
    pub volumes: Vec<VolumeRec>,
    pub volume_by_id: FxHashMap<VolumeId, VolumeIdx>,
    pub totals: Totals,
    pub exclusions: Exclusions,
    /// Bumped on every mutation that changes visible results.
    pub generation: u64,
    /// Directory nodes created since the last drain; the engine turns these
    /// into new inotify watches.
    pub new_dirs: Vec<NodeId>,
}

impl Default for Index {
    fn default() -> Self {
        Self::new(Exclusions::default())
    }
}

impl Index {
    pub fn new(exclusions: Exclusions) -> Self {
        Index {
            names: Interner::default(),
            nodes: Vec::new(),
            dead: Vec::new(),
            children: FxHashMap::default(),
            roots: Vec::new(),
            volumes: Vec::new(),
            volume_by_id: FxHashMap::default(),
            totals: Totals::default(),
            exclusions,
            generation: 0,
            new_dirs: Vec::new(),
        }
    }

    /// Query-only immutable copy. The interner's borrowed lookup keys and the
    /// mutation-only child map are deliberately not copied. Names are shared.
    pub(crate) fn frozen_copy(&self) -> Self {
        Self {
            names: Interner {
                entries: self.names.entries.clone(),
                map: FxHashMap::default(),
            },
            nodes: self.nodes.clone(),
            dead: self.dead.clone(),
            children: FxHashMap::default(),
            roots: self.roots.clone(),
            volumes: self.volumes.clone(),
            volume_by_id: self.volume_by_id.clone(),
            totals: self.totals,
            exclusions: self.exclusions.clone(),
            generation: self.generation,
            new_dirs: Vec::new(),
        }
    }

    /// Event timestamps affect freshness, not searchable rows.
    pub fn record_volume_event(&mut self, volume: VolumeIdx, time: i64) {
        self.volumes[volume.0 as usize].last_event = Some(time);
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn live_count(&self) -> usize {
        self.nodes.len() - self.dead_count()
    }

    pub fn dead_count(&self) -> usize {
        self.dead.iter().filter(|d| **d).count()
    }

    pub fn is_dead(&self, id: NodeId) -> bool {
        self.dead.get(id as usize).copied().unwrap_or(true)
    }

    pub fn node(&self, id: NodeId) -> Option<&Node> {
        if self.is_dead(id) {
            None
        } else {
            self.nodes.get(id as usize)
        }
    }

    pub fn name_of(&self, id: NodeId) -> &str {
        self.names.original(self.nodes[id as usize].name)
    }

    pub fn name_lower(&self, id: NodeId) -> &str {
        self.names.lower(self.nodes[id as usize].name)
    }

    pub fn ext_of(&self, id: NodeId) -> &str {
        self.names.original(self.nodes[id as usize].ext)
    }

    pub fn kind_of(&self, id: NodeId) -> EntryKind {
        self.nodes[id as usize].kind
    }

    pub fn volume_of(&self, id: NodeId) -> VolumeIdx {
        self.nodes[id as usize].volume
    }

    pub fn volume_rec(&self, idx: VolumeIdx) -> &VolumeRec {
        &self.volumes[idx.0 as usize]
    }

    /// Volume state/name changes affect search results, even without node edits.
    pub fn volume_rec_mut(&mut self, idx: VolumeIdx) -> &mut VolumeRec {
        self.generation += 1;
        &mut self.volumes[idx.0 as usize]
    }

    pub fn volume_state(&self, idx: VolumeIdx) -> VolumeState {
        self.volumes[idx.0 as usize].state
    }

    pub fn roots_of_volume(&self, idx: VolumeIdx) -> &[String] {
        &self.volumes[idx.0 as usize].root_paths
    }

    /// Depth of a node below its root (0 = the root itself).
    pub fn depth(&self, id: NodeId) -> u32 {
        let mut d = 0u32;
        let mut cur = id;
        loop {
            let n = &self.nodes[cur as usize];
            if n.parent == NO_PARENT {
                break;
            }
            d += 1;
            cur = n.parent;
        }
        d
    }

    /// Register (or fetch) a volume, keyed by device id.
    pub fn ensure_volume(&mut self, id: VolumeId, name: &str, mount: &str) -> VolumeIdx {
        if let Some(&idx) = self.volume_by_id.get(&id) {
            return idx;
        }
        let idx = VolumeIdx(self.volumes.len() as u16);
        self.volumes.push(VolumeRec {
            id,
            name: name.to_string(),
            mount: mount.to_string(),
            state: VolumeState::Scanning,
            watching: false,
            watch_errors: 0,
            totals: Totals::default(),
            last_scan_finished: None,
            last_event: None,
            roots: Vec::new(),
            root_paths: Vec::new(),
        });
        self.volume_by_id.insert(id, idx);
        idx
    }

    fn push_node(
        &mut self,
        meta: EntryMeta,
        parent: NodeId,
        name: NameId,
        ext: NameId,
        vol: VolumeIdx,
    ) -> NodeId {
        let id = self.nodes.len() as NodeId;
        self.nodes.push(Node {
            parent,
            name,
            ext,
            volume: vol,
            kind: meta.kind,
            size: meta.size,
            mtime: meta.mtime,
            mtime_ns: meta.mtime_ns,
            inode: meta.inode,
        });
        self.dead.push(false);
        id
    }

    fn add_totals(&mut self, vol: VolumeIdx, kind: EntryKind, size: u64) {
        self.totals.add_kind(kind, size);
        self.volumes[vol.0 as usize].totals.add_kind(kind, size);
    }

    fn sub_totals(&mut self, vol: VolumeIdx, kind: EntryKind, size: u64) {
        self.totals.sub_kind(kind, size);
        self.volumes[vol.0 as usize].totals.sub_kind(kind, size);
    }

    fn link_child(&mut self, parent: NodeId, name: NameId, id: NodeId) {
        let list = self.children.entry(parent).or_default();
        match list.binary_search_by_key(&name, |(n, _)| *n) {
            Ok(pos) => list[pos] = (name, id),
            Err(pos) => list.insert(pos, (name, id)),
        }
    }

    fn unlink_child(&mut self, parent: NodeId, name: NameId) {
        if let Some(list) = self.children.get_mut(&parent) {
            if let Ok(pos) = list.binary_search_by_key(&name, |(n, _)| *n) {
                list.remove(pos);
            }
        }
    }

    pub fn child_by_name(&self, parent: NodeId, name: NameId) -> Option<NodeId> {
        let list = self.children.get(&parent)?;
        list.binary_search_by_key(&name, |(n, _)| *n)
            .ok()
            .map(|pos| list[pos].1)
    }

    pub fn direct_children(&self, parent: NodeId) -> &[(NameId, NodeId)] {
        self.children
            .get(&parent)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Create or update a child of `parent`.
    pub fn upsert_child(&mut self, parent: NodeId, name: &str, meta: EntryMeta) -> NodeId {
        let name_id = self.names.intern(name);
        if let Some(existing) = self.child_by_name(parent, name_id) {
            self.update_meta(existing, meta);
            return existing;
        }
        let ext = self.names.intern(normalize_ext(name).as_ref());
        let vol = self.nodes[parent as usize].volume;
        let id = self.push_node(meta, parent, name_id, ext, vol);
        self.link_child(parent, name_id, id);
        self.add_totals(vol, meta.kind, meta.size);
        if meta.kind == EntryKind::Dir {
            self.new_dirs.push(id);
        }
        self.generation += 1;
        id
    }

    pub fn update_meta(&mut self, id: NodeId, meta: EntryMeta) {
        let old = self.nodes[id as usize];
        if old.size == meta.size
            && old.mtime == meta.mtime
            && old.mtime_ns == meta.mtime_ns
            && old.inode == meta.inode
            && old.kind == meta.kind
        {
            return;
        }
        // Replace old contribution, then add the new one.
        self.sub_totals(old.volume, old.kind, old.size);
        self.add_totals(old.volume, meta.kind, meta.size);
        let n = &mut self.nodes[id as usize];
        n.size = meta.size;
        n.mtime = meta.mtime;
        n.mtime_ns = meta.mtime_ns;
        n.inode = meta.inode;
        n.kind = meta.kind;
        self.generation += 1;
    }

    /// Tombstone a node and all of its descendants.
    pub fn remove_subtree(&mut self, id: NodeId) {
        if self.is_dead(id) {
            return;
        }
        let mut stack = vec![id];
        // Collect first (post-order not required for tombstones).
        let mut order = Vec::new();
        while let Some(cur) = stack.pop() {
            if self.is_dead(cur) {
                continue;
            }
            order.push(cur);
            if let Some(list) = self.children.get(&cur) {
                for (_, child) in list {
                    stack.push(*child);
                }
            }
        }
        for cur in order.into_iter().rev() {
            if self.is_dead(cur) {
                continue;
            }
            let n = self.nodes[cur as usize];
            self.unlink_child(n.parent, n.name);
            self.children.remove(&cur);
            self.dead[cur as usize] = true;
            self.sub_totals(n.volume, n.kind, n.size);
        }
        self.generation += 1;
    }

    /// Mark every node of a volume dead (used before a full volume rescan).
    pub fn clear_volume(&mut self, vol: VolumeIdx) {
        let root_ids: Vec<NodeId> = self.volumes[vol.0 as usize].roots.clone();
        for id in root_ids {
            self.remove_subtree(id);
        }
        // Remove any orphan nodes still tagged with this volume.
        let orphans: Vec<NodeId> = self
            .nodes
            .iter()
            .enumerate()
            .filter(|(i, n)| n.volume == vol && !self.dead[*i])
            .map(|(i, _)| i as NodeId)
            .collect();
        for id in orphans {
            let n = self.nodes[id as usize];
            self.unlink_child(n.parent, n.name);
            self.dead[id as usize] = true;
            self.sub_totals(vol, n.kind, n.size);
        }
        self.children
            .retain(|k, _| self.nodes[*k as usize].volume != vol);
        let v = &mut self.volumes[vol.0 as usize];
        v.roots.clear();
        v.root_paths.clear();
        v.totals = Totals::default();
        self.roots.retain(|r| r.volume != vol);
        self.generation += 1;
    }

    /// Remove a single root subtree (used when a root no longer exists).
    pub fn remove_root(&mut self, root_id: NodeId) {
        if self.is_dead(root_id) {
            return;
        }
        let vol = self.nodes[root_id as usize].volume;
        let path = self.path_of(root_id);
        self.remove_subtree(root_id);
        self.roots.retain(|r| r.id != root_id);
        let v = &mut self.volumes[vol.0 as usize];
        v.roots.retain(|id| *id != root_id);
        v.root_paths.retain(|p| *p != path);
        self.generation += 1;
    }

    /// Register a root directory node (creating it if needed) and return its
    /// id. Used by the incremental scanner.
    pub fn ensure_root(&mut self, vol: VolumeIdx, root_path: &str) -> NodeId {
        if let Some(r) = self.roots.iter().find(|r| r.path == root_path) {
            return r.id;
        }
        let name_id = self.names.intern(root_path);
        let empty = self.names.intern("");
        let id = self.push_node(
            EntryMeta {
                kind: EntryKind::Dir,
                size: 0,
                mtime: 0,
                mtime_ns: 0,
                inode: 0,
            },
            NO_PARENT,
            name_id,
            empty,
            vol,
        );
        self.roots.push(RootRec {
            id,
            volume: vol,
            path: root_path.to_string(),
        });
        let v = &mut self.volumes[vol.0 as usize];
        v.roots.push(id);
        if !v.root_paths.iter().any(|p| p == root_path) {
            v.root_paths.push(root_path.to_string());
        }
        self.add_totals(vol, EntryKind::Dir, 0);
        self.generation += 1;
        id
    }

    /// Insert a batch of scanned entries under the live index, building
    /// children incrementally so queries see partial results. `first_gid` is
    /// the global scan id of the first entry; each entry's `parent` is a
    /// global scan id (`None` = the scanned root). Returns the new node ids.
    pub fn insert_scan_batch(
        &mut self,
        root: NodeId,
        first_gid: usize,
        batch: &[crate::scan::ScannedEntry],
    ) -> Vec<NodeId> {
        let mut ids: Vec<NodeId> = Vec::with_capacity(batch.len());
        for e in batch {
            let parent = match e.parent {
                None => root,
                Some(p) => {
                    if p < first_gid {
                        root
                    } else {
                        ids[p - first_gid]
                    }
                }
            };
            let name_id = self.names.intern(&e.name);
            let ext = self.names.intern(normalize_ext(&e.name).as_ref());
            let vol = self.nodes[parent as usize].volume;
            let id = self.push_node(e.meta, parent, name_id, ext, vol);
            self.link_child(parent, name_id, id);
            self.add_totals(vol, e.meta.kind, e.meta.size);
            if e.meta.kind == EntryKind::Dir {
                self.new_dirs.push(id);
            }
            ids.push(id);
        }
        self.generation += 1;
        ids
    }

    /// Resolve an absolute path to a live node.
    pub fn lookup(&self, path: &Path) -> Option<NodeId> {
        let mut best: Option<&RootRec> = None;
        for root in &self.roots {
            let rp = Path::new(&root.path);
            if (path == rp || path.starts_with(rp))
                && best
                    .map(|b| rp.as_os_str().len() > Path::new(&b.path).as_os_str().len())
                    .unwrap_or(true)
            {
                best = Some(root);
            }
        }
        let root = best?;
        if path == Path::new(&root.path) {
            return Some(root.id);
        }
        let rel = path.strip_prefix(&root.path).ok()?;
        let mut cur = root.id;
        for comp in rel.components() {
            let s = comp.as_os_str().to_str()?;
            let name_id = self.names.get(s)?;
            cur = self.child_by_name(cur, name_id)?;
        }
        if self.is_dead(cur) {
            None
        } else {
            Some(cur)
        }
    }

    /// Reconstruct the absolute path for a node.
    pub fn path_of(&self, id: NodeId) -> String {
        let mut chain: Vec<NodeId> = Vec::new();
        let mut cur = id;
        loop {
            chain.push(cur);
            let n = &self.nodes[cur as usize];
            if n.parent == NO_PARENT {
                break;
            }
            cur = n.parent;
        }
        chain.reverse();
        let mut out = String::with_capacity(64);
        for (i, nid) in chain.iter().enumerate() {
            let n = &self.nodes[*nid as usize];
            let name = self.names.original(n.name);
            if i == 0 {
                out.push_str(name); // root stores its absolute path
            } else {
                if !out.ends_with('/') {
                    out.push('/');
                }
                out.push_str(name);
            }
        }
        out
    }

    /// Rebuild a directory from the filesystem, adding/refreshing entries and
    /// removing ones that disappeared. Newly discovered subdirectories are
    /// scanned recursively.
    pub fn resync_dir(&mut self, dir: NodeId) -> io::Result<()> {
        if self.is_dead(dir) || self.nodes[dir as usize].kind != EntryKind::Dir {
            return Ok(());
        }
        let dir_path = PathBuf::from(self.path_of(dir));
        let existing: Vec<(NameId, NodeId)> = self.direct_children(dir).to_vec();
        let mut seen: FxHashSet<NameId> = FxHashSet::default();

        let rd = std::fs::read_dir(&dir_path)?;

        for entry in rd.flatten() {
            let name = entry.file_name();
            let name = match name.to_str() {
                Some(s) => s,
                None => continue,
            };
            let child_path = dir_path.join(name);
            if self.exclusions.excludes(&child_path, name, false) {
                continue;
            }
            let meta = match std::fs::symlink_metadata(&child_path) {
                Ok(m) => EntryMeta::from_metadata(&m),
                Err(_) => continue,
            };
            match self.names.get(name) {
                Some(nid) => match self.child_by_name(dir, nid) {
                    Some(existing_id) => {
                        seen.insert(nid);
                        let was_dir = self.nodes[existing_id as usize].kind == EntryKind::Dir;
                        self.update_meta(existing_id, meta);
                        if meta.kind == EntryKind::Dir && !was_dir {
                            // Type changed; children map is stale.
                            self.children.remove(&existing_id);
                        }
                    }
                    None => {
                        let nid = self.names.intern(name);
                        seen.insert(nid);
                        let new_id = self.upsert_child(dir, name, meta);
                        if meta.kind == EntryKind::Dir {
                            self.scan_subtree(new_id);
                        }
                    }
                },
                None => {
                    let nid = self.names.intern(name);
                    seen.insert(nid);
                    let new_id = self.upsert_child(dir, name, meta);
                    if meta.kind == EntryKind::Dir {
                        self.scan_subtree(new_id);
                    }
                }
            }
        }

        for (nid, child) in existing {
            if !seen.contains(&nid) {
                self.remove_subtree(child);
            }
        }
        Ok(())
    }

    /// Recursively index a directory node that was just added.
    pub fn scan_subtree(&mut self, dir: NodeId) {
        if self.is_dead(dir) {
            return;
        }
        let root_path = PathBuf::from(self.path_of(dir));
        // Clone exclusions so the walker closure does not borrow `self` while
        // the loop mutates the index.
        let excl = self.exclusions.clone();
        let mut stack: Vec<(usize, NodeId)> = vec![(0, dir)];
        let walker = walkdir::WalkDir::new(&root_path)
            .follow_links(false)
            .into_iter()
            .filter_entry(move |e| {
                if e.depth() == 0 {
                    return true;
                }
                let name = e.file_name().to_string_lossy();
                !excl.excludes(e.path(), &name, e.file_type().is_dir())
            });
        for entry in walker.flatten() {
            if entry.depth() == 0 {
                continue;
            }
            let name = match entry.file_name().to_str() {
                Some(s) => s,
                None => continue,
            };
            let path = entry.path();
            let meta = match std::fs::symlink_metadata(path) {
                Ok(m) => EntryMeta::from_metadata(&m),
                Err(_) => continue,
            };
            let depth = entry.depth();
            while let Some((d, _)) = stack.last() {
                if *d < depth {
                    break;
                }
                stack.pop();
            }
            let parent = match stack.last() {
                Some((_, id)) => *id,
                None => continue,
            };
            let id = self.upsert_child(parent, name, meta);
            if meta.kind == EntryKind::Dir {
                stack.push((depth, id));
            }
        }
    }

    /// Merge a scanned fragment under a volume, returning the new root node.
    pub fn merge_fragment(&mut self, frag: Fragment, vol: VolumeIdx, root_path: &str) -> NodeId {
        let Fragment { index: f, root, .. } = frag;
        let mut remap = vec![NO_PARENT; f.nodes.len()];
        let root_name = self.names.intern(root_path);
        let empty_ext = self.names.intern("");
        let root_id = self.push_node(
            EntryMeta {
                kind: EntryKind::Dir,
                size: 0,
                mtime: 0,
                mtime_ns: 0,
                inode: 0,
            },
            NO_PARENT,
            root_name,
            empty_ext,
            vol,
        );
        remap[root as usize] = root_id;
        // Count the root itself (it is a directory).
        self.add_totals(vol, EntryKind::Dir, 0);
        self.roots.push(RootRec {
            id: root_id,
            volume: vol,
            path: root_path.to_string(),
        });
        {
            let v = &mut self.volumes[vol.0 as usize];
            v.roots.push(root_id);
            v.root_paths.push(root_path.to_string());
        }

        for i in 0..f.nodes.len() {
            if i as NodeId == root {
                continue;
            }
            let n = f.nodes[i];
            let name = self.names.intern(f.names.original(n.name));
            let ext = self.names.intern(f.names.original(n.ext));
            let parent = remap[n.parent as usize];
            let id = self.push_node(
                EntryMeta {
                    kind: n.kind,
                    size: n.size,
                    mtime: n.mtime,
                    mtime_ns: n.mtime_ns,
                    inode: n.inode,
                },
                parent,
                name,
                ext,
                vol,
            );
            remap[i] = id;
            self.link_child(parent, name, id);
            self.add_totals(vol, n.kind, n.size);
        }
        self.generation += 1;
        root_id
    }

    /// Rebuild the arena, dropping tombstones and renumbering nodes.
    pub fn compact(&mut self) {
        if self.dead_count() == 0 {
            return;
        }
        let names = std::mem::take(&mut self.names);
        let mut out = Index {
            names,
            nodes: Vec::with_capacity(self.live_count()),
            dead: Vec::with_capacity(self.live_count()),
            children: FxHashMap::default(),
            roots: Vec::new(),
            volumes: self.volumes.clone(),
            volume_by_id: self.volume_by_id.clone(),
            totals: Totals::default(),
            exclusions: self.exclusions.clone(),
            generation: self.generation + 1,
            new_dirs: Vec::new(),
        };
        for v in out.volumes.iter_mut() {
            v.totals = Totals::default();
            v.roots.clear();
        }
        let mut remap = vec![NO_PARENT; self.nodes.len()];
        for (i, slot) in remap.iter_mut().enumerate() {
            if self.dead[i] {
                continue;
            }
            let n = self.nodes[i];
            let new_id = out.nodes.len() as NodeId;
            *slot = new_id;
            out.nodes.push(Node {
                parent: NO_PARENT, // fixed below
                ..n
            });
            out.dead.push(false);
        }
        // Fix parents and rebuild children + totals.
        for (i, is_dead) in self.dead.iter().enumerate() {
            if *is_dead {
                continue;
            }
            let new_id = remap[i];
            let old = self.nodes[i];
            let new_parent = if old.parent == NO_PARENT {
                NO_PARENT
            } else {
                remap[old.parent as usize]
            };
            out.nodes[new_id as usize].parent = new_parent;
            if new_parent != NO_PARENT {
                out.link_child(new_parent, old.name, new_id);
            }
            out.add_totals(old.volume, old.kind, old.size);
        }
        // Remap roots.
        for r in &self.roots {
            let new_id = remap[r.id as usize];
            out.roots.push(RootRec {
                id: new_id,
                volume: r.volume,
                path: r.path.clone(),
            });
            let v = &mut out.volumes[r.volume.0 as usize];
            v.roots.push(new_id);
            if !v.root_paths.contains(&r.path) {
                v.root_paths.push(r.path.clone());
            }
        }
        *self = out;
    }
}

/// Current unix time in seconds.
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
