//! Download list, queue and lifecycle.

use crate::config::AppConfig;
use freedm_core::{Download, DownloadHandle, Progress, RequestSpec};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, Notify};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Waiting for a free slot.
    Queued,
    /// Probing the server before any bytes move.
    Starting,
    Running,
    Paused,
    Done,
    Failed,
    Cancelled,
}

impl Status {
    fn is_terminal(self) -> bool {
        matches!(self, Status::Done | Status::Failed | Status::Cancelled)
    }

    fn occupies_a_slot(self) -> bool {
        matches!(self, Status::Starting | Status::Running)
    }
}

/// One download as the API presents it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: Uuid,
    pub url: String,
    pub filename: String,
    pub path: Option<PathBuf>,
    pub status: Status,
    pub progress: Progress,
    pub error: Option<String>,
    pub added_at: String,
    /// Where the download came from, for display: "browser", "api", "cli".
    pub source: String,
}

struct Entry {
    item: Item,
    spec: RequestSpec,
    filename_override: Option<String>,
    handle: Option<DownloadHandle>,
}

/// An event pushed to connected clients.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Added { item: Item },
    Updated { item: Item },
    Removed { id: Uuid },
}

pub struct Manager {
    state: Mutex<State>,
    events: broadcast::Sender<Event>,
    /// Raised whenever slot occupancy might have changed.
    ///
    /// A long-lived scheduler task waits on this and does the dispatching, rather than having
    /// a finishing download call back into the dispatcher: `pump` spawning a task that calls
    /// `pump` again is mutual async recursion, which has no finite future type and so cannot
    /// be proved `Send` for `tokio::spawn`.
    wake: Notify,
}

struct State {
    entries: HashMap<Uuid, Entry>,
    /// Display and dequeue order.
    order: Vec<Uuid>,
    config: AppConfig,
}

impl Manager {
    /// Build a manager and start its scheduler.
    ///
    /// Must be called from within a Tokio runtime, since the scheduler runs as a task.
    pub fn new(config: AppConfig) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        let me = Arc::new(Self {
            state: Mutex::new(State { entries: HashMap::new(), order: Vec::new(), config }),
            events,
            wake: Notify::new(),
        });
        let scheduler = me.clone();
        tokio::spawn(async move {
            loop {
                scheduler.wake.notified().await;
                scheduler.pump().await;
            }
        });
        me
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub async fn config(&self) -> AppConfig {
        self.state.lock().await.config.clone()
    }

    /// Replace the config and persist it. Running downloads keep their current settings.
    pub async fn set_config(self: &Arc<Self>, config: AppConfig) -> anyhow::Result<()> {
        config.save()?;
        let mut state = self.state.lock().await;
        state.config = config;
        drop(state);
        self.wake.notify_one();
        Ok(())
    }

    pub async fn list(&self) -> Vec<Item> {
        let state = self.state.lock().await;
        state.order.iter().filter_map(|id| state.entries.get(id)).map(|e| e.item.clone()).collect()
    }

    pub async fn get(&self, id: Uuid) -> Option<Item> {
        self.state.lock().await.entries.get(&id).map(|e| e.item.clone())
    }

    /// Queue a download. It starts as soon as a slot is free.
    pub async fn add(
        self: &Arc<Self>,
        spec: RequestSpec,
        filename: Option<String>,
        source: &str,
    ) -> Item {
        let id = Uuid::new_v4();
        // Until the probe runs, the best name available is the one in the URL.
        let provisional = filename.clone().unwrap_or_else(|| {
            freedm_core::probe::resolve_filename(None, &spec.url, None)
        });
        let item = Item {
            id,
            url: spec.url.clone(),
            filename: provisional,
            path: None,
            status: Status::Queued,
            progress: Progress::default(),
            error: None,
            added_at: chrono::Utc::now().to_rfc3339(),
            source: source.to_string(),
        };

        {
            let mut state = self.state.lock().await;
            state.entries.insert(
                id,
                Entry { item: item.clone(), spec, filename_override: filename, handle: None },
            );
            state.order.push(id);
        }
        let _ = self.events.send(Event::Added { item: item.clone() });
        // Dispatch inline as well as notifying, so a caller observing the list straight after
        // adding sees the download already picked up when a slot was free.
        self.pump().await;
        item
    }

