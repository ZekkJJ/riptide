//! Download orchestration: a pool of connections cooperating over one [`ChunkMap`].

use crate::chunks::{ChunkMap, DEFAULT_MIN_SPLIT};
use crate::error::{Error, Result};
use crate::probe::{self, RequestSpec, ResourceInfo};
use crate::state::ResumeState;
use crate::storage::{DownloadPaths, SegmentWriter, SparseFile};
use crate::throttle::{SpeedMeter, Throttle};
use futures_util::StreamExt;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, Mutex, Notify};

/// How often progress is published and resume state written out.
const TICK: Duration = Duration::from_millis(500);

/// Largest single read taken from a socket before checking the throttle and the stop flag.
const READ_QUANTUM: u64 = 64 * 1024;

/// How long without a single byte before a running download is reported as stalled.
const STALL_AFTER: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadConfig {
    /// Upper bound on simultaneous connections for this download.
    pub max_connections: usize,
    /// Smallest range worth a connection of its own.
    pub min_split: u64,
    /// Attempts per segment before the segment is abandoned.
    pub max_retries: u32,
    /// Idle read timeout for a single connection.
    pub timeout: Duration,
    pub proxy: Option<String>,
    /// Bytes per second for this download, 0 for unlimited.
    pub speed_limit: u64,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            // Eight is the usual sweet spot: enough to saturate a link whose per-connection
            // throughput is capped, while staying under the per-client connection limits that
            // many servers enforce (and that get clients rate-limited or blocked when
            // exceeded).
            max_connections: 8,
            min_split: DEFAULT_MIN_SPLIT,
            max_retries: 5,
            timeout: Duration::from_secs(30),
            proxy: None,
            speed_limit: 0,
        }
    }
}

/// A snapshot of a download's progress, published on a watch channel.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Progress {
    pub downloaded: u64,
    pub total: Option<u64>,
    /// Bytes per second over the recent window.
    pub speed: u64,
    /// Connections currently transferring.
    pub connections: usize,
    pub segments: usize,
    pub eta_secs: Option<u64>,
    pub done: bool,
    /// No bytes have arrived for a while although the download is active.
    ///
    /// Connections sit in connect or read timeouts and then back off before retrying, so a
    /// server that has stopped answering leaves the transfer looking busy for a minute or
    /// more. Saying so plainly is better than showing a confident "8 connections" next to a
    /// progress bar that is not moving.
    pub stalled: bool,
}

impl Progress {
    pub fn percent(&self) -> Option<f64> {
        let total = self.total?;
        if total == 0 {
            return Some(100.0);
        }
        Some(self.downloaded as f64 / total as f64 * 100.0)
    }
}

/// Run state, driven by the controller and observed by every worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunState {
    Running,
    Paused,
    Cancelled,
}

/// Handle for controlling and observing a download in flight.
#[derive(Clone)]
pub struct DownloadHandle {
    state: watch::Sender<RunState>,
    pub progress: watch::Receiver<Progress>,
}

impl DownloadHandle {
    pub fn pause(&self) {
        let _ = self.state.send(RunState::Paused);
    }
    pub fn resume(&self) {
        let _ = self.state.send(RunState::Running);
    }
    pub fn cancel(&self) {
        let _ = self.state.send(RunState::Cancelled);
    }
    pub fn snapshot(&self) -> Progress {
        self.progress.borrow().clone()
    }
}

/// Rotating set of equivalent URLs for one file.
///
/// Each mirror carries a failure count; the pool always offers the URL that has failed least,
/// so a dead mirror drops out of rotation on its own without being permanently blacklisted.
struct UrlPool {
    urls: Vec<(String, AtomicU64)>,
}

impl UrlPool {
    fn new(primary: String, mirrors: &[String]) -> Self {
        let mut urls = vec![(primary, AtomicU64::new(0))];
        for m in mirrors {
            if !urls.iter().any(|(u, _)| u == m) {
                urls.push((m.clone(), AtomicU64::new(0)));
            }
        }
        Self { urls }
    }

    fn best(&self) -> &str {
        self.urls
            .iter()
            .min_by_key(|(_, fails)| fails.load(Ordering::Relaxed))
            .map(|(u, _)| u.as_str())
            .unwrap_or_default()
    }

