//! Initial (and reconciliation) scanning.
//!
//! [`scan_root`] produces a standalone fragment (fast to scan, merged in one
//! shot). [`visit_root`] is an incremental scanner used for large roots: it
//! walks the tree and hands the caller batches of entries in pre-order, so the
//! index can be built up while the scan is still running and clients get
//! partial results.

use crate::index::{EntryMeta, Fragment, Index, NameId};
use crate::model::NO_PARENT;
use crate::paths::{normalize_ext, Exclusions};
use std::path::{Path, PathBuf};

/// Recursively scan `root` into a standalone fragment.
pub fn scan_root(root: &Path, exclusions: &Exclusions) -> Fragment {
    let mut idx = Index::new(exclusions.clone());
    let root_path = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let root_name = root_path.to_string_lossy().to_string();
    let root_name_id = idx.names.intern(&root_name);
    let empty = idx.names.intern("");
    let root_id = idx.nodes.len() as u32;
    idx.nodes.push(crate::index::Node {
        parent: NO_PARENT,
        name: root_name_id,
        ext: empty,
        volume: crate::index::VolumeIdx(0),
        kind: crate::model::EntryKind::Dir,
        size: 0,
        mtime: 0,
        mtime_ns: 0,
        inode: 0,
    });
    idx.dead.push(false);

    let excl = exclusions.clone();
    let walker = walkdir::WalkDir::new(&root_path)
        .follow_links(false)
        .into_iter()
        .filter_entry(move |e| {
            if e.depth() == 0 {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            // Pruning here means an excluded path's whole subtree is never
            // walked.
            !excl.excludes(e.path(), &name, e.file_type().is_dir())
        });

    let mut stack: Vec<(usize, u32)> = vec![(0, root_id)];
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

        let name_id: NameId = idx.names.intern(name);
        let ext = idx.names.intern(normalize_ext(name).as_ref());
        let id = idx.nodes.len() as u32;
        idx.nodes.push(crate::index::Node {
            parent,
            name: name_id,
            ext,
            volume: crate::index::VolumeIdx(0),
            kind: meta.kind,
            size: meta.size,
            mtime: meta.mtime,
            mtime_ns: meta.mtime_ns,
            inode: meta.inode,
        });
        idx.dead.push(false);
        idx.children.entry(parent).or_default().push((name_id, id));
        if meta.kind == crate::model::EntryKind::Dir {
            stack.push((depth, id));
        }
    }

    Fragment {
        index: idx,
        root: root_id,
        root_path: PathBuf::from(root_name),
    }
}

/// A scanned entry. `parent` is the **global** scan id of its parent
/// (`None` when the parent is the scanned root). Global ids are assigned by
/// [`visit_root`] as `first_gid + index_within_batch` and stay valid across
/// batch flushes.
#[derive(Clone, Debug)]
pub struct ScannedEntry {
    pub name: String,
    pub meta: EntryMeta,
    pub parent: Option<usize>,
}

const BATCH: usize = 32_768;

/// Incrementally scan `root`, calling `sink(first_gid, batch)` with batches of
/// entries in pre-order. Entries in a batch have consecutive global ids
/// starting at `first_gid`; a parent always has a smaller global id than its
/// child. The root itself is not emitted. `sink` returns `false` to stop early.
pub fn visit_root<F>(root: &Path, exclusions: &Exclusions, mut sink: F)
where
    F: FnMut(usize, &[ScannedEntry]) -> bool,
{
    let root_path = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let excl = exclusions.clone();
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

    // depth_parent[d] = global id of the current directory at depth d.
    let mut depth_parent: Vec<Option<usize>> = vec![None];
    let mut batch: Vec<ScannedEntry> = Vec::with_capacity(BATCH);
    let mut batch_first_gid = 0usize;
    let mut emitted = 0usize;

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
        let parent = if depth <= 1 {
            None
        } else {
            depth_parent.get(depth - 1).copied().flatten()
        };

        let gid = emitted;
        batch.push(ScannedEntry {
            name: name.to_string(),
            meta,
            parent,
        });
        emitted += 1;

        if depth_parent.len() <= depth {
            depth_parent.resize(depth + 1, None);
        }
        depth_parent[depth] = if meta.kind == crate::model::EntryKind::Dir {
            Some(gid)
        } else {
            None
        };

        if batch.len() >= BATCH {
            if !sink(batch_first_gid, &batch) {
                return;
            }
            batch.clear();
            // Global ids remain valid across the flush, so depth_parent is
            // intentionally preserved.
            batch_first_gid = emitted;
        }
    }
    if !batch.is_empty() {
        sink(batch_first_gid, &batch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn incremental_batches_are_preorder() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/c.txt"), b"x").unwrap();
        fs::write(root.join("top.txt"), b"y").unwrap();

        let excl = Exclusions::default();
        let mut count = 0usize;
        visit_root(root, &excl, |_first, batch| {
            for (i, e) in batch.iter().enumerate() {
                let gid = _first + i;
                if let Some(p) = e.parent {
                    assert!(p < gid, "parent precedes child");
                }
            }
            count += batch.len();
            true
        });
        assert_eq!(count, 4, "a, a/b, a/b/c.txt, top.txt");
    }

    #[test]
    fn fragment_counts_match_incremental() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("x/y/z")).unwrap();
        for i in 0..200 {
            fs::write(root.join(format!("x/f{i}.txt")), b"z").unwrap();
        }
        let excl = Exclusions::default();
        let frag = scan_root(root, &excl);
        let mut n = 0;
        visit_root(root, &excl, |_f, b| {
            n += b.len();
            true
        });
        // scan_root counts the root node; visit_root does not emit it.
        assert_eq!(frag.index.nodes.len() - 1, n);
    }
}
