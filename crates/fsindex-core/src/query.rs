//! Query evaluation: filtering, sorting and pagination over the in-memory
//! index. Names/metadata only — never file contents.

pub use crate::catalog::{ResultRows, SearchCatalog, SnapshotSearch};
use crate::index::{Index, VolumeIdx};
use crate::model::{EntryKind, NodeId, SearchHit, SearchQuery, SortKey};
use regex::{Regex, RegexBuilder};
use rustc_hash::FxHashSet;
use std::sync::Arc;

/// A compiled search query. Regexes are built once per request.
pub struct CompiledQuery {
    pub q: SearchQuery,
    name_sub: Option<String>,
    path_sub: Option<String>,
    name_re: Option<Regex>,
    path_re: Option<Regex>,
    volume_allow: Option<Vec<bool>>,
    volume_name_terms: Vec<String>,
    ext_set: FxHashSet<String>,
}

impl CompiledQuery {
    pub fn new(q: SearchQuery) -> anyhow::Result<Self> {
        let cs = q.case_sensitive;
        let name_re = if q.regex {
            match &q.name {
                Some(p) => Some(
                    RegexBuilder::new(p)
                        .case_insensitive(!cs)
                        .build()
                        .map_err(|e| anyhow::anyhow!("invalid name regex: {e}"))?,
                ),
                None => None,
            }
        } else {
            None
        };
        let path_re = if q.regex {
            match &q.path {
                Some(p) => Some(
                    RegexBuilder::new(p)
                        .case_insensitive(!cs)
                        .build()
                        .map_err(|e| anyhow::anyhow!("invalid path regex: {e}"))?,
                ),
                None => None,
            }
        } else {
            None
        };
        let name_sub = if q.regex {
            None
        } else {
            q.name
                .as_ref()
                .map(|s| if cs { s.clone() } else { s.to_lowercase() })
        };
        let path_sub = if q.regex {
            None
        } else {
            q.path
                .as_ref()
                .map(|s| if cs { s.clone() } else { s.to_lowercase() })
        };
        let ext_set = q
            .extensions
            .iter()
            .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
            .filter(|e| !e.is_empty())
            .collect();
        let volume_name_terms = q.volume_names.iter().map(|s| s.to_lowercase()).collect();
        Ok(CompiledQuery {
            q,
            name_sub,
            path_sub,
            name_re,
            path_re,
            volume_allow: None,
            volume_name_terms,
            ext_set,
        })
    }

    pub fn needs_path(&self) -> bool {
        self.path_sub.is_some() || self.path_re.is_some() || self.q.search_path
    }

    fn prepared_volumes(&mut self, idx: &Index) -> &[bool] {
        if self.volume_allow.is_none() {
            let mut allow = vec![false; idx.volumes.len()];
            let any_id_filter = !self.q.volumes.is_empty();
            if any_id_filter {
                for v in &self.q.volumes {
                    if let Some(&vi) = idx.volume_by_id.get(v) {
                        allow[vi.0 as usize] = true;
                    }
                }
            } else {
                allow.iter_mut().for_each(|b| *b = true);
            }
            self.volume_allow = Some(allow);
        }
        self.volume_allow.as_ref().unwrap()
    }

    fn volume_name_matches(&self, idx: &Index, vol: VolumeIdx) -> bool {
        if self.volume_name_terms.is_empty() {
            return true;
        }
        let v = idx.volume_rec(vol);
        let name = v.name.to_lowercase();
        let mount = v.mount.to_lowercase();
        self.volume_name_terms
            .iter()
            .any(|t| name.contains(t.as_str()) || mount.contains(t.as_str()))
    }