    fn penalise(&self, url: &str) {
        if let Some((_, fails)) = self.urls.iter().find(|(u, _)| u == url) {
            fails.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Everything the workers share.
struct Shared {
    client: Client,
    spec: RequestSpec,
    urls: UrlPool,
    map: Mutex<ChunkMap>,
    file: SparseFile,
    throttle: Throttle,
    /// How a worker learns that work may have appeared, so idle workers wait instead of spin.
    work: Notify,
    state: watch::Receiver<RunState>,
    config: DownloadConfig,
    /// What the probe established, carried so periodic saves record the validators a later
    /// resume needs in order to tell whether the remote file changed.
    info: ResourceInfo,
    state_path: PathBuf,
    /// Bytes arrived since the last progress tick, for the speed meter.
    received: AtomicU64,
}

impl Shared {
    fn run_state(&self) -> RunState {
        *self.state.borrow()
    }

    /// Persist the chunk tiling and validators beside the payload.
    async fn save_state(&self, map: &ChunkMap) {
        let mut state = ResumeState::new(&self.info, map, &self.config);
        state.mirrors = self.spec.mirrors.clone();
        if let Err(e) = state.save(&self.state_path).await {
            tracing::warn!(error = %e, "could not write resume state");
        }
    }
}

/// A download, from probe through to a finished file.
pub struct Download {
    shared: Arc<Shared>,
    paths: DownloadPaths,
    info: ResourceInfo,
    progress_tx: watch::Sender<Progress>,
    state_tx: watch::Sender<RunState>,
}

impl Download {
    /// Probe the resource, prepare the output file and restore any previous progress.
    ///
    /// `dest_dir` is where the file lands; the name comes from the server unless `filename`
    /// overrides it.
    pub async fn prepare(
        spec: RequestSpec,
        dest_dir: impl AsRef<Path>,
        filename: Option<String>,
        config: DownloadConfig,
    ) -> Result<Self> {
        let client = probe::client(config.proxy.as_deref(), config.timeout)?;
        let info = probe_with_mirrors(&client, &spec, config.max_retries).await?;

        let name = filename
            .map(|n| probe::sanitize_filename(&n))
            .unwrap_or_else(|| info.filename.clone());
        let desired = dest_dir.as_ref().join(&name);

        // An existing state file means a partial download under this exact name; adopt it
        // rather than picking a fresh name beside it.
        let existing = ResumeState::load(&DownloadPaths::new(&desired).state_path).await;
        let (paths, map) = match existing {
            Some(saved) => match saved.reusable(&info) {
                Ok(()) => {
                    tracing::info!(
                        file = %desired.display(),
                        done = saved.map.downloaded(),
                        "resuming an earlier download"
                    );
                    let mut map = saved.map;
                    // Nothing is owned until a worker claims it in this process.
                    map.release_all();
                    (DownloadPaths::new(&desired), map)
                }
                Err(reason) => {
                    // The remote file is not the one on disk. Start again under a free name
                    // rather than splicing new bytes into stale ones.
                    tracing::warn!(%reason, "cannot reuse partial download, starting fresh");
                    let fresh = crate::storage::unique_path(&desired);
                    (DownloadPaths::new(fresh), new_map(&info, &config))
                }
            },
            None => {
                let target = crate::storage::unique_path(&desired);
                (DownloadPaths::new(target), new_map(&info, &config))
            }
        };

        let file = SparseFile::open(&paths.part_path, info.total)
            .map_err(|e| Error::io(&paths.part_path, e))?;

        let (state_tx, state_rx) = watch::channel(RunState::Running);
        let (progress_tx, _) = watch::channel(Progress {
            downloaded: map.downloaded(),
            total: info.total,
            segments: map.chunks().len(),
            ..Default::default()
        });

        let shared = Arc::new(Shared {
            client,
            urls: UrlPool::new(info.final_url.clone(), &spec.mirrors),
            spec,
            map: Mutex::new(map),
            file,
            throttle: Throttle::new(config.speed_limit),
            work: Notify::new(),
            state: state_rx,
            config,
            info: info.clone(),
            state_path: paths.state_path.clone(),
            received: AtomicU64::new(0),
        });

        Ok(Self { shared, paths, info, progress_tx, state_tx })
    }

    pub fn info(&self) -> &ResourceInfo {
        &self.info
    }

    pub fn output_path(&self) -> &Path {
        &self.paths.final_path
    }

    pub fn handle(&self) -> DownloadHandle {
        DownloadHandle { state: self.state_tx.clone(), progress: self.progress_tx.subscribe() }
    }

    /// Transfer the file and return where it landed.
    pub async fn run(self) -> Result<PathBuf> {
        let Self { shared, paths, info, progress_tx, state_tx: _state_tx } = self;

        // A server that will not serve ranges, or will not state a length, gets exactly one
        // connection: splitting would either corrupt the file or be impossible to place.
        let workers = if info.is_segmentable() {
            shared.config.max_connections.max(1)
        } else {
            if shared.config.max_connections > 1 {
                tracing::info!(
                    ranges = info.supports_ranges,
                    sized = info.total.is_some(),
                    "server does not support segmented transfer, using a single connection"
                );
            }
            1
        };

        let mut tasks = Vec::with_capacity(workers);
        for id in 0..workers {
            let shared = shared.clone();
            tasks.push(tokio::spawn(async move { worker(shared, id).await }));
        }

        let reporter = tokio::spawn(report(shared.clone(), progress_tx.clone(), info.total));

        // Collect worker outcomes. A single failure does not sink the download as long as the
        // bytes it was responsible for get picked up by someone else, so the error is only
        // surfaced if the file ends up incomplete.
        let mut first_error = None;
        for t in tasks {
            match t.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                }
                Err(join) => {
                    if first_error.is_none() {
                        first_error = Some(Error::other(format!("worker panicked: {join}")));
                    }
                }
            }
        }
        reporter.abort();

        if shared.run_state() == RunState::Cancelled {
            paths.discard().await.map_err(|e| Error::io(&paths.part_path, e))?;
            return Err(Error::Cancelled);
        }

        let mut map = shared.map.lock().await;

        // An unsized stream is complete when its single connection ends cleanly.
        if !map.is_sized() && first_error.is_none() {
            let received = map.downloaded();
            map.finalize_unsized(received);
            shared
                .file
                .truncate(received)
                .await
                .map_err(|e| Error::io(shared.file.path(), e))?;
        }

        if !map.is_complete() {
            // Keep the partial payload and its state so the user can retry later.
            map.coalesce();
            shared.save_state(&map).await;
            drop(map);
            shared.file.sync().await.ok();
            return Err(first_error.unwrap_or_else(|| {
                Error::other("transfer ended with bytes still outstanding")
            }));
        }

        map.coalesce();
        drop(map);

        // A server that over-reported its length leaves tail bytes that were never written.
        if let Some(total) = info.total {
            if shared.file.size().map_err(|e| Error::io(shared.file.path(), e))? > total {
                shared.file.truncate(total).await.map_err(|e| Error::io(shared.file.path(), e))?;
            }
        }

        shared.file.sync().await.map_err(|e| Error::io(shared.file.path(), e))?;
        paths.finish().await.map_err(|e| Error::io(&paths.final_path, e))?;

        let final_progress = Progress {
            downloaded: info.total.unwrap_or_default(),
            total: info.total,
            speed: 0,
            connections: 0,
            segments: 1,
            eta_secs: Some(0),
            done: true,
            stalled: false,
        };
        let _ = progress_tx.send(final_progress);

        Ok(paths.final_path)
    }
}

fn new_map(info: &ResourceInfo, config: &DownloadConfig) -> ChunkMap {
    match info.total {
        Some(total) if info.supports_ranges => ChunkMap::sized(total).with_min_split(config.min_split),
        Some(total) => ChunkMap::sized(total).with_min_split(u64::MAX), // sized but unsplittable
        None => ChunkMap::unsized_stream(),
    }
}

/// Probe the primary URL, falling back to mirrors when it will not answer.
///
/// The whole sequence is retried for transient faults. Without this a single connection blip
/// at probe time kills the download outright, which is a worse outcome than anything that can
/// happen later: once transferring, every segment already retries, so the probe was the one
/// step with no second chance.
async fn probe_with_mirrors(
    client: &Client,
    spec: &RequestSpec,
    max_retries: u32,
) -> Result<ResourceInfo> {
    let mut attempt = 0;
    loop {
        let mut last = match probe::probe(client, spec).await {
            Ok(info) => return Ok(info),
            Err(e) => e,
        };
        for mirror in &spec.mirrors {
            let alt = RequestSpec { url: mirror.clone(), ..spec.clone() };
            match probe::probe(client, &alt).await {
                Ok(info) => {
                    tracing::info!(%mirror, "primary url failed, using mirror");
                    return Ok(info);
                }
                Err(e) => last = e,
            }
        }

        // An expired link, a 404 or a changed file will not fix itself by asking again.
        if !last.is_retryable() || attempt >= max_retries {
            return Err(last);
        }
        attempt += 1;
        let delay = Duration::from_millis(500 * (1u64 << attempt.min(5)))
            .min(Duration::from_secs(15));
        tracing::debug!(attempt, ?delay, error = %last, "probe failed, retrying");
        tokio::time::sleep(delay).await;
    }
}

/// One connection slot: claim work, transfer it, repeat.
async fn worker(shared: Arc<Shared>, worker_id: usize) -> Result<()> {
    loop {
        match shared.run_state() {
            RunState::Cancelled => return Ok(()),
            RunState::Paused => {
                // Wait for the controller rather than holding a connection open.
                let mut state = shared.state.clone();
                loop {
                    if state.changed().await.is_err() {
                        return Ok(());
                    }
                    if *state.borrow() != RunState::Paused {
                        break;
                    }
                }
                continue;
            }
            RunState::Running => {}
        }

        // Take an unfinished chunk if one is going, otherwise steal the tail of the chunk
        // with the most work left.
        let chunk = {
            let mut map = shared.map.lock().await;
            match map.claim_idle() {
                Some(c) => Some(c),
                None => map.split_largest().map(|split| {
                    map.mark_active(split.created.id, true);
                    split.created
                }),
            }
        };

        let Some(chunk) = chunk else {
            // Nothing claimable. The download may still be finishing in other workers, and a
            // worker that errors out will release its chunk, so wait to be told rather than
            // exiting and leaving those bytes unclaimed.
            if shared.map.lock().await.is_complete() {
                return Ok(());
            }
            tokio::select! {
                _ = shared.work.notified() => continue,
                _ = tokio::time::sleep(Duration::from_millis(200)) => {
                    let map = shared.map.lock().await;
                    if map.is_complete() || map.active_count() == 0 {
                        return Ok(());
                    }
                    continue;
                }
            }
        };

        let result = transfer(&shared, chunk.id, worker_id).await;

        {
            let mut map = shared.map.lock().await;
            map.mark_active(chunk.id, false);
        }
        // Whether this chunk finished or failed, a slot just freed up.
        shared.work.notify_waiters();

        if let Err(e) = result {
            if matches!(e, Error::Cancelled) {
                return Ok(());
            }
            return Err(e);
        }
    }
}

/// Fetch one chunk, with retries and mirror rotation.
async fn transfer(shared: &Arc<Shared>, chunk_id: u64, worker_id: usize) -> Result<()> {
    let mut attempt = 0u32;
    loop {
        let Some(bound) = shared.map.lock().await.bound(chunk_id) else { return Ok(()) };
        if bound.is_done() {
            return Ok(());
        }

        let url = shared.urls.best().to_string();
        match transfer_once(shared, chunk_id, &url, worker_id).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                if matches!(e, Error::Cancelled) {
                    return Err(e);
                }
                // A chunk that has been fully handed away is not a failure.
                if shared.map.lock().await.bound(chunk_id).is_some_and(|b| b.is_done()) {
                    return Ok(());
                }
                if !e.is_retryable() || attempt >= shared.config.max_retries {
                    return Err(e);
                }
                shared.urls.penalise(&url);
                attempt += 1;
                // Exponential backoff, capped, with a little spread so that several segments
                // failing at once do not all come back at the same instant.
                let base = Duration::from_millis(400 * (1u64 << attempt.min(6)));
                let jitter = Duration::from_millis((chunk_id * 37 + attempt as u64 * 13) % 400);
                let delay = (base + jitter).min(Duration::from_secs(30));
                tracing::debug!(
                    chunk = chunk_id,
                    attempt,
                    ?delay,
                    error = %e,
                    "segment failed, retrying"
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// A single attempt at the outstanding part of a chunk.
async fn transfer_once(
    shared: &Arc<Shared>,
    chunk_id: u64,
    url: &str,
    worker_id: usize,
) -> Result<()> {
    let Some(bound) = shared.map.lock().await.bound(chunk_id) else { return Ok(()) };
    if bound.is_done() {
        return Ok(());
    }

    let sized = shared.map.lock().await.is_sized();
    // Ask only for what is left of this chunk. The last byte is inclusive in HTTP.
    let range = if sized { Some((bound.cursor, Some(bound.end - 1))) } else { None };

    let resp = shared
        .client
        .get(url)
        .headers(shared.spec.headers(range))
        .send()
        .await?;

    let status = resp.status();
    if status == StatusCode::OK && bound.cursor > 0 {
        // The server ignored Range and is sending the file from byte zero. Writing that at
        // our offset would corrupt the output, so refuse rather than produce a broken file.
        return Err(Error::ResumeUnsupported);
    }
    if !status.is_success() {
        return Err(Error::BadStatus { status: status.as_u16(), url: url.to_string() });
    }

    tracing::debug!(
        worker = worker_id,
        chunk = chunk_id,
        start = bound.cursor,
        end = bound.end,
        "segment started"
    );

    let mut writer = SegmentWriter::new(shared.file.clone(), bound.cursor);
    let mut stream = resp.bytes_stream();

    while let Some(piece) = stream.next().await {
        let piece = piece?;
        let mut data = &piece[..];

        while !data.is_empty() {
            match shared.run_state() {
                RunState::Cancelled => return Err(Error::Cancelled),
                RunState::Paused => {
                    // Commit what is in hand so a pause is a safe stopping point, then let the
                    // worker loop handle waiting.
                    let n = writer.flush().await.map_err(|e| Error::io(shared.file.path(), e))?;
                    commit(shared, chunk_id, n).await;
                    return Ok(());
                }
                RunState::Running => {}
            }

            let allowed = shared.throttle.acquire(READ_QUANTUM.min(data.len() as u64)).await;
            let take = (allowed as usize).min(data.len());
            let n = writer
                .push(&data[..take])
                .await
                .map_err(|e| Error::io(shared.file.path(), e))?;
            data = &data[take..];

            // Speed is measured on arrival rather than on flush, so the reading tracks the
            // link instead of stepping with the write buffer.
            shared.received.fetch_add(take as u64, Ordering::Relaxed);

            if n > 0 {
                let bound = commit(shared, chunk_id, n).await;
                // A steal may have moved our end bound behind us while we were reading; the
                // rest of this range now belongs to another connection.
                if bound.is_some_and(|b| b.is_done()) {
                    return Ok(());
                }
            }
        }

        // Stop once the chunk's range is covered, even if the server is still sending (which
        // it will be for an open-ended range request).
        if writer.position() >= bound_end(shared, chunk_id).await {
            break;
        }
    }

    let n = writer.flush().await.map_err(|e| Error::io(shared.file.path(), e))?;
    commit(shared, chunk_id, n).await;

    let Some(final_bound) = shared.map.lock().await.bound(chunk_id) else { return Ok(()) };
    if sized && !final_bound.is_done() {
        // The connection ended before the range was covered. Retryable: the next attempt
        // resumes from the new cursor.
        return Err(Error::PlainIo(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("connection closed with {} bytes outstanding", final_bound.remaining()),
        )));
    }
    Ok(())
}

/// Record written bytes against a chunk and against the speed meter.
async fn commit(shared: &Arc<Shared>, chunk_id: u64, n: u64) -> Option<crate::chunks::ChunkBound> {
    if n == 0 {
        return shared.map.lock().await.bound(chunk_id);
    }
    shared.map.lock().await.advance(chunk_id, n)
}

async fn bound_end(shared: &Arc<Shared>, chunk_id: u64) -> u64 {
    shared.map.lock().await.bound(chunk_id).map(|b| b.end).unwrap_or(u64::MAX)
}

/// Publish progress and persist resume state on a timer.
async fn report(shared: Arc<Shared>, tx: watch::Sender<Progress>, total: Option<u64>) {
    let mut meter = SpeedMeter::new();
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_saved = 0u64;
    let mut last_save_at = tokio::time::Instant::now();
    let mut last_progress = 0u64;
    let mut last_progress_at = tokio::time::Instant::now();

    loop {
        ticker.tick().await;
        meter.record(shared.received.swap(0, Ordering::Relaxed));

        let (downloaded, segments, connections, snapshot) = {
            let map = shared.map.lock().await;
            (map.downloaded(), map.chunks().len(), map.active_count(), map.clone())
        };

        if downloaded > last_progress {
            last_progress = downloaded;
            last_progress_at = tokio::time::Instant::now();
        }

        let remaining = total.map(|t| t.saturating_sub(downloaded));
        let _ = tx.send(Progress {
            downloaded,
            total,
            speed: meter.rate(),
            connections,
            segments,
            eta_secs: remaining.and_then(|r| meter.eta(r)).map(|d| d.as_secs()),
            done: false,
            stalled: last_progress_at.elapsed() >= STALL_AFTER,
        });

        // Checkpoint on whichever comes first: a few megabytes of progress, or a couple of
        // seconds. The byte threshold alone would be too coarse on a slow link, where a
        // process killed outright could lose everything since the last multi-megabyte mark;
        // the state file is only a few hundred bytes, so the time-based save is nearly free.
        let advanced = downloaded.saturating_sub(last_saved);
        let stale = last_save_at.elapsed() >= Duration::from_secs(2);
        if advanced > 0 && (advanced > 8 << 20 || stale) {
            last_saved = downloaded;
            last_save_at = tokio::time::Instant::now();
            shared.save_state(&snapshot).await;
        }
    }
}
