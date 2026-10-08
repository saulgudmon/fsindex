//! Immutable, pre-sorted search data, independent of the live index writer.
//! Result positions belong to this owned catalogue, never to the mutable index.
use crate::{
    index::Index,
    model::{EntryKind, NodeId, SearchHit, SearchQuery, SortKey, VolumeState},
    query::{self, CompiledQuery, ResultSnapshot},
};
use std::{
    collections::VecDeque,
    ops::Index as IndexOp,
    sync::{Arc, OnceLock},
};

#[derive(Default)]
struct PackedNames {
    text: String,
    ends: Vec<usize>,
}
impl PackedNames {
    fn push(&mut self, name: &str) {
        self.text.push_str(name);
        self.ends.push(self.text.len());
    }
    fn get(&self, position: u32) -> &str {
        let n = position as usize;
        let start = if n == 0 { 0 } else { self.ends[n - 1] };
        &self.text[start..self.ends[n]]
    }
}

/// Owns frozen data. Mutation, tombstones and compaction of the source index
/// cannot change either these positions or their row identities.
pub struct SearchCatalog {
    index: Index,
    ids: Vec<NodeId>,
    lower: PackedNames,
    original: PackedNames,
    hits: Vec<OnceLock<Arc<SearchHit>>>,
    all: Arc<[u32]>,
    online: [Arc<[u32]>; 5], // all, files, dirs, symlinks
}