    /// Full predicate. `path` must be `Some` when [`needs_path`] is true.
    pub fn matches(&mut self, idx: &Index, id: NodeId, path: Option<&str>) -> bool {
        let node = &idx.nodes[id as usize];

        if let Some(kind) = self.q.kind {
            if node.kind != kind {
                return false;
            }
        }
        // Volume id filter.
        let allow = self.prepared_volumes(idx);
        if !allow.get(node.volume.0 as usize).copied().unwrap_or(false) {
            return false;
        }
        if !self.volume_name_matches(idx, node.volume) {
            return false;
        }
        if !self.ext_set.is_empty() {
            let ext = idx.names.original(node.ext);
            if !self.ext_set.contains(ext) {
                return false;
            }
        }
        if let Some(min) = self.q.min_size {
            if node.size < min {
                return false;
            }
        }
        if let Some(max) = self.q.max_size {
            if node.size > max {
                return false;
            }
        }
        if let Some(after) = self.q.modified_after {
            if node.mtime < after {
                return false;
            }
        }
        if let Some(before) = self.q.modified_before {
            if node.mtime > before {
                return false;
            }
        }
        // Name. When searching the whole path, `name` matches the full path
        // instead of just the file name.
        if let Some(re) = &self.name_re {
            let hay = if self.q.search_path {
                path.unwrap_or_default()
            } else {
                idx.names.original(node.name)
            };
            if !re.is_match(hay) {
                return false;
            }
        } else if let Some(sub) = &self.name_sub {
            if self.q.search_path {
                match path {
                    Some(p) => {
                        let hit = if self.q.case_sensitive {
                            p.contains(sub.as_str())
                        } else {
                            p.to_lowercase().contains(sub.as_str())
                        };
                        if !hit {
                            return false;
                        }
                    }
                    None => return false,
                }
            } else if self.q.case_sensitive {
                if !idx.names.original(node.name).contains(sub.as_str()) {
                    return false;
                }
            } else if !idx.names.lower(node.name).contains(sub.as_str()) {
                return false;
            }
        }
        // Path.
        if let Some(re) = &self.path_re {
            match path {
                Some(p) if re.is_match(p) => {}
                _ => return false,
            }
        } else if let Some(sub) = &self.path_sub {
            match path {
                Some(p) => {
                    let hit = if self.q.case_sensitive {
                        p.contains(sub.as_str())
                    } else {
                        p.to_lowercase().contains(sub.as_str())
                    };
                    if !hit {
                        return false;
                    }
                }
                None => return false,
            }
        }
        true
    }
}

/// A query's match set plus how much of it is already sorted.
///
/// Scrolling the GUI issues one `search` per page; without a snapshot each page
/// re-filters and re-sorts every match (`O(n log n)`), which stutters on a
/// multi-million-entry index. This is held behind its own `Mutex` so the
/// daemon's background thread can rebuild it (filter **and** sort) while
/// requests keep serving the previous snapshot — the churn that bumps
/// `Index::generation` never lands on the request path.
struct Snapshot {
    /// Index `generation` the match set was built at.
    gen_at_build: u64,
    /// The query (filter + sort fields only).
    key: SearchQuery,
    /// Match set, or `None` for an uncacheable query (path matching) — cached
    /// so the background refresher doesn't retry it every tick.
    ids: Option<Vec<NodeId>>,
    /// Number of leading entries of `ids` in final sorted order.
    sorted_len: usize,
}

/// The cache key for a query: the filter and sort fields, but **not** the
/// paging fields (`offset`/`limit`), so every page of the same query reuses one
/// snapshot.
fn filter_key(q: &SearchQuery) -> SearchQuery {
    SearchQuery {
        offset: 0,
        limit: 0,
        ..q.clone()
    }
}

/// Public form of [`filter_key`], for callers that cache a query's key.
pub fn filter_key_for(q: &SearchQuery) -> SearchQuery {
    filter_key(q)
}

/// A query the cache cannot serve: path-sorted (the cached id-comparator is
/// wrong, and a full sort is comparatively cheap) or path-matching (each
/// candidate materialises a path, so `build_match_set` bails). Mirrors
/// `CompiledQuery::needs_path`.
fn uncacheable(q: &SearchQuery) -> bool {
    q.sort == SortKey::Path || q.path.is_some() || q.search_path
}

