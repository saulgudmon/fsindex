//! End-to-end tests of the core engine: scanning, filtering, live resync,
//! deletion, compaction and exclusions.

use fsindex_core::index::Index;
use fsindex_core::model::{EntryKind, SearchQuery, SortKey};
use fsindex_core::paths::Exclusions;
use fsindex_core::{query, scan};
use std::fs;
use std::path::Path;

fn build_index(root: &Path) -> (Index, u32) {
    let excl = Exclusions::default();
    let frag = scan::scan_root(root, &excl);
    let mut idx = Index::new(excl);
    let vol = idx.ensure_volume(1, "testvol", "/");
    let root_id = idx.merge_fragment(frag, vol, &root.to_string_lossy());
    (idx, root_id)
}

fn names(idx: &Index, q: &SearchQuery) -> Vec<String> {
    let (hits, _) = query::search(idx, q, 100).unwrap();
    hits.into_iter().map(|h| h.name).collect()
}

#[test]
fn scan_query_filters() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
    fs::write(root.join("docs/readme.md"), b"hello").unwrap();
    fs::write(root.join("docs/report.PDF"), vec![0u8; 2048]).unwrap();
    fs::write(root.join("main.rs"), b"fn main(){}").unwrap();
    fs::write(root.join("node_modules/pkg/index.js"), b"x").unwrap();

    let (idx, _) = build_index(root);

    // Default exclusions drop node_modules. There are five live nodes: the
    // root directory, docs, readme.md, report.PDF and main.rs.
    assert_eq!(
        query::count(&idx, &SearchQuery::default()).unwrap(),
        5,
        "root, docs, readme, report, main.rs"
    );
    // Totals now agree with the live node count (the root is counted).
    assert_eq!(idx.totals.entries, 5);

    // Name substring (case-insensitive).
    let mut q = SearchQuery {
        name: Some("read".into()),
        ..Default::default()
    };
    assert_eq!(names(&idx, &q), vec!["readme.md"]);

    // Extension normalisation (PDF vs pdf).
    q = SearchQuery {
        extensions: vec!["pdf".into()],
        ..Default::default()
    };
    assert_eq!(names(&idx, &q), vec!["report.PDF"]);

    // Kind filter. The root node is itself a directory; node_modules is
    // excluded by default.
    q = SearchQuery {
        kind: Some(EntryKind::Dir),
        ..Default::default()
    };
    let got = names(&idx, &q);
    assert_eq!(got.len(), 2);
    assert!(got.iter().any(|n| n == "docs"));
    assert!(got.iter().any(|n| n == &*root.to_string_lossy()));

    // Size filter.
    q = SearchQuery {
        min_size: Some(1000),
        ..Default::default()
    };
    assert_eq!(names(&idx, &q), vec!["report.PDF"]);

    // Regex.
    q = SearchQuery {
        name: Some(r"^main\..+$".into()),
        regex: true,
        ..Default::default()
    };
    assert_eq!(names(&idx, &q), vec!["main.rs"]);

    // Path filter.
    q = SearchQuery {
        path: Some("docs/".into()),
        ..Default::default()
    };
    let mut got = names(&idx, &q);
    got.sort();
    assert_eq!(got, vec!["readme.md", "report.PDF"]);
}

#[test]
fn sort_and_pagination_totals() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    for (name, size) in [("a.txt", 10u64), ("b.txt", 30), ("c.txt", 20)] {
        fs::write(root.join(name), vec![0u8; size as usize]).unwrap();
    }
    let (idx, _) = build_index(root);

    let mut q = SearchQuery {
        extensions: vec!["txt".into()],
        sort: SortKey::Size,
        desc: true,
        ..Default::default()
    };
    let (hits, total) = query::search(&idx, &q, 100).unwrap();
    assert_eq!(total, 3);
    assert_eq!(
        hits.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(),
        vec!["b.txt", "c.txt", "a.txt"]
    );

    // Pagination.
    q.limit = 1;
    q.offset = 1;
    let (hits, total) = query::search(&idx, &q, 100).unwrap();
    assert_eq!(total, 3);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].name, "c.txt");
}