    pub async fn pause(self: &Arc<Self>, id: Uuid) -> bool {
        let mut state = self.state.lock().await;
        let Some(entry) = state.entries.get_mut(&id) else { return false };
        match entry.item.status {
            Status::Running | Status::Starting => {
                if let Some(h) = &entry.handle {
                    h.pause();
                }
                entry.item.status = Status::Paused;
                let item = entry.item.clone();
                drop(state);
                let _ = self.events.send(Event::Updated { item });
                // Pausing frees a slot for whatever is queued behind it.
                self.wake.notify_one();
                true
            }
            // A queued download is already not running; pausing it keeps it out of the pump.
            Status::Queued => {
                entry.item.status = Status::Paused;
                let item = entry.item.clone();
                drop(state);
                let _ = self.events.send(Event::Updated { item });
                true
            }
            _ => false,
        }
    }

    pub async fn resume(self: &Arc<Self>, id: Uuid) -> bool {
        let mut state = self.state.lock().await;
        let Some(entry) = state.entries.get_mut(&id) else { return false };
        if !matches!(entry.item.status, Status::Paused | Status::Failed) {
            return false;
        }
        // A paused download that still holds a live handle can simply carry on; one whose task
        // has ended is re-queued and will resume from its state file.
        if let Some(h) = &entry.handle {
            h.resume();
            entry.item.status = Status::Running;
        } else {
            entry.item.status = Status::Queued;
            entry.item.error = None;
        }
        let item = entry.item.clone();
        drop(state);
        let _ = self.events.send(Event::Updated { item });
        self.wake.notify_one();
        true
    }

    pub async fn cancel(self: &Arc<Self>, id: Uuid) -> bool {
        let mut state = self.state.lock().await;
        let Some(entry) = state.entries.get_mut(&id) else { return false };
        if let Some(h) = &entry.handle {
            h.cancel();
        }
        entry.item.status = Status::Cancelled;
        let item = entry.item.clone();
        drop(state);
        let _ = self.events.send(Event::Updated { item });
        self.wake.notify_one();
        true
    }

    /// Forget a download. Its partial file is left alone unless `delete_file`.
    pub async fn remove(self: &Arc<Self>, id: Uuid, delete_file: bool) -> bool {
        let mut state = self.state.lock().await;
        let Some(entry) = state.entries.remove(&id) else { return false };
        state.order.retain(|x| *x != id);
        drop(state);

        if let Some(h) = &entry.handle {
            h.cancel();
        }
        if delete_file {
            if let Some(path) = &entry.item.path {
                let paths = freedm_core::storage::DownloadPaths::new(path);
                let _ = paths.discard().await;
                let _ = tokio::fs::remove_file(path).await;
            }
        }
        let _ = self.events.send(Event::Removed { id });
        self.wake.notify_one();
        true
    }

    /// Drop every finished, failed or cancelled entry from the list.
    pub async fn clear_finished(self: &Arc<Self>) -> usize {
        let ids: Vec<Uuid> = {
            let state = self.state.lock().await;
            state
                .order
                .iter()
                .filter(|id| {
                    state.entries.get(*id).is_some_and(|e| e.item.status.is_terminal())
                })
                .copied()
                .collect()
        };
        for id in &ids {
            self.remove(*id, false).await;
        }
        ids.len()
    }

    pub async fn pause_all(self: &Arc<Self>) {
        for item in self.list().await {
            if item.status.occupies_a_slot() || item.status == Status::Queued {
                self.pause(item.id).await;
            }
        }
    }

    pub async fn resume_all(self: &Arc<Self>) {
        for item in self.list().await {
            if matches!(item.status, Status::Paused | Status::Failed) {
                self.resume(item.id).await;
            }
        }
    }

    /// Start queued downloads until the concurrency limit is reached.
    async fn pump(self: &Arc<Self>) {
        loop {
            let next = {
                let state = self.state.lock().await;
                let running = state
                    .entries
                    .values()
                    .filter(|e| e.item.status.occupies_a_slot())
                    .count();
                if running >= state.config.max_concurrent_downloads.max(1) {
                    return;
                }
                // Oldest queued first, so the list behaves like a queue.
                state
                    .order
                    .iter()
                    .find(|id| {
                        state.entries.get(*id).is_some_and(|e| e.item.status == Status::Queued)
                    })
                    .copied()
            };
            let Some(id) = next else { return };

            let (spec, filename, config) = {
                let mut state = self.state.lock().await;
                let Some(entry) = state.entries.get_mut(&id) else { continue };
                // Claim the slot while the lock is held, or two pumps could both take it.
                if entry.item.status != Status::Queued {
                    continue;
                }
                entry.item.status = Status::Starting;
                (entry.spec.clone(), entry.filename_override.clone(), state.config.clone())
            };

            if let Some(item) = self.get(id).await {
                let _ = self.events.send(Event::Updated { item });
            }

            let me = self.clone();
            tokio::spawn(async move { me.run_one(id, spec, filename, config).await });
        }
    }