/// What the engine should do for a request.
pub enum CachePlan {
    /// A snapshot for this query is present; serve it (possibly slightly stale).
    Ready,
    /// No snapshot for this query yet; build one.
    Build,
    /// Not servable from the cache (path matching); use [`search`].
    Uncacheable,
}

#[derive(Default)]
pub struct QueryCache {
    slot: Option<Snapshot>,
}

impl QueryCache {
    /// Decide what a request needs. A same-key snapshot is served even if the
    /// index has advanced since it was built (the background refresher catches
    /// up), so only the very first request for a query builds synchronously.
    pub fn plan(&self, q: &SearchQuery) -> CachePlan {
        if uncacheable(q) {
            return CachePlan::Uncacheable;
        }
        let key = filter_key(q);
        match &self.slot {
            Some(s) if s.key == key && s.ids.is_some() => CachePlan::Ready,
            _ => CachePlan::Build,
        }
    }

    /// Install a freshly built snapshot. Ignores a build that is older than the
    /// current one for the same key (a slow background refresh can never clobber
    /// a newer snapshot).
    pub fn store_built(
        &mut self,
        key: SearchQuery,
        generation: u64,
        ids: Option<Vec<NodeId>>,
        sorted_len: usize,
    ) {
        if let Some(s) = &self.slot {
            if s.key == key && s.gen_at_build > generation {
                return;
            }
        }
        self.slot = Some(Snapshot {
            gen_at_build: generation,
            key,
            ids,
            sorted_len,
        });
    }

    /// Force a rebuild (exclusion / root change). Drops the snapshot so the next
    /// request rebuilds at the current index; the background refresher then keeps
    /// it current.
    pub fn invalidate(&mut self) {
        self.slot = None;
    }

    /// The key + target-sorted-length the background refresher should rebuild,
    /// or `None` when the snapshot is already current (or uncacheable).
    pub fn refresh_target(&self, generation: u64) -> Option<(SearchQuery, usize)> {
        let s = self.slot.as_ref()?;
        s.ids.as_ref()?; // uncacheable query: nothing to build
        if s.gen_at_build == generation {
            return None; // already current
        }
        Some((s.key.clone(), s.sorted_len))
    }

    /// If a snapshot for this query is present, return the requested page,
    /// extending the sorted prefix as needed.
    pub fn ready_page(
        &mut self,
        idx: &Index,
        q: &SearchQuery,
        default_limit: u64,
    ) -> Option<(Vec<SearchHit>, u64)> {
        let s = self.slot.as_mut()?;
        let ids = s.ids.as_mut()?;
        if s.key != filter_key(q) {
            return None;
        }
        let limit = if q.limit == 0 { default_limit } else { q.limit };
        let start = q.offset as usize;
        let end = start.saturating_add(limit as usize);
        if end > s.sorted_len {
            let target = end.min(ids.len());
            s.sorted_len = sort_ids_prefix(idx, ids, q.sort, q.desc, s.sorted_len, target);
        }
        let start = start.min(ids.len());
        let end = end.min(ids.len());
        let hits = ids[start..end]
            .iter()
            .map(|&id| build_hit(idx, id))
            .collect();
        Some((hits, ids.len() as u64))
    }
}

/// Build the match set for `key` and sort its first `sort_target` entries, off
/// the request path (the daemon's background refresher). Returns the id list
/// and the length actually sorted.
pub fn build_sorted(
    idx: &Index,
    key: &SearchQuery,
    sort_target: usize,
) -> (Option<Vec<NodeId>>, usize) {
    let Some(mut ids) = build_match_set(idx, key) else {
        return (None, 0);
    };
    let target = sort_target.min(ids.len());
    if target > 0 {
        let len = sort_ids_prefix(idx, &mut ids, key.sort, key.desc, 0, target);
        return (Some(ids), len);
    }
    (Some(ids), 0)
}