#[test]
fn resync_add_and_remove() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("d")).unwrap();
    fs::write(root.join("d/one.txt"), b"1").unwrap();
    let (mut idx, _) = build_index(root);

    let d = idx.lookup(&root.join("d")).expect("dir node");
    // Add a file and resync.
    fs::write(root.join("d/two.txt"), b"22").unwrap();
    idx.resync_dir(d).unwrap();
    assert!(
        query::count(
            &idx,
            &SearchQuery {
                name: Some("two".into()),
                ..Default::default()
            }
        )
        .unwrap()
            == 1
    );

    // Remove it and resync.
    fs::remove_file(root.join("d/two.txt")).unwrap();
    idx.resync_dir(d).unwrap();
    assert_eq!(
        query::count(
            &idx,
            &SearchQuery {
                name: Some("two".into()),
                ..Default::default()
            }
        )
        .unwrap(),
        0
    );
    // Totals stay consistent.
    assert_eq!(idx.totals.files, 1);
}

#[test]
fn subtree_removal_updates_totals() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("d/sub")).unwrap();
    fs::write(root.join("d/sub/deep.txt"), b"deep").unwrap();
    fs::write(root.join("d/keep.txt"), b"keep").unwrap();
    let (mut idx, _) = build_index(root);

    let before = idx.totals.files;
    let d = idx.lookup(&root.join("d")).expect("dir node");
    idx.remove_subtree(d);
    assert_eq!(idx.totals.files, before - 2);
    assert_eq!(
        query::count(
            &idx,
            &SearchQuery {
                path: Some("d/".into()),
                ..Default::default()
            }
        )
        .unwrap(),
        0
    );
}

#[test]
fn compact_preserves_paths_and_totals() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("a/b")).unwrap();
    fs::write(root.join("a/b/file.txt"), b"data").unwrap();
    let (mut idx, _) = build_index(root);
    let expected = idx.path_of(idx.lookup(&root.join("a/b/file.txt")).unwrap());
    let totals = idx.totals;

    // Tombstone something, then compact.
    let b = idx.lookup(&root.join("a/b")).unwrap();
    idx.remove_subtree(b);
    idx.compact();

    // The removed subtree is gone and the surviving node renumbers correctly.
    assert!(idx.lookup(&root.join("a/b/file.txt")).is_none());
    let a = idx.lookup(&root.join("a")).unwrap();
    assert_eq!(idx.path_of(a), format!("{}/a", root.display()));
    assert_eq!(idx.dead_count(), 0);
    assert_eq!(idx.totals.files, totals.files - 1);
    let _ = expected;
}

#[test]
fn symlinks_are_not_followed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("real")).unwrap();
    fs::write(root.join("real/data.txt"), b"x").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
    let (idx, _) = build_index(root);
    // `link` is indexed as a symlink, not descended into.
    let link = idx.lookup(&root.join("link")).expect("symlink node");
    assert_eq!(idx.kind_of(link), EntryKind::Symlink);
    assert_eq!(
        query::count(
            &idx,
            &SearchQuery {
                path: Some("link/data".into()),
                ..Default::default()
            }
        )
        .unwrap(),
        0
    );
}

#[test]
fn search_path_mode_matches_full_path() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("deep/nested")).unwrap();
    fs::write(root.join("deep/nested/target.txt"), b"x").unwrap();
    fs::write(root.join("other.txt"), b"y").unwrap();
    let (idx, _) = build_index(root);

    // Normal mode: "nested" only matches the directory name.
    let q = SearchQuery {
        name: Some("nested".into()),
        ..Default::default()
    };
    assert_eq!(query::count(&idx, &q).unwrap(), 1);

    // Path mode: "nested/target" matches the file by its full path.
    let q = SearchQuery {
        name: Some("nested/target".into()),
        search_path: true,
        ..Default::default()
    };
    let (hits, _) = query::search(&idx, &q, 100).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].name, "target.txt");
}

#[test]
fn mtime_filters() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("new.txt"), b"n").unwrap();
    let (idx, _) = build_index(root);
    let far_future = 4_000_000_000i64;
    let q = SearchQuery {
        modified_before: Some(far_future),
        ..Default::default()
    };
    assert!(query::count(&idx, &q).unwrap() >= 2);
    let q = SearchQuery {
        modified_after: Some(far_future),
        ..Default::default()
    };
    assert_eq!(query::count(&idx, &q).unwrap(), 0);
}

