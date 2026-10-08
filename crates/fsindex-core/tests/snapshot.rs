use fsindex_core::{index::Index, paths::Exclusions, query, scan, SearchQuery, SortKey};

fn index(root: &std::path::Path) -> Index {
    let mut index = Index::new(Exclusions::default());
    let volume = index.ensure_volume(1, "test", "/");
    let fragment = scan::scan_root(root, &Exclusions::default());
    index.merge_fragment(fragment, volume, &root.to_string_lossy());
    index
}

#[test]
fn snapshot_is_stable_after_delete_rescan_and_compaction() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "a").unwrap();
    std::fs::write(root.path().join("b.txt"), "bb").unwrap();
    let mut idx = index(root.path());
    let query = SearchQuery {
        extensions: vec!["txt".into()],
        ..Default::default()
    };
    let snapshot = query::snapshot(&idx, &query, || false).unwrap().unwrap();
    let deleted = idx.lookup(&root.path().join("a.txt")).unwrap();
    idx.remove_subtree(deleted);
    idx.compact();
    std::fs::remove_file(root.path().join("a.txt")).unwrap();
    std::fs::write(root.path().join("c.txt"), "ccc").unwrap();
    let root_id = idx.lookup(root.path()).unwrap();
    idx.resync_dir(root_id).unwrap();
    assert_eq!(
        snapshot
            .rows
            .iter()
            .map(|h| h.name.as_str())
            .collect::<Vec<_>>(),
        ["a.txt", "b.txt"]
    );
    assert_eq!(snapshot.rows[0].size, 1);
    assert!(snapshot.rows[0].path.ends_with("/a.txt"));
    let fresh = query::snapshot(&idx, &query, || false).unwrap().unwrap();
    assert_eq!(
        fresh
            .rows
            .iter()
            .map(|h| h.name.as_str())
            .collect::<Vec<_>>(),
        ["b.txt", "c.txt"]
    );
}

#[test]
fn full_snapshot_ignores_paging_and_handles_path_search_and_sort() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("nested")).unwrap();
    for name in ["a", "c", "b"] {
        std::fs::write(root.path().join("nested").join(name), "").unwrap();
    }
    let idx = index(root.path());
    let query = SearchQuery {
        path: Some("nested/".into()),
        sort: SortKey::Path,
        desc: true,
        limit: 1,
        offset: 100,
        ..Default::default()
    };
    let snapshot = query::snapshot(&idx, &query, || false).unwrap().unwrap();
    assert_eq!(
        snapshot
            .rows
            .iter()
            .map(|h| h.name.as_str())
            .collect::<Vec<_>>(),
        ["c", "b", "a"]
    );
}

#[test]
fn cancelled_and_invalid_queries_do_not_publish_results() {
    let root = tempfile::tempdir().unwrap();
    let idx = index(root.path());
    assert!(query::snapshot(&idx, &SearchQuery::default(), || true)
        .unwrap()
        .is_none());
    let bad = SearchQuery {
        name: Some("[".into()),
        regex: true,
        ..Default::default()
    };
    assert!(query::snapshot(&idx, &bad, || false).is_err());
}

#[test]
fn volume_changes_invalidate_snapshot_generation() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file.txt"), "").unwrap();
    let mut idx = index(root.path());
    let query = SearchQuery::default();
    let before = query::snapshot(&idx, &query, || false).unwrap().unwrap();
    let vol = idx.volume_of(idx.lookup(root.path()).unwrap());
    idx.volume_rec_mut(vol).state = fsindex_core::VolumeState::Offline;
    let after = query::snapshot(&idx, &query, || false).unwrap().unwrap();
    assert!(after.generation > before.generation);
    assert!(!before.rows.is_empty());
    assert!(after.rows.is_empty());
}

#[test]
fn typing_reuses_rows_and_backspacing_reuses_result_arrays() {
    use std::sync::Arc;
    let root = tempfile::tempdir().unwrap();
    for name in [
        "music.flac",
        "musical.mp3",
        "museum.txt",
        "photo.jpg",
        "MUSIC.txt",
        "MÜNCHEN.txt",
    ] {
        std::fs::write(root.path().join(name), "").unwrap();
    }
    let idx = index(root.path());
    let mut session = query::SnapshotSearch::default();
    let all = session
        .search(&idx, &SearchQuery::default(), || false)
        .unwrap()
        .unwrap();
    let q = SearchQuery {
        name: Some("mu".into()),
        ..Default::default()
    };
    let broad = session.search(&idx, &q, || false).unwrap().unwrap();
    for row in broad.rows.iter() {
        assert!(
            all.rows.iter().any(|original| Arc::ptr_eq(row, original)),
            "reuse metadata, don't rebuild paths"
        );
    }
    let narrow = session
        .search(
            &idx,
            &SearchQuery {
                name: Some("music".into()),
                ..q.clone()
            },
            || false,
        )
        .unwrap()
        .unwrap();
    assert_eq!(narrow.rows.len(), 3);
    let back = session.search(&idx, &q, || false).unwrap().unwrap();
    assert!(back.rows.ptr_eq(&broad.rows));
    let cleared = session
        .search(&idx, &SearchQuery::default(), || false)
        .unwrap()
        .unwrap();
    assert!(cleared.rows.ptr_eq(&all.rows));
}