/// Evaluate a query without a cache: returns the requested page and the total
/// match count.
pub fn search(
    idx: &Index,
    q: &SearchQuery,
    default_limit: u64,
) -> anyhow::Result<(Vec<SearchHit>, u64)> {
    let mut cq = CompiledQuery::new(q.clone())?;
    let need_path = cq.needs_path();
    let include_offline = q.include_offline;

    let mut matched: Vec<NodeId> = Vec::new();
    for (i, node) in idx.nodes.iter().enumerate() {
        if idx.dead[i] {
            continue;
        }
        if !include_offline && idx.volume_state(node.volume) == crate::model::VolumeState::Offline {
            continue;
        }
        let id = i as NodeId;
        if need_path {
            let p = idx.path_of(id);
            if !cq.matches(idx, id, Some(&p)) {
                continue;
            }
        } else if !cq.matches(idx, id, None) {
            continue;
        }
        matched.push(id);
    }

    let total = matched.len() as u64;
    sort_ids(idx, &mut matched, q.sort, q.desc);

    let limit = if q.limit == 0 { default_limit } else { q.limit };
    let offset = q.offset;
    let start = (offset as usize).min(matched.len());
    let end = (start + limit as usize).min(matched.len());
    let page = &matched[start..end];

    let hits = page
        .iter()
        .map(|&id| build_hit(idx, id))
        .collect::<Vec<_>>();

    Ok((hits, total))
}

/// Collect the ids matching the **filter** part of a query (no sort/paging).
/// Used to (re)build a snapshot, both inline and on the daemon's background
/// refresh thread.
pub fn build_match_set(idx: &Index, q: &SearchQuery) -> Option<Vec<NodeId>> {
    let mut cq = CompiledQuery::new(q.clone()).ok()?;
    if cq.needs_path() {
        return None;
    }
    let include_offline = q.include_offline;
    let mut ids: Vec<NodeId> = Vec::new();
    for (i, node) in idx.nodes.iter().enumerate() {
        if idx.dead[i] {
            continue;
        }
        if !include_offline && idx.volume_state(node.volume) == crate::model::VolumeState::Offline {
            continue;
        }
        let id = i as NodeId;
        if cq.matches(idx, id, None) {
            ids.push(id);
        }
    }
    Some(ids)
}

/// Count matches without sorting or materialising hits.
pub fn count(idx: &Index, q: &SearchQuery) -> anyhow::Result<u64> {
    let mut cq = CompiledQuery::new(q.clone())?;
    let need_path = cq.needs_path();
    let include_offline = q.include_offline;
    let mut n = 0u64;
    for (i, node) in idx.nodes.iter().enumerate() {
        if idx.dead[i] {
            continue;
        }
        if !include_offline && idx.volume_state(node.volume) == crate::model::VolumeState::Offline {
            continue;
        }
        let id = i as NodeId;
        let ok = if need_path {
            let p = idx.path_of(id);
            cq.matches(idx, id, Some(&p))
        } else {
            cq.matches(idx, id, None)
        };
        if ok {
            n += 1;
        }
    }
    Ok(n)
}

fn sort_ids(idx: &Index, ids: &mut [NodeId], key: SortKey, desc: bool) {
    match key {
        SortKey::Path => {
            // Sorting by full path requires materialising paths.
            let mut keyed: Vec<(String, NodeId)> =
                ids.iter().map(|&id| (idx.path_of(id), id)).collect();
            keyed.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            if desc {
                keyed.reverse();
            }
            for (slot, (_, id)) in ids.iter_mut().zip(keyed) {
                *slot = id;
            }
        }
        _ => {
            ids.sort_by(|&a, &b| cmp_key(idx, a, b, key));
            if desc {
                ids.reverse();
            }
        }
    }
}

/// The sort comparator for one key. Ties break by node id for a stable, total
/// order (so the whole slice sorts identically to a full sort).
pub(crate) fn cmp_key(idx: &Index, a: NodeId, b: NodeId, key: SortKey) -> std::cmp::Ordering {
    let na = &idx.nodes[a as usize];
    let nb = &idx.nodes[b as usize];
    let ord = match key {
        SortKey::Name => idx.names.lower(na.name).cmp(idx.names.lower(nb.name)),
        SortKey::Size => na.size.cmp(&nb.size),
        SortKey::Mtime => (na.mtime, na.mtime_ns).cmp(&(nb.mtime, nb.mtime_ns)),
        SortKey::Kind => na.kind.rank().cmp(&nb.kind.rank()),
        SortKey::Extension => idx.names.lower(na.ext).cmp(idx.names.lower(nb.ext)),
        SortKey::Path => std::cmp::Ordering::Equal, // handled separately
    };
    ord.then_with(|| a.cmp(&b))
}

