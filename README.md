# fsindex

A native Rust desktop remake of [fsindex](https://fsindex.zyx0.xyz/).
The GUI owns the live in-memory filename/metadata index. There is no daemon,
local server, socket, webview, or serialized request in the desktop search path.
Closing the application stops indexing.

This is the first working desktop foundation, not feature parity with the old
application. The previous implementation is archived at
[fsindex-mk1](https://github.com/saulgudmon/fsindex-mk1).

## Run

Requires Linux, Rust 1.95 or newer, and a Wayland or X11 desktop with OpenGL.
`xdg-open` handles opening files and folders.

```sh
cargo run --release -- --root ~/Documents --root /path/to/another/volume
```

Without `--root`, it reads `~/.config/fsindex-mk2/fsindex.toml` (respecting
`XDG_CONFIG_HOME`), or indexes the home directory with default exclusions if
that file is absent. It does not read or overwrite the original app's config.
An explicitly supplied missing or invalid config is an error.

```sh
cargo run --release -- --config ./fsindex.example.toml
```

The window uses a compact dark file-browser layout with a search field and
All/Files/Folders/Links filter. The virtual table has aligned Name, Path, Size,
Modified (local time), and Type columns; click a heading to sort. Drag the
Name/Path divider to resize the Name column, or double-click it to reset.
Alternating rows, full-row selection and colored file-type icons aid scanning.
Long text truncates within its own cell; hovering shows the full path.

Double-click a row to open it. Right-click anywhere on a row for Open, Open
containing folder, and Copy path. Enter opens a selected row; Ctrl+C copies its
path. Ctrl+F focuses search and Escape clears a focused search field.

The bottom bar contains case sensitivity (`Aa`), regex (`.*`), and full-path
(`Path`) toggles, counts and index freshness. **Menu → Index details** shows
roots and configuration; **Menu → About fsindex** shows shortcuts. Initial scan
results are partial; the status bar makes that visible.

Typing submits immediately. The previous results stay visible (marked as previous
items and not actionable) until the newest query is ready, then the table swaps
results in one step. A syntax error keeps that previous view while you correct it.
Refining a literal search filters already sorted matches. Up to 32 recent searches
(with a 64 MiB result-position budget per cache) are retained for backspacing.
Clearing a name-sorted search immediately reuses the full prepared list, even
while live changes are being processed. Only visible rows expand display metadata.

Results refresh automatically until you start browsing (scrolling or selecting).
After that, new snapshots wait behind **Apply updates**, keeping the current
rows stable. Applying updates returns to the top; changing the query or sort
starts a new result set. The watcher continues running while the view is held.

## Architecture

```text
fsindex-desktop (one process)
  ├─ indexer thread ── initial scan → watchers/reconciliation
  │                        │
  │                   Arc<Engine>
  │                        │ direct Rust access
  ├─ catalogue publisher ── frozen copy → prepared name order
  │                                      │ immutable catalogue
  ├─ search worker ── filter references → owned ResultSnapshot
  │                                      │ latest-result mailbox
  └─ egui UI ─────────────────────────────┘
       paints visible rows from the snapshot; no engine locks while scrolling
```

- `crates/fsindex-core`: reused scanner, exclusions, compact index, inotify
  watcher, volume lifecycle and query code. Socket/client/updater modules were
  removed. The engine constructor no longer takes a socket path.
- `apps/fsindex-desktop`: native egui/eframe window and a background search
  worker. There is one engine per application process.
- Search requests replace pending requests. Revision checks reject outdated
  results; filtering, catalogue preparation, bounded sort runs and merges check
  cancellation. Large replaced snapshots are released on the worker thread.
- Snapshots share immutable owned rows, so deletions, metadata updates and index compaction
  cannot corrupt existing scroll positions or make a row refer to another node.
- The list virtualizes rendering over the entire snapshot. There is no paging,
  load-more request, or re-sort when dragging the scrollbar.
- Names and metadata only: no file-content searching, and no root privileges.
  File contents are opened only by the user's chosen external application.

### Current tradeoffs and next work

The catalogue publisher copies nodes and shared names under a short engine read
lock, then sorts and prepares packed search text outside that lock. Search and
rendering use only the owned immutable catalogue. Live mutations and compaction
cannot invalidate its local positions. Updates are coalesced in the background;
the status bar reports **Updating index…** until the displayed generation catches
up. Watcher event timestamps alone do not invalidate searchable data.

Name searches filter pre-sorted positions, splitting large inputs over up to four
workers. Empty searches reuse prepared arrays. Other sort orders and complex
path/regex filters can still require more work. Live catalogue rebuilding needs
additional memory for frozen nodes, packed names and result positions; the old
catalogue remains alive while a displayed snapshot uses it. Core query code is
optimized in local debug builds too.

This independently implemented Rust design follows architectural ideas studied in
[FSearch's index-store search](https://github.com/cboxdoerfer/fsearch/blob/cb6cd67f9ef12fedcf6a160b6a38faf492f7c989/src/fsearch_database_index_store.c#L1348-L1497):
pre-sorted entry arrays, lightweight result references, parallel filtering, and
reusing the full array for empty queries. No FSearch implementation code was copied.

The repeatable synthetic typing comparison is:

```sh
cargo run --release -p fsindex-core --example search_typing -- 1000000
```

It compares the stateless rebuild with a warm interactive session over the same
index and checks identical ordered paths. It measures query work, not total
keystroke-to-screen latency; real paths, filters, cache eviction, and index churn
will affect performance.

Settings editing, keyboard result navigation, previews, system-theme integration,
single-instance activation, background/tray lifetime, persistence, release
packaging and the original MCP/CLI features have not been ported. Configuration
changes currently require restarting the desktop app. Do not run multiple copies
against huge roots unless you intend to maintain multiple indexes.

A future headless executable should depend on `fsindex-core`, own its own engine,
and have no GUI dependencies. The engine itself still works without a display;
only this executable needs one. A persistent headless mode can later serve CLI/MCP
clients if desired without placing that transport back into the GUI's data path.

The future installer can select desktop or headless artifacts, with explicit
`--desktop` / `--headless` overrides. Detection should consider installed desktop
support, not just `DISPLAY`/`WAYLAND_DISPLAY`: an SSH shell on a desktop can lack
both. No installer is included until both installable artifacts exist.

## Verify

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

Tests include live filesystem changes, filtering/sorting, deletion + compaction
snapshot stability, volume-state invalidation, cancellation, latest-query wins,
actual text geometry at the end of a 100,000-row virtual list, column clipping
and alignment, full-row hit testing and sortable-header interactions. Search
tests cover same-frame keystroke submission, retained results, revision rejection,
backspace cache reuse, sort cancellation, and cached/stateless equivalence.

Optional native rendering smoke test (use a small fixture root):

```sh
FSINDEX_CAPTURE_TO=/tmp/fsindex.png cargo run --features capture -- --root /tmp/fixture
```

This development-only feature saves a frame after results arrive and closes the
window. Normal builds do not include the capture hook.
