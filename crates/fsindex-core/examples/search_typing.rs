//! Synthetic comparison of the stateless snapshot path and a warm typing session.
//! Run: cargo run --release -p fsindex-core --example search_typing -- 1000000
use fsindex_core::{
    index::{EntryMeta, Index},
    paths::Exclusions,
    query::{self, SnapshotSearch},
    EntryKind, SearchQuery,
};
use std::{hint::black_box, time::Instant};
fn main() {
    let count: usize = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "250000".into())
        .parse()
        .unwrap();
    let mut idx = Index::new(Exclusions::default());
    let volume = idx.ensure_volume(1, "fixture", "/fixture");
    let root = idx.ensure_root(volume, "/fixture/library");
    for i in 0..count {
        let (kind, ext) = [
            ("music", "flac"),
            ("photo", "jpg"),
            ("document", "pdf"),
            ("movie", "mkv"),
        ][i % 4];
        idx.upsert_child(
            root,
            &format!("{kind}-{i:07}.{ext}"),
            EntryMeta {
                kind: EntryKind::File,
                size: i as u64,
                mtime: 1700000000,
                mtime_ns: 0,
                inode: i as u64,
            },
        );
    }
    let mut session = SnapshotSearch::default();
    let start = Instant::now();
    black_box(
        session
            .search(&idx, &SearchQuery::default(), || false)
            .unwrap(),
    );
    println!(
        "{count} synthetic files; initial snapshot: {:.1} ms",
        start.elapsed().as_secs_f64() * 1000.0
    );
    println!("query, matches, stateless_ms, interactive_ms");
    for name in [
        "m",
        "mu",
        "mus",
        "music",
        "music-000",
        "music-0001",
        "music-000",
        "music",
        "mu",
        "m",
        "",
    ] {
        let q = SearchQuery {
            name: (!name.is_empty()).then(|| name.into()),
            ..Default::default()
        };
        let start = Instant::now();
        let baseline = query::snapshot(&idx, &q, || false).unwrap().unwrap();
        let old = start.elapsed();
        let start = Instant::now();
        let current = session.search(&idx, &q, || false).unwrap().unwrap();
        let new = start.elapsed();
        assert_eq!(baseline.rows.len(), current.rows.len());
        assert!(baseline
            .rows
            .iter()
            .zip(current.rows.iter())
            .all(|(a, b)| a.path == b.path));
        println!(
            "{name:?}, {}, {:.3}, {:.3}",
            current.rows.len(),
            old.as_secs_f64() * 1000.0,
            new.as_secs_f64() * 1000.0
        );
        black_box(current);
    }
}