    async fn run_one(
        self: Arc<Self>,
        id: Uuid,
        spec: RequestSpec,
        filename: Option<String>,
        config: AppConfig,
    ) {
        // The destination depends on the filename, which only the probe knows. Prepare into
        // the root first, then let the engine settle the final path.
        let dest = config.download_dir.clone();

        let prepared = Download::prepare(
            spec,
            &dest,
            filename.clone(),
            config.engine_config(),
        )
        .await;

        let download = match prepared {
            Ok(d) => d,
            Err(e) => {
                self.finish(id, Status::Failed, None, Some(e.to_string())).await;
                return;
            }
        };

        // With the real filename known, move the destination into its category folder.
        let info = download.info().clone();
        let name = filename.unwrap_or_else(|| info.filename.clone());
        let download = if config.categorize {
            let folder = config.categories.folder_for(&config.download_dir, &name);
            if folder != dest {
                match Download::prepare(
                    freedm_core::RequestSpec::new(&info.final_url),
                    &folder,
                    Some(name.clone()),
                    config.engine_config(),
                )
                .await
                {
                    Ok(d) => d,
                    // If the category folder cannot be used, carry on in the root rather than
                    // failing the download over a filing decision.
                    Err(e) => {
                        tracing::warn!(error = %e, "could not use the category folder");
                        download
                    }
                }
            } else {
                download
            }
        } else {
            download
        };

        let handle = download.handle();
        let path = download.output_path().to_path_buf();

        {
            let mut state = self.state.lock().await;
            if let Some(entry) = state.entries.get_mut(&id) {
                // A cancel that arrived during the probe must still take effect.
                if entry.item.status == Status::Cancelled {
                    handle.cancel();
                }
                entry.item.filename =
                    path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or(name);
                entry.item.path = Some(path.clone());
                entry.item.progress.total = info.total;
                if entry.item.status == Status::Starting {
                    entry.item.status = Status::Running;
                }
                entry.handle = Some(handle.clone());
                let item = entry.item.clone();
                drop(state);
                let _ = self.events.send(Event::Updated { item });
            }
        }

        // Mirror engine progress into the list while the transfer runs.
        let watcher = {
            let me = self.clone();
            let mut rx = handle.progress.clone();
            tokio::spawn(async move {
                while rx.changed().await.is_ok() {
                    let progress = rx.borrow_and_update().clone();
                    let mut state = me.state.lock().await;
                    let Some(entry) = state.entries.get_mut(&id) else { break };
                    entry.item.progress = progress;
                    let item = entry.item.clone();
                    drop(state);
                    let _ = me.events.send(Event::Updated { item });
                }
            })
        };

        let result = download.run().await;
        watcher.abort();

        match result {
            Ok(final_path) => {
                self.finish(id, Status::Done, Some(final_path), None).await;
            }
            Err(freedm_core::Error::Cancelled) => {
                self.finish(id, Status::Cancelled, None, None).await;
            }
            Err(e) => {
                // A paused download stops with bytes outstanding, which is not a failure.
                let paused = self
                    .get(id)
                    .await
                    .is_some_and(|i| i.status == Status::Paused);
                if paused {
                    // Drop the handle so a later resume re-queues from the state file.
                    let mut state = self.state.lock().await;
                    if let Some(entry) = state.entries.get_mut(&id) {
                        entry.handle = None;
                    }
                } else {
                    self.finish(id, Status::Failed, None, Some(e.to_string())).await;
                }
            }
        }
        // A slot just freed up; the scheduler picks up whatever is queued.
        self.wake.notify_one();
    }

    async fn finish(
        self: &Arc<Self>,
        id: Uuid,
        status: Status,
        path: Option<PathBuf>,
        error: Option<String>,
    ) {
        let mut state = self.state.lock().await;
        let Some(entry) = state.entries.get_mut(&id) else { return };
        entry.item.status = status;
        entry.item.error = error;
        entry.handle = None;
        if let Some(p) = path {
            entry.item.path = Some(p);
        }
        if status == Status::Done {
            entry.item.progress.done = true;
            if let Some(total) = entry.item.progress.total {
                entry.item.progress.downloaded = total;
            }
            entry.item.progress.speed = 0;
            entry.item.progress.connections = 0;
        }
        let item = entry.item.clone();
        drop(state);
        let _ = self.events.send(Event::Updated { item });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> AppConfig {
        AppConfig {
            download_dir: std::env::temp_dir().join("freedm-mgr-test"),
            max_concurrent_downloads: 2,
            categorize: false,
            ..Default::default()
        }
    }

    /// A URL that will fail fast without touching the network meaningfully.
    fn spec(n: usize) -> RequestSpec {
        RequestSpec::new(format!("http://127.0.0.1:1/never/file{n}.bin"))
    }

    #[tokio::test]
    async fn added_downloads_appear_in_order() {
        let m = Manager::new(test_config());
        let a = m.add(spec(1), None, "api").await;
        let b = m.add(spec(2), None, "api").await;
        let list = m.list().await;
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, a.id);
        assert_eq!(list[1].id, b.id);
        assert_eq!(list[0].filename, "file1.bin", "the url gives a provisional name");
    }