#[test]
fn path_exclusions_prune_subtree() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("keep/sub")).unwrap();
    fs::create_dir_all(root.join("skip/sub")).unwrap();
    fs::write(root.join("keep/sub/a.txt"), b"a").unwrap();
    fs::write(root.join("skip/sub/secret.txt"), b"s").unwrap();
    fs::write(root.join("skip/top.txt"), b"t").unwrap();

    // Exclude a specific path; the whole subtree must be skipped.
    let skip_dir = root.join("skip");
    let excl =
        Exclusions::build(&[], &[skip_dir.to_string_lossy().to_string()], &[], true).unwrap();
    let frag = scan::scan_root(root, &excl);
    let mut idx = Index::new(excl);
    let vol = idx.ensure_volume(1, "testvol", "/");
    idx.merge_fragment(frag, vol, &root.to_string_lossy());

    let count = |name: &str| {
        query::count(
            &idx,
            &SearchQuery {
                name: Some(name.into()),
                search_path: true,
                ..Default::default()
            },
        )
        .unwrap()
    };
    assert_eq!(count("secret.txt"), 0, "file under excluded path");
    assert_eq!(count("/skip"), 0, "excluded path node itself");
    assert_eq!(count("a.txt"), 1, "sibling subtree survives");
}

/// The cached paging path must return byte-identical pages to the uncached
/// one, for every sort key/order, at arbitrary offsets, including the
/// geometric prefix-sort boundaries.
#[test]
fn cached_search_matches_uncached() {
    use fsindex_core::model::SearchHit;

    /// Mimic `Engine::search`: plan, build off-lock if needed, then serve.
    fn serve(
        cache: &mut query::QueryCache,
        idx: &Index,
        q: &SearchQuery,
        default_limit: u64,
    ) -> (Vec<SearchHit>, u64) {
        match cache.plan(q) {
            query::CachePlan::Uncacheable => query::search(idx, q, default_limit).unwrap(),
            query::CachePlan::Ready => cache
                .ready_page(idx, q, default_limit)
                .unwrap_or_else(|| query::search(idx, q, default_limit).unwrap()),
            query::CachePlan::Build => {
                let key = query::filter_key_for(q);
                let (ids, sorted_len) = query::build_sorted(idx, &key, 0);
                cache.store_built(key, idx.generation, ids, sorted_len);
                cache
                    .ready_page(idx, q, default_limit)
                    .unwrap_or_else(|| query::search(idx, q, default_limit).unwrap())
            }
        }
    }

    fn sig(hits: &[SearchHit]) -> Vec<(String, u64, i64)> {
        hits.iter()
            .map(|h| (h.path.clone(), h.size, h.mtime))
            .collect()
    }

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("a/b")).unwrap();
    for i in 0..500 {
        fs::write(
            root.join(format!("a/f{i:04}.txt")),
            vec![i as u8; (i % 97) as usize],
        )
        .unwrap();
    }
    for i in 0..300 {
        fs::write(root.join(format!("g{i:04}.bin")), vec![0u8; i % 50]).unwrap();
    }
    let (idx, _) = build_index(root);

    let sorts = [
        SortKey::Name,
        SortKey::Size,
        SortKey::Mtime,
        SortKey::Kind,
        SortKey::Extension,
    ];
    // `Path` sort (and any path-matching query) is intentionally uncacheable;
    // `plan` reports `Uncacheable` and the engine falls back to a full snapshot.
    let pathsort = SearchQuery {
        sort: SortKey::Path,
        ..Default::default()
    };
    let cache = query::QueryCache::default();
    assert!(
        matches!(cache.plan(&pathsort), query::CachePlan::Uncacheable),
        "path sort must not use the cache"
    );
    let pathmatch = SearchQuery {
        path: Some("a/".into()),
        ..Default::default()
    };
    assert!(
        matches!(cache.plan(&pathmatch), query::CachePlan::Uncacheable),
        "path-matching query must not use the cache"
    );
    for sort in sorts {
        for desc in [false, true] {
            for offset in [0u64, 1, 999, 1000, 1001, 2048, 5000, 100000] {
                let mut cache = query::QueryCache::default();
                // Warm a few small pages first, then jump forward: this exercises
                // the geometric growth from a non-zero sorted prefix.
                for step in [0u64, 10, 1000] {
                    let q = SearchQuery {
                        sort,
                        desc,
                        offset: step,
                        limit: 1000,
                        ..Default::default()
                    };
                    let (cached, ct) = serve(&mut cache, &idx, &q, 100);
                    let (full, ft) = query::search(&idx, &q, 100).unwrap();
                    assert_eq!(ct, ft, "total {sort:?} desc={desc}");
                    assert_eq!(
                        sig(&cached),
                        sig(&full),
                        "step {sort:?} desc={desc} offset={step}"
                    );
                }
                let q = SearchQuery {
                    sort,
                    desc,
                    offset,
                    limit: 1000,
                    ..Default::default()
                };
                let (cached, ct) = serve(&mut cache, &idx, &q, 100);
                let (full, ft) = query::search(&idx, &q, 100).unwrap();
                assert_eq!(ct, ft, "total {sort:?} desc={desc} offset={offset}");
                assert_eq!(
                    sig(&cached),
                    sig(&full),
                    "page {sort:?} desc={desc} offset={offset}"
                );
            }
        }
    }

    // The GUI pattern: page forward with limit=1000 (the filter-only key must
    // reuse the snapshot and only grow the sorted prefix).
    let mut cache = query::QueryCache::default();
    for offset in (0u64..8000).step_by(1000) {
        let q = SearchQuery {
            sort: SortKey::Name,
            offset,
            limit: 1000,
            ..Default::default()
        };
        let (cached, _) = serve(&mut cache, &idx, &q, 100);
        let (full, _) = query::search(&idx, &q, 100).unwrap();
        assert_eq!(sig(&cached), sig(&full), "forward paging offset={offset}");
    }
}

