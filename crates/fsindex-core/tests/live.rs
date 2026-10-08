//! Live-index integration test: the engine must reflect filesystem changes
//! made after the initial scan, via the inotify watcher + debounce loop.

use fsindex_core::config::{Config, RootConfig};
use fsindex_core::engine::Engine;
use fsindex_core::model::SearchQuery;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn wait_until_count(engine: &Engine, name: &str, want: u64, timeout: Duration) -> u64 {
    let q = SearchQuery {
        name: Some(name.to_string()),
        ..Default::default()
    };
    let start = Instant::now();
    loop {
        let n = engine.count(&q).unwrap().count;
        if n == want {
            return n;
        }
        if start.elapsed() > timeout {
            return n;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

#[test]
fn live_index_tracks_create_and_delete() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("preexisting.txt"), b"seed").unwrap();

    let cfg = Config {
        roots: vec![RootConfig::new(root.to_string_lossy())],
        debounce_ms: 50,
        reconcile_interval_secs: 3600,
        ..Default::default()
    };
    let engine = Engine::new(cfg, PathBuf::from("/nonexistent.toml")).unwrap();

    // Drive the watcher/debounce loop on a background thread.
    let bg = engine.clone();
    let handle = std::thread::spawn(move || bg.maintenance_loop());

    let timeout = Duration::from_secs(8);

    // A new file must appear.
    std::fs::write(root.join("hello_live.txt"), b"x").unwrap();
    let n = wait_until_count(&engine, "hello_live", 1, timeout);
    assert_eq!(n, 1, "created file never appeared in the live index");

    // A newly created subtree must be discovered and watched too.
    std::fs::create_dir_all(root.join("newdir/deep")).unwrap();
    std::fs::write(root.join("newdir/deep/leaf.txt"), b"y").unwrap();
    let n = wait_until_count(&engine, "leaf", 1, timeout);
    assert_eq!(n, 1, "file in a new subtree never appeared");

    // Deleting the subtree must remove it from the index.
    std::fs::remove_dir_all(root.join("newdir")).unwrap();
    let n = wait_until_count(&engine, "leaf", 0, timeout);
    assert_eq!(n, 0, "deleted subtree still present in the index");

    // A simple file deletion must be reflected too.
    std::fs::remove_file(root.join("hello_live.txt")).unwrap();
    let n = wait_until_count(&engine, "hello_live", 0, timeout);
    assert_eq!(n, 0, "deleted file still present in the index");

    // Totals stay consistent with the surviving tree.
    let status = engine.status();
    assert_eq!(status.totals.entries, 2, "root + preexisting.txt");

    engine.request_shutdown();
    let _ = handle.join();
}

#[test]
fn reload_applies_exclusion_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir_all(root.join("keep")).unwrap();
    std::fs::create_dir_all(root.join("skip")).unwrap();
    std::fs::write(root.join("keep/a.txt"), b"a").unwrap();
    std::fs::write(root.join("skip/secret.txt"), b"s").unwrap();

    let cfg_path = tmp.path().join("fsindex.toml");
    let cfg = Config {
        roots: vec![RootConfig::new(root.to_string_lossy())],
        reconcile_interval_secs: 3600,
        ..Default::default()
    };
    cfg.save(&cfg_path).unwrap();

    let engine = Engine::new(cfg, cfg_path.clone()).unwrap();

    let count = |name: &str| {
        engine
            .count(&SearchQuery {
                name: Some(name.to_string()),
                search_path: true,
                ..Default::default()
            })
            .unwrap()
            .count
    };
    assert_eq!(count("secret.txt"), 1);

    // Exclude the `skip` subtree and reload: the engine must reconcile and drop
    // the newly-excluded entries (a directory resync alone would not).
    Config {
        roots: vec![RootConfig::new(root.to_string_lossy())],
        reconcile_interval_secs: 3600,
        exclusions: fsindex_core::config::ExclusionsConfig {
            use_defaults: true,
            paths: vec![root.join("skip").to_string_lossy().to_string()],
            ..Default::default()
        },
        ..Default::default()
    }
    .save(&cfg_path)
    .unwrap();

    engine.reload_config().unwrap();
    assert_eq!(count("secret.txt"), 0, "newly-excluded file still indexed");
    assert_eq!(count("a.txt"), 1, "sibling subtree was dropped too");
}