    #[tokio::test]
    async fn only_the_configured_number_of_downloads_hold_slots() {
        let mut cfg = test_config();
        cfg.max_concurrent_downloads = 2;
        let m = Manager::new(cfg);
        for i in 0..5 {
            m.add(spec(i), None, "api").await;
        }
        let list = m.list().await;
        let holding = list.iter().filter(|i| i.status.occupies_a_slot()).count();
        let queued = list.iter().filter(|i| i.status == Status::Queued).count();
        // The rest must wait rather than all starting at once.
        assert!(holding <= 2, "{holding} downloads holding slots");
        assert_eq!(holding + queued, 5);
    }

    #[tokio::test]
    async fn pausing_a_queued_download_keeps_it_from_starting() {
        let mut cfg = test_config();
        cfg.max_concurrent_downloads = 1;
        let m = Manager::new(cfg);
        m.add(spec(1), None, "api").await;
        let second = m.add(spec(2), None, "api").await;

        assert!(m.pause(second.id).await);
        assert_eq!(m.get(second.id).await.unwrap().status, Status::Paused);
        // Even with a slot free, a paused entry is not picked up by the pump.
        m.cancel(m.list().await[0].id).await;
        assert_eq!(m.get(second.id).await.unwrap().status, Status::Paused);
    }

    #[tokio::test]
    async fn resuming_a_paused_queue_entry_puts_it_back_in_line() {
        let m = Manager::new(test_config());
        let item = m.add(spec(1), None, "api").await;
        m.pause(item.id).await;
        assert!(m.resume(item.id).await);
        let status = m.get(item.id).await.unwrap().status;
        assert_ne!(status, Status::Paused);
    }

    #[tokio::test]
    async fn cancelling_marks_the_entry_and_frees_the_slot() {
        let m = Manager::new(test_config());
        let item = m.add(spec(1), None, "api").await;
        assert!(m.cancel(item.id).await);
        assert_eq!(m.get(item.id).await.unwrap().status, Status::Cancelled);
    }

    #[tokio::test]
    async fn removing_takes_the_entry_out_of_the_list() {
        let m = Manager::new(test_config());
        let item = m.add(spec(1), None, "api").await;
        assert!(m.remove(item.id, false).await);
        assert!(m.get(item.id).await.is_none());
        assert!(m.list().await.is_empty());
        // Removing something already gone is a no-op, not a panic.
        assert!(!m.remove(item.id, false).await);
    }

    #[tokio::test]
    async fn clear_finished_keeps_live_downloads() {
        let m = Manager::new(test_config());
        let a = m.add(spec(1), None, "api").await;
        let b = m.add(spec(2), None, "api").await;
        m.cancel(a.id).await;
        m.pause(b.id).await;

        assert_eq!(m.clear_finished().await, 1, "only the cancelled entry is cleared");
        assert!(m.get(a.id).await.is_none());
        assert!(m.get(b.id).await.is_some(), "a paused download is not finished");
    }

    #[tokio::test]
    async fn operations_on_an_unknown_id_are_refused_rather_than_panicking() {
        let m = Manager::new(test_config());
        let ghost = Uuid::new_v4();
        assert!(!m.pause(ghost).await);
        assert!(!m.resume(ghost).await);
        assert!(!m.cancel(ghost).await);
        assert!(!m.remove(ghost, false).await);
    }

    #[tokio::test]
    async fn subscribers_are_told_when_a_download_is_added() {
        let m = Manager::new(test_config());
        let mut rx = m.subscribe();
        let item = m.add(spec(1), None, "browser").await;

        let event = rx.recv().await.unwrap();
        match event {
            Event::Added { item: got } => {
                assert_eq!(got.id, item.id);
                assert_eq!(got.source, "browser");
            }
            other => panic!("expected an Added event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_failing_download_records_why() {
        let m = Manager::new(test_config());
        // Port 1 refuses connections, so this fails quickly and without retries mattering.
        let item = m.add(spec(1), None, "api").await;

        // Wait for the entry to reach a terminal state.
        for _ in 0..100 {
            let got = m.get(item.id).await.unwrap();
            if got.status == Status::Failed {
                assert!(got.error.is_some(), "a failure must say what went wrong");
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("the download never reached a terminal state");
    }
}
