# fsindex

Native Rust desktop application. Work in this repository. The previous
implementation is archived at https://github.com/saulgudmon/fsindex-mk1;
its former local checkout is no longer available.

- The GUI owns one in-process `fsindex-core::Engine`. Do not reintroduce a
  daemon/socket/HTTP dependency into desktop search or scrolling.
- Names and metadata only. No content indexing, root permissions, or implicit
  writes to the original app's configuration or installation.
- Keep scan/query logic in `fsindex-core`, reusable by a future headless binary.
- Filtering/sorting runs on the search worker. Rendering uses stable owned
  snapshots; it must not take index locks or fetch pages.
- Never publish outdated query revisions or retain raw node IDs across index
  mutation/compaction. New search-affecting state must invalidate generation.
- Preserve inotify overflow reconciliation, exclusions, volume health and honest
  scan freshness when changing the engine.
- Current watcher storage is process-global: one engine per desktop process.
- Config namespace is `fsindex-mk2`, separate from the original app.

Run `cargo fmt --all --check`, `cargo clippy --workspace --all-targets
--all-features -- -D warnings`, and `cargo test --workspace`. Scroll tests must
check visible geometry at deep offsets, not merely row counts. Optional native
frame capture is documented in README.md.