/// Sort `ids[..target]` into final order, expanding geometrically and sorting
/// from scratch only when the target has more than doubled. Each pass uses
/// `select_nth_unstable_by` (O(n)) to place the target smallest, then sorts
/// just that prefix, so no pass ever re-sorts the whole array repeatedly.
///
/// Returns the number of leading elements now in final sorted order (>= `target`).
fn sort_ids_prefix(
    idx: &Index,
    ids: &mut [NodeId],
    key: SortKey,
    desc: bool,
    from: usize,
    target: usize,
) -> usize {
    let len = ids.len();
    if target <= from || target == 0 {
        return from.min(len);
    }
    let cmp = |a: &NodeId, b: &NodeId| {
        let ord = cmp_key(idx, *a, *b, key);
        if desc {
            ord.reverse()
        } else {
            ord
        }
    };

    // Grow geometrically so a long forward scroll does O(log n) passes, and a
    // pass that reaches the end sorts once and is then complete.
    let grown = target.max(from.saturating_mul(2));
    let want = grown.min(len);
    if want >= len {
        // Full sort; every later page is a plain slice.
        ids.sort_by(cmp);
        return len;
    }
    // Partition so the `want` smallest (by `cmp`) land in `ids[..want]`, then
    // order just that prefix.
    let (prefix, _, _) = ids.select_nth_unstable_by(want, cmp);
    prefix.sort_by(cmp);
    want
}

pub(crate) fn build_hit(idx: &Index, id: NodeId) -> SearchHit {
    let node = &idx.nodes[id as usize];
    let vol = idx.volume_rec(node.volume);
    let path = idx.path_of(id);
    let name = idx.names.original(node.name).to_string();
    // Depth below the root: count separators in the relative portion.
    let depth = {
        let mut d = 0u32;
        let mut cur = id;
        loop {
            let n = &idx.nodes[cur as usize];
            if n.parent == crate::model::NO_PARENT {
                break;
            }
            d += 1;
            cur = n.parent;
        }
        d
    };
    SearchHit {
        path,
        name,
        volume: vol.id,
        volume_name: vol.name.clone(),
        kind: node.kind,
        size: node.size,
        mtime: node.mtime,
        mtime_ns: node.mtime_ns,
        inode: node.inode,
        depth,
    }
}

/// Kind helper exposed for tests.
pub fn kind_is_dir(k: EntryKind) -> bool {
    k == EntryKind::Dir
}

/// An owned, immutable result model for a native view. It contains no index
/// node IDs: watcher removals and compaction cannot retarget a displayed row.
/// Paging fields are intentionally ignored; scrolling indexes this slice.
#[derive(Clone, Debug)]
pub struct ResultSnapshot {
    pub generation: u64,
    pub rows: ResultRows,
}

/// Build on a worker, never on the UI thread. Cancellation is checked during
/// filtering/materialization and around sorting (the sort itself is atomic).
pub fn snapshot(
    idx: &Index,
    q: &SearchQuery,
    cancelled: impl Fn() -> bool,
) -> anyhow::Result<Option<ResultSnapshot>> {
    let mut cq = CompiledQuery::new(q.clone())?;
    let need_path = cq.needs_path();
    let mut ids = Vec::new();
    for (i, node) in idx.nodes.iter().enumerate() {
        if i % 1024 == 0 && cancelled() {
            return Ok(None);
        }
        if idx.dead[i]
            || (!q.include_offline
                && idx.volume_state(node.volume) == crate::model::VolumeState::Offline)
        {
            continue;
        }
        let id = i as NodeId;
        let path = need_path.then(|| idx.path_of(id));
        if cq.matches(idx, id, path.as_deref()) {
            ids.push(id);
        }
    }
    if cancelled() {
        return Ok(None);
    }
    sort_ids(idx, &mut ids, q.sort, q.desc);
    if cancelled() {
        return Ok(None);
    }
    let mut rows = Vec::with_capacity(ids.len());
    for (i, id) in ids.into_iter().enumerate() {
        if i % 1024 == 0 && cancelled() {
            return Ok(None);
        }
        rows.push(Arc::new(build_hit(idx, id)));
    }
    Ok(Some(ResultSnapshot {
        generation: idx.generation,
        rows: rows.into(),
    }))
}