#[test]
fn interactive_results_match_stateless_across_filters_sorts_and_mutations() {
    use fsindex_core::{index::EntryMeta, EntryKind};
    let mut idx = Index::new(Exclusions::default());
    let vol = idx.ensure_volume(1, "fixture", "/fixture");
    let root = idx.ensure_root(vol, "/fixture");
    for i in 0..5100 {
        idx.upsert_child(
            root,
            &format!(
                "{}-{:05}.{}",
                ["Music", "photo", "MÜNCHEN"][i % 3],
                5100 - i,
                ["flac", "jpg"][i % 2]
            ),
            EntryMeta {
                kind: EntryKind::File,
                size: (i % 101) as u64,
                mtime: i as i64,
                mtime_ns: 0,
                inode: i as u64,
            },
        );
    }
    let mut session = query::SnapshotSearch::default();
    for sort in [
        SortKey::Name,
        SortKey::Path,
        SortKey::Size,
        SortKey::Mtime,
        SortKey::Kind,
        SortKey::Extension,
    ] {
        for desc in [false, true] {
            for (name, regex, case_sensitive, search_path) in [
                ("", false, false, false),
                ("m", false, false, false),
                ("mu", false, false, false),
                ("Music", false, true, false),
                ("MÜ", false, false, false),
                ("m", true, false, false),
                ("^(Music|photo)", true, false, false),
                ("fixture/m", false, false, true),
            ] {
                let q = SearchQuery {
                    name: (!name.is_empty()).then(|| name.into()),
                    sort,
                    desc,
                    regex,
                    case_sensitive,
                    search_path,
                    ..Default::default()
                };
                let got = session.search(&idx, &q, || false).unwrap().unwrap();
                let expected = query::snapshot(&idx, &q, || false).unwrap().unwrap();
                assert_eq!(got.rows.len(), expected.rows.len());
                assert!(
                    got.rows
                        .iter()
                        .zip(expected.rows.iter())
                        .all(|(a, b)| a.path == b.path),
                    "{q:?}"
                );
            }
        }
    }
    let q = SearchQuery::default();
    let old = session.search(&idx, &q, || false).unwrap().unwrap();
    let deleted = idx
        .lookup(std::path::Path::new("/fixture/Music-05100.flac"))
        .unwrap();
    idx.remove_subtree(deleted);
    idx.compact();
    let new = session.search(&idx, &q, || false).unwrap().unwrap();
    assert_eq!(old.rows.len(), new.rows.len() + 1);
    assert!(!new.rows.iter().any(|r| r.name == "Music-05100.flac"));
    idx.volume_rec_mut(vol).state = fsindex_core::VolumeState::Offline;
    assert!(session
        .search(&idx, &q, || false)
        .unwrap()
        .unwrap()
        .rows
        .is_empty());
    let offline = SearchQuery {
        include_offline: true,
        ..q
    };
    assert_eq!(
        session
            .search(&idx, &offline, || false)
            .unwrap()
            .unwrap()
            .rows
            .len(),
        new.rows.len()
    );
}

#[test]
fn catalogue_is_lazy_and_survives_live_compaction_and_cache_eviction() {
    let root = tempfile::tempdir().unwrap();
    for name in ["music.txt", "photo.jpg", "movie.mkv"] {
        std::fs::write(root.path().join(name), "data").unwrap();
    }
    let mut idx = index(root.path());
    let catalogue =
        query::SearchCatalog::prepare(query::SearchCatalog::capture(&idx), || false).unwrap();
    let mut session = query::SnapshotSearch::default();
    let all = catalogue.baseline(&SearchQuery::default()).unwrap();
    let q = SearchQuery {
        name: Some("m".into()),
        ..Default::default()
    };
    let matches = session
        .search_catalog(catalogue.clone(), &q, || false)
        .unwrap()
        .unwrap();
    assert_eq!(
        catalogue.materialized_rows(),
        0,
        "search must not expand metadata for all matches"
    );
    let removed = idx.lookup(&root.path().join("music.txt")).unwrap();
    idx.remove_subtree(removed);
    idx.compact();
    assert!(matches
        .rows
        .iter()
        .any(|row| row.name == "music.txt" && row.size == 4));
    for n in 0..40 {
        let q = SearchQuery {
            name: Some(format!("absent-{n}")),
            ..Default::default()
        };
        session
            .search_catalog(catalogue.clone(), &q, || false)
            .unwrap();
    }
    let cleared = session
        .search_catalog(catalogue.clone(), &SearchQuery::default(), || false)
        .unwrap()
        .unwrap();
    assert!(
        all.rows.ptr_eq(&cleared.rows),
        "baseline survives cache eviction and live mutation"
    );
    let generation = idx.generation;
    let volume = idx.volume_of(idx.lookup(root.path()).unwrap());
    idx.record_volume_event(volume, 123);
    assert_eq!(
        idx.generation, generation,
        "watcher timestamps must not invalidate search data"
    );
}