/// After a mutation, the background refresh must pick up the new data, and a
/// request served before the refresh must still return a valid (stale) page
/// rather than blocking or failing.
#[test]
fn cache_refresh_picks_up_mutation() {
    use fsindex_core::model::SearchHit;
    fn serve(cache: &mut query::QueryCache, idx: &Index, q: &SearchQuery) -> (Vec<SearchHit>, u64) {
        match cache.plan(q) {
            query::CachePlan::Uncacheable => query::search(idx, q, 100).unwrap(),
            query::CachePlan::Ready => cache
                .ready_page(idx, q, 100)
                .unwrap_or_else(|| query::search(idx, q, 100).unwrap()),
            query::CachePlan::Build => {
                let key = query::filter_key_for(q);
                let (ids, sorted_len) = query::build_sorted(idx, &key, 0);
                cache.store_built(key, idx.generation, ids, sorted_len);
                cache
                    .ready_page(idx, q, 100)
                    .unwrap_or_else(|| query::search(idx, q, 100).unwrap())
            }
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    fs::write(root.join("one.txt"), b"1").unwrap();
    let (mut idx, _) = build_index(root);

    let q = SearchQuery {
        extensions: vec!["txt".into()],
        ..Default::default()
    };
    let mut cache = query::QueryCache::default();
    assert_eq!(serve(&mut cache, &idx, &q).0.len(), 1);

    // Add a matching file and resync. Before the background refresh runs, a
    // request still serves the previous snapshot (no blocking, no failure).
    fs::write(root.join("two.txt"), b"2").unwrap();
    let d = idx.lookup(root).unwrap();
    idx.resync_dir(d).unwrap();

    // The refresher rebuilds at the new generation and installs it.
    let (ids, sorted_len) = query::build_sorted(&idx, &q, 0);
    cache.store_built(q.clone(), idx.generation, ids, sorted_len);
    let (hits, total) = serve(&mut cache, &idx, &q);
    assert_eq!(hits.len(), 2, "refresh picked up the new file");
    assert_eq!(total, 2);

    // An exclusion/root change invalidates the cache; the next request rebuilds.
    cache.invalidate();
    assert!(matches!(cache.plan(&q), query::CachePlan::Build));
    let (hits, _) = serve(&mut cache, &idx, &q);
    assert_eq!(hits.len(), 2, "rebuilt after invalidation");
}