pub(crate) fn sort_snapshot_ids(
    idx: &Index,
    ids: &mut Vec<NodeId>,
    q: &SearchQuery,
    cancelled: &impl Fn() -> bool,
) -> bool {
    if q.sort == SortKey::Path {
        // Materialize each path once; sort small integer keys, not cloned strings.
        let mut paths = Vec::with_capacity(ids.len());
        for (i, &id) in ids.iter().enumerate() {
            if i % 1024 == 0 && cancelled() {
                return false;
            }
            paths.push((idx.path_of(id), id));
        }
        let mut order: Vec<usize> = (0..paths.len()).collect();
        let compare = |a: &usize, b: &usize| {
            let ord = paths[*a].cmp(&paths[*b]);
            if q.desc {
                ord.reverse()
            } else {
                ord
            }
        };
        if !cancellable_sort(&mut order, compare, cancelled) {
            return false;
        }
        for (slot, key) in ids.iter_mut().zip(order) {
            *slot = paths[key].1;
        }
        !cancelled()
    } else {
        cancellable_sort(
            ids,
            |a, b| {
                let ord = cmp_key(idx, *a, *b, q.sort);
                if q.desc {
                    ord.reverse()
                } else {
                    ord
                }
            },
            cancelled,
        )
    }
}

/// Bounded sort runs and cancellable merges let a new keystroke interrupt a
/// cold/broad search instead of waiting for a monolithic million-row sort.
pub(crate) fn cancellable_sort<T: Copy>(
    values: &mut Vec<T>,
    compare: impl Fn(&T, &T) -> std::cmp::Ordering,
    cancelled: &impl Fn() -> bool,
) -> bool {
    const RUN: usize = 2048;
    for chunk in values.chunks_mut(RUN) {
        if cancelled() {
            return false;
        }
        chunk.sort_unstable_by(&compare);
    }
    if cancelled() {
        return false;
    }
    if values.len() <= RUN {
        return true;
    }
    let mut scratch = values.clone();
    let mut width = RUN;
    while width < values.len() {
        for start in (0..values.len()).step_by(width * 2) {
            let mid = (start + width).min(values.len());
            let end = (start + width * 2).min(values.len());
            let (mut a, mut b) = (start, mid);
            for (offset, slot) in scratch[start..end].iter_mut().enumerate() {
                if offset % 1024 == 0 && cancelled() {
                    return false;
                }
                if a < mid && (b == end || compare(&values[a], &values[b]).is_le()) {
                    *slot = values[a];
                    a += 1;
                } else {
                    *slot = values[b];
                    b += 1;
                }
            }
        }
        std::mem::swap(values, &mut scratch);
        width *= 2;
    }
    !cancelled()
}

#[cfg(test)]
mod interactive_sort_tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn cancellation_interrupts_merge_and_sort_matches_total_order() {
        let original: Vec<_> = (0..20000).map(|i| (i * 3571) % 20000).collect();
        let mut values = original.clone();
        let checks = Cell::new(0);
        let completed = cancellable_sort(&mut values, Ord::cmp, &|| {
            checks.set(checks.get() + 1);
            checks.get() > 18 // Past initial sorted runs, inside the first merge.
        });
        assert!(!completed);
        let mut values = original;
        assert!(cancellable_sort(&mut values, Ord::cmp, &|| false));
        assert!(values.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(values.len(), 20000);
    }
}