/// Capture under the live read lock; prepare sorting/text outside that lock.
pub struct CatalogInput(Index);
impl SearchCatalog {
    pub fn capture(index: &Index) -> CatalogInput {
        CatalogInput(index.frozen_copy())
    }
    pub fn prepare(input: CatalogInput, cancelled: impl Fn() -> bool) -> Option<Arc<Self>> {
        let index = input.0;
        let mut ids: Vec<_> = (0..index.nodes.len())
            .filter(|&i| !index.dead[i])
            .map(|i| i as NodeId)
            .collect();
        if !query::sort_snapshot_ids(&index, &mut ids, &SearchQuery::default(), &cancelled) {
            return None;
        }
        let mut lower = PackedNames::default();
        let mut original = PackedNames::default();
        let mut online: [Vec<u32>; 5] = std::array::from_fn(|_| Vec::new());
        for (position, &id) in ids.iter().enumerate() {
            if position % 1024 == 0 && cancelled() {
                return None;
            }
            lower.push(index.name_lower(id));
            original.push(index.name_of(id));
            if index.volume_state(index.volume_of(id)) != VolumeState::Offline {
                online[0].push(position as u32);
                let kind = match index.kind_of(id) {
                    EntryKind::File => 1,
                    EntryKind::Dir => 2,
                    EntryKind::Symlink => 3,
                    EntryKind::Other => 4,
                };
                online[kind].push(position as u32);
            }
        }
        let count = ids.len();
        Some(Arc::new(Self {
            index,
            ids,
            lower,
            original,
            hits: (0..count).map(|_| OnceLock::new()).collect(),
            all: (0..count as u32).collect(),
            online: online.map(Arc::from),
        }))
    }
    pub fn generation(&self) -> u64 {
        self.index.generation
    }
    pub fn len(&self) -> usize {
        self.ids.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
    fn hit(&self, position: u32) -> &Arc<SearchHit> {
        self.hits[position as usize]
            .get_or_init(|| Arc::new(query::build_hit(&self.index, self.ids[position as usize])))
    }
    fn base_indices(&self, q: &SearchQuery) -> Option<Arc<[u32]>> {
        if q.include_offline {
            return q.kind.is_none().then(|| Arc::clone(&self.all));
        }
        let kind = match q.kind {
            None => 0,
            Some(EntryKind::File) => 1,
            Some(EntryKind::Dir) => 2,
            Some(EntryKind::Symlink) => 3,
            Some(EntryKind::Other) => 4,
        };
        Some(Arc::clone(&self.online[kind]))
    }
    /// Empty search is a reference to the prepared order, including Files/Folders.
    pub fn baseline(self: &Arc<Self>, q: &SearchQuery) -> Option<ResultSnapshot> {
        if q.sort != SortKey::Name
            || !q.name.as_deref().unwrap_or_default().is_empty()
            || !simple_filters(q)
        {
            return None;
        }
        let indices = self.base_indices(q)?;
        Some(self.result(indices, q.desc))
    }
    fn result(self: &Arc<Self>, indices: Arc<[u32]>, reverse: bool) -> ResultSnapshot {
        ResultSnapshot {
            generation: self.generation(),
            rows: ResultRows(Arc::new(RowView::Catalog {
                catalog: Arc::clone(self),
                indices,
                reverse,
            })),
        }
    }
    pub fn materialized_rows(&self) -> usize {
        self.hits.iter().filter(|row| row.get().is_some()).count()
    }
}

enum RowView {
    Direct(Arc<[Arc<SearchHit>]>),
    Catalog {
        catalog: Arc<SearchCatalog>,
        indices: Arc<[u32]>,
        reverse: bool,
    },
}
/// Shared immutable result order; only the viewport's display rows are expanded.
#[derive(Clone)]
pub struct ResultRows(Arc<RowView>);
impl Default for ResultRows {
    fn default() -> Self {
        Vec::new().into()
    }
}
impl std::fmt::Debug for ResultRows {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResultRows")
            .field("len", &self.len())
            .finish()
    }
}
impl From<Vec<Arc<SearchHit>>> for ResultRows {
    fn from(rows: Vec<Arc<SearchHit>>) -> Self {
        Self(Arc::new(RowView::Direct(rows.into())))
    }
}
impl FromIterator<Arc<SearchHit>> for ResultRows {
    fn from_iter<T: IntoIterator<Item = Arc<SearchHit>>>(rows: T) -> Self {
        rows.into_iter().collect::<Vec<_>>().into()
    }
}
impl ResultRows {
    pub fn len(&self) -> usize {
        match &*self.0 {
            RowView::Direct(rows) => rows.len(),
            RowView::Catalog { indices, .. } => indices.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn get(&self, row: usize) -> Option<&Arc<SearchHit>> {
        match &*self.0 {
            RowView::Direct(rows) => rows.get(row),
            RowView::Catalog {
                catalog,
                indices,
                reverse,
            } => {
                if row >= indices.len() {
                    return None;
                }
                let n = if *reverse {
                    indices.len() - row - 1
                } else {
                    row
                };
                Some(catalog.hit(indices[n]))
            }
        }
    }
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &Arc<SearchHit>> {
        (0..self.len()).map(|i| &self[i])
    }
    pub fn ptr_eq(&self, other: &Self) -> bool {
        if Arc::ptr_eq(&self.0, &other.0) {
            return true;
        }
        match (&*self.0, &*other.0) {
            (
                RowView::Catalog {
                    catalog: a,
                    indices: ai,
                    reverse: ar,
                },
                RowView::Catalog {
                    catalog: b,
                    indices: bi,
                    reverse: br,
                },
            ) => Arc::ptr_eq(a, b) && Arc::ptr_eq(ai, bi) && ar == br,
            _ => false,
        }
    }
    fn indices(&self) -> &[u32] {
        match &*self.0 {
            RowView::Catalog { indices, .. } => indices,
            _ => &[],
        }
    }
}
impl IndexOp<usize> for ResultRows {
    type Output = Arc<SearchHit>;
    fn index(&self, row: usize) -> &Self::Output {
        self.get(row).expect("result row in bounds")
    }
}

struct Cached {
    key: SearchQuery,
    snapshot: ResultSnapshot,
}
/// Per-catalogue result cache. Empty queries never depend on history retention.
#[derive(Default)]
pub struct SnapshotSearch {
    catalog: Option<Arc<SearchCatalog>>,
    recent: VecDeque<Cached>,
}
impl SnapshotSearch {
    /// Convenience path for callers/tests. Desktop publishes catalogues separately.
    pub fn search(
        &mut self,
        index: &Index,
        q: &SearchQuery,
        cancelled: impl Fn() -> bool + Sync,
    ) -> anyhow::Result<Option<ResultSnapshot>> {
        let catalog = if let Some(catalog) = self
            .catalog
            .as_ref()
            .filter(|c| c.generation() == index.generation)
        {
            Arc::clone(catalog)
        } else {
            let Some(catalog) = SearchCatalog::prepare(SearchCatalog::capture(index), &cancelled)
            else {
                return Ok(None);
            };
            catalog
        };
        self.search_catalog(catalog, q, cancelled)
    }
    pub fn search_catalog(
        &mut self,
        catalog: Arc<SearchCatalog>,
        q: &SearchQuery,
        cancelled: impl Fn() -> bool + Sync,
    ) -> anyhow::Result<Option<ResultSnapshot>> {
        if cancelled() {
            return Ok(None);
        }
        // Validate before accepting even an empty regex shortcut.
        let _ = CompiledQuery::new(q.clone())?;
        if self
            .catalog
            .as_ref()
            .is_none_or(|old| !Arc::ptr_eq(old, &catalog))
        {
            self.recent.clear();
            self.catalog = Some(Arc::clone(&catalog));
        }
        if let Some(result) = catalog.baseline(q) {
            return Ok(Some(result));
        }
        let key = query::filter_key_for(q);
        if let Some(cached) = self.recent.iter().find(|entry| entry.key == key) {
            return Ok(Some(cached.snapshot.clone()));
        }
        let candidate = self
            .recent
            .iter()
            .filter(|entry| refines(&key, &entry.key))
            .min_by_key(|entry| entry.snapshot.rows.len());
        let base = catalog
            .base_indices(q)
            .unwrap_or_else(|| Arc::clone(&catalog.all));
        let source = if let Some(candidate) = candidate {
            candidate.snapshot.rows.indices()
        } else {
            &base
        };
        let simple = !q.regex
            && !q.search_path
            && simple_filters(q)
            && (!q.include_offline || q.kind.is_none());
        let needle = if q.case_sensitive {
            q.name.clone().unwrap_or_default()
        } else {
            q.name.as_deref().unwrap_or_default().to_lowercase()
        };
        let names = if q.case_sensitive {
            &catalog.original
        } else {
            &catalog.lower
        };
        let workers = if source.len() < 65536 {
            1
        } else {
            std::thread::available_parallelism()
                .map_or(1, usize::from)
                .min(4)
        };
        let chunk_size = source.len().div_ceil(workers).max(1);
        let filter = |chunk: &[u32]| -> anyhow::Result<Option<Vec<u32>>> {
            let finder = memchr::memmem::Finder::new(needle.as_bytes());
            let mut compiled = CompiledQuery::new(q.clone())?;
            let need_path = compiled.needs_path();
            let mut matches = Vec::with_capacity(chunk.len());
            for (i, &position) in chunk.iter().enumerate() {
                if i % 1024 == 0 && cancelled() {
                    return Ok(None);
                }
                let matched = if simple {
                    finder.find(names.get(position).as_bytes()).is_some()
                } else {
                    let id = catalog.ids[position as usize];
                    if !q.include_offline
                        && catalog.index.volume_state(catalog.index.volume_of(id))
                            == VolumeState::Offline
                    {
                        continue;
                    }
                    let path = need_path.then(|| catalog.index.path_of(id));
                    compiled.matches(&catalog.index, id, path.as_deref())
                };
                if matched {
                    matches.push(position);
                }
            }
            Ok(Some(matches))
        };
        let parts = std::thread::scope(|scope| {
            if workers == 1 {
                return vec![filter(source)];
            }
            let tasks: Vec<_> = source
                .chunks(chunk_size)
                .map(|chunk| scope.spawn(|| filter(chunk)))
                .collect();
            tasks
                .into_iter()
                .map(|task| task.join().expect("search filter thread"))
                .collect()
        });
        let mut parts_ok = Vec::new();
        for part in parts {
            let Some(part) = part? else {
                return Ok(None);
            };
            parts_ok.push(part);
        }
        if cancelled() {
            return Ok(None);
        }
        let mut matches = Vec::with_capacity(parts_ok.iter().map(Vec::len).sum());
        for part in parts_ok {
            matches.extend(part);
        }
        if q.sort != SortKey::Name && candidate.is_none() {
            let sorted = if q.sort == SortKey::Path {
                let paths: Vec<_> = matches
                    .iter()
                    .map(|&position| catalog.index.path_of(catalog.ids[position as usize]))
                    .collect();
                let mut order: Vec<_> = (0..matches.len()).collect();
                let ok = query::cancellable_sort(
                    &mut order,
                    |a, b| {
                        paths[*a].cmp(&paths[*b]).then_with(|| {
                            catalog.ids[matches[*a] as usize]
                                .cmp(&catalog.ids[matches[*b] as usize])
                        })
                    },
                    &cancelled,
                );
                if ok {
                    matches = order.into_iter().map(|n| matches[n]).collect();
                }
                ok
            } else {
                query::cancellable_sort(
                    &mut matches,
                    |a, b| {
                        query::cmp_key(
                            &catalog.index,
                            catalog.ids[*a as usize],
                            catalog.ids[*b as usize],
                            q.sort,
                        )
                    },
                    &cancelled,
                )
            };
            if !sorted {
                return Ok(None);
            }
        }
        if cancelled() {
            return Ok(None);
        }
        let snapshot = catalog.result(matches.into(), q.desc);
        self.recent.push_front(Cached {
            key,
            snapshot: snapshot.clone(),
        });
        let mut bytes = 0;
        let keep = self
            .recent
            .iter()
            .take(32)
            .take_while(|entry| {
                bytes += entry.snapshot.rows.len() * 4;
                bytes <= 64 * 1024 * 1024
            })
            .count()
            .max(1);
        self.recent.truncate(keep);
        Ok(Some(snapshot))
    }
}

fn simple_filters(q: &SearchQuery) -> bool {
    q.path.is_none()
        && q.volumes.is_empty()
        && q.volume_names.is_empty()
        && q.extensions.is_empty()
        && q.min_size.is_none()
        && q.max_size.is_none()
        && q.modified_after.is_none()
        && q.modified_before.is_none()
}
fn refines(new: &SearchQuery, old: &SearchQuery) -> bool {
    if new.regex || old.regex {
        return false;
    }
    let a = SearchQuery {
        name: None,
        ..query::filter_key_for(new)
    };
    let b = SearchQuery {
        name: None,
        ..query::filter_key_for(old)
    };
    if a != b {
        return false;
    }
    let new = new.name.as_deref().unwrap_or_default();
    let old = old.name.as_deref().unwrap_or_default();
    if a.case_sensitive {
        new.contains(old)
    } else {
        new.to_lowercase().contains(&old.to_lowercase())
    }
}
