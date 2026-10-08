//! Latest-request-wins mailbox. No unbounded queue of searches or results.
use fsindex_core::{
    query::{ResultSnapshot, SearchCatalog, SnapshotSearch},
    Engine, SearchQuery, StatusResponse,
};
use parking_lot::{Condvar, Mutex};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread::JoinHandle,
    time::Duration,
};

pub struct Update {
    pub revision: u64,
    pub status: StatusResponse,
    pub result: Option<Result<ResultSnapshot, String>>,
}

#[derive(Default)]
struct Published {
    catalog: Option<Arc<SearchCatalog>>,
    status: Option<StatusResponse>,
}

struct Mailbox {
    published: Mutex<Published>,
    cache: Mutex<VecDeque<(SearchQuery, ResultSnapshot)>>,
    request: Mutex<(u64, SearchQuery)>,
    wake: Condvar,
    revision: AtomicU64,
    live_generation: AtomicU64,
    stopped: AtomicBool,
    output: Mutex<Option<Update>>,
    retired: Mutex<Vec<ResultSnapshot>>,
}

pub struct SearchWorker {
    mailbox: Arc<Mailbox>,
    handle: Option<JoinHandle<()>>,
    publisher: Option<JoinHandle<()>>,
}

impl SearchWorker {
    pub fn new(engine: Arc<Engine>, repaint: impl Fn() + Send + 'static) -> Self {
        let mailbox = Arc::new(Mailbox {
            published: Mutex::new(Published::default()),
            cache: Mutex::new(VecDeque::new()),
            request: Mutex::new((1, SearchQuery::default())),
            wake: Condvar::new(),
            revision: AtomicU64::new(1),
            live_generation: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
            output: Mutex::new(None),
            retired: Mutex::new(Vec::new()),
        });
        let publish = Arc::clone(&mailbox);
        let publisher = std::thread::Builder::new()
            .name("fsindex-catalog".into())
            .spawn(move || {
                let mut generation = None;
                while !publish.stopped.load(Ordering::Acquire) {
                    let input = {
                        let state = engine.shared().read();
                        publish
                            .live_generation
                            .store(state.index.generation, Ordering::Release);
                        (generation != Some(state.index.generation))
                            .then(|| SearchCatalog::capture(&state.index))
                    };
                    if let Some(input) = input {
                        let Some(catalog) = SearchCatalog::prepare(input, || {
                            publish.stopped.load(Ordering::Acquire)
                        }) else {
                            break;
                        };
                        generation = Some(catalog.generation());
                        publish.published.lock().catalog = Some(catalog);
                    }
                    publish.published.lock().status = Some(engine.status());
                    {
                        let _request = publish.request.lock();
                        publish.wake.notify_one();
                    }
                    for _ in 0..8 {
                        if publish.stopped.load(Ordering::Acquire) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            })
            .expect("start catalogue publisher");
        let shared = Arc::clone(&mailbox);
        let handle = std::thread::Builder::new()
            .name("fsindex-search".into())
            .spawn(move || {
                let mut last_build = None;
                let mut search = SnapshotSearch::default();
                while !shared.stopped.load(Ordering::Acquire) {
                    drop(std::mem::take(&mut *shared.retired.lock()));
                    let (revision, query) = shared.request.lock().clone();
                    let cancelled = || {
                        shared.stopped.load(Ordering::Acquire)
                            || shared.revision.load(Ordering::Acquire) != revision
                    };
                    let (catalog, status) = {
                        let published = shared.published.lock();
                        (published.catalog.clone(), published.status.clone())
                    };
                    let (Some(catalog), Some(status)) = (catalog, status) else {
                        let mut request = shared.request.lock();
                        shared
                            .wake
                            .wait_for(&mut request, Duration::from_millis(20));
                        continue;
                    };
                    let key = (revision, catalog.generation());
                    let result = if last_build == Some(key) {
                        None
                    } else {
                        match search.search_catalog(catalog, &query, cancelled) {
                            Ok(Some(snapshot)) => {
                                last_build = Some(key);
                                let mut cache = shared.cache.lock();
                                cache.retain(|(_, rows)| rows.generation == snapshot.generation);
                                cache.push_front((query.clone(), snapshot.clone()));
                                while cache.len() > 32
                                    || (cache.len() > 1
                                        && cache
                                            .iter()
                                            .map(|(_, s)| s.rows.len() * 4)
                                            .sum::<usize>()
                                            > 64 * 1024 * 1024)
                                {
                                    cache.pop_back();
                                }
                                Some(Ok(snapshot))
                            }
                            Ok(None) => continue,
                            Err(e) => {
                                last_build = Some(key);
                                Some(Err(e.to_string()))
                            }
                        }
                    };
                    if !cancelled() {
                        let mut output = shared.output.lock();
                        // Preserve an unread result when only status changed.
                        let result = result.or_else(|| {
                            output
                                .as_mut()
                                .filter(|u| u.revision == revision)
                                .and_then(|u| u.result.take())
                        });
                        *output = Some(Update {
                            revision,
                            result,
                            status,
                        });
                        drop(output);
                        repaint();
                    }
                    let mut request = shared.request.lock();
                    if request.0 == revision && !shared.stopped.load(Ordering::Acquire) {
                        shared
                            .wake
                            .wait_for(&mut request, Duration::from_millis(100));
                    }
                }
                drop(std::mem::take(&mut *shared.retired.lock()));
            })
            .expect("start search worker");
        Self {
            mailbox,
            handle: Some(handle),
            publisher: Some(publisher),
        }
    }

    /// Ready results only: never touch the live index or wait for a rebuild.
    pub fn refreshing(&self, generation: u64) -> bool {
        self.mailbox.live_generation.load(Ordering::Acquire) != generation
    }

    pub fn cached(&self, query: &SearchQuery) -> Option<ResultSnapshot> {
        let catalog = self.mailbox.published.try_lock()?.catalog.clone()?;
        if let Some(baseline) = catalog.baseline(query) {
            return Some(baseline);
        }
        self.mailbox
            .cache
            .try_lock()?
            .iter()
            .find(|(key, rows)| key == query && rows.generation == catalog.generation())
            .map(|(_, rows)| rows.clone())
    }

    pub fn submit(&self, query: SearchQuery) -> u64 {
        let mut request = self.mailbox.request.lock();
        let revision = self.mailbox.revision.fetch_add(1, Ordering::AcqRel) + 1;
        *request = (revision, query);
        self.mailbox.wake.notify_one();
        revision
    }

    /// Release large result arrays and their last row references off the UI thread.
    pub fn retire(&self, snapshot: Option<ResultSnapshot>) {
        if let Some(snapshot) = snapshot {
            self.mailbox.retired.lock().push(snapshot);
        }
    }

    pub fn take(&self) -> Option<Update> {
        self.mailbox.output.try_lock()?.take()
    }
}

impl Drop for SearchWorker {
    fn drop(&mut self) {
        // Pair shutdown with the wait mutex so a notification cannot be lost.
        {
            let _request = self.mailbox.request.lock();
            self.mailbox.stopped.store(true, Ordering::Release);
            self.mailbox.wake.notify_one();
        }
        if let Some(handle) = self.publisher.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsindex_core::{config::RootConfig, Config};
    use std::time::Instant;

    #[test]
    fn latest_query_wins_and_unread_results_survive_status_ticks() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("first.txt"), "").unwrap();
        std::fs::write(root.path().join("latest.txt"), "").unwrap();
        let engine = Engine::new(
            Config {
                roots: vec![RootConfig::new(root.path().to_string_lossy())],
                ..Default::default()
            },
            root.path().join("config.toml"),
        )
        .unwrap();
        let worker = SearchWorker::new(Arc::clone(&engine), || {});
        for i in 0..50 {
            worker.submit(SearchQuery {
                name: Some(format!("old-{i}")),
                ..Default::default()
            });
        }
        let revision = worker.submit(SearchQuery {
            name: Some("latest".into()),
            ..Default::default()
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while worker
            .mailbox
            .output
            .lock()
            .as_ref()
            .is_none_or(|u| u.revision != revision)
        {
            assert!(Instant::now() < deadline, "search worker did not finish");
            std::thread::sleep(Duration::from_millis(10));
        }
        // Leave the result unread across at least one status-only update.
        std::thread::sleep(Duration::from_millis(900));
        let update = worker.take().unwrap();
        assert_eq!(update.revision, revision);
        let snapshot = update.result.unwrap().unwrap();
        assert_eq!(snapshot.rows.len(), 1);
        assert_eq!(snapshot.rows[0].name, "latest.txt");
        // A blocked live writer/publisher must never block typing or clearing.
        let state = engine.shared().write();
        let revision = worker.submit(SearchQuery {
            name: Some("first".into()),
            ..Default::default()
        });
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            if let Some(update) = worker.take().filter(|u| u.revision == revision) {
                let result = update.result.unwrap().unwrap();
                assert_eq!(result.rows[0].name, "first.txt");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "typing waited for live index lock"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let cleared = worker
            .cached(&SearchQuery::default())
            .expect("clear has a ready baseline");
        assert!(cleared.rows.iter().any(|row| row.name == "latest.txt"));
        drop(state);
    }
}
