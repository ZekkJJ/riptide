//! Output file handling.
//!
//! Every segment writes straight into its final position in one sparse file, so there is no
//! separate join pass once the transfer ends: when the last byte lands the file is already
//! complete and only needs renaming. This is the main structural departure from download
//! managers that give each segment its own temporary file and concatenate them at the end,
//! which costs a second full read plus a second full write of the whole file and is why large
//! downloads in those tools appear to hang at 100%.
//!
//! Writes are positional (`seek_write` / `write_at`), which take `&File` rather than
//! `&mut File`, so every worker can share one handle and write at its own offset with no lock
//! and no shared cursor. Each worker buffers into [`SegmentWriter`] and flushes in large
//! blocks on the blocking pool, keeping syscalls off the async runtime.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// Bytes a [`SegmentWriter`] accumulates before handing a block to the blocking pool.
pub const WRITE_BUFFER: usize = 1 << 20; // 1 MiB

/// Longest a buffer may sit unflushed.
///
/// Flushing only when the buffer fills makes progress advance in whole-megabyte steps per
/// connection, which on a slow link means a frozen-looking progress bar and resume state that
/// is never written at all. Bounding the buffer's age decouples how often progress is
/// accounted from how fast the transfer happens, while leaving large sequential writes intact
/// on fast links.
pub const MAX_BUFFER_AGE: Duration = Duration::from_millis(500);

#[cfg(windows)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_write(buf, offset)
}

#[cfg(unix)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.write_at(buf, offset)
}

/// A shared, randomly-writable output file.
#[derive(Debug, Clone)]
pub struct SparseFile {
    file: Arc<File>,
    path: PathBuf,
}

impl SparseFile {
    /// Open (creating if absent) the partial file for a download, sizing it to `total` when
    /// the length is known so that later writes never extend the file and the filesystem can
    /// lay it out in one go.
    pub fn open(path: impl AsRef<Path>, total: Option<u64>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        // truncate(false) is deliberate and load-bearing: this same call reopens the
        // partial file of a resumed download, and truncating would throw away every byte
        // already fetched.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        if let Some(total) = total {
            // Only ever grow: a resumed download must not truncate bytes already written.
            if file.metadata()?.len() < total {
                file.set_len(total)?;
            }
        }
        Ok(Self { file: Arc::new(file), path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Current length of the file on disk.
    pub fn size(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    /// Write a whole block at `offset`, retrying short writes.
    fn write_all_at(&self, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
        while !buf.is_empty() {
            match write_at(&self.file, buf, offset) {
                Ok(0) => {
                    return Err(io::Error::new(io::ErrorKind::WriteZero, "failed to write whole block"))
                }
                Ok(n) => {
                    buf = &buf[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Flush a block at `offset` without blocking the async runtime.
    pub async fn write_block(&self, buf: Vec<u8>, offset: u64) -> io::Result<()> {
        let me = self.clone();
        tokio::task::spawn_blocking(move || me.write_all_at(&buf, offset))
            .await
            .map_err(io::Error::other)?
    }

    /// Push file data and metadata to disk.
    pub async fn sync(&self) -> io::Result<()> {
        let file = self.file.clone();
        tokio::task::spawn_blocking(move || file.sync_all())
            .await
            .map_err(io::Error::other)?
    }

    /// Cut the file down to its true length, for a stream whose size was unknown until the end
    /// (or a server that over-reported `Content-Length`).
    pub async fn truncate(&self, len: u64) -> io::Result<()> {
        let file = self.file.clone();
        tokio::task::spawn_blocking(move || file.set_len(len))
            .await
            .map_err(io::Error::other)?
    }
}

/// Per-worker write buffer.
///
/// Holds up to [`WRITE_BUFFER`] bytes of contiguous data and remembers the offset the buffer
/// starts at, so a flush is a single positional write. The caller must `flush` before dropping
/// the writer, since a `Drop` impl cannot await.
pub struct SegmentWriter {
    file: SparseFile,
    buf: Vec<u8>,
    /// Offset in the output file that `buf[0]` belongs at.
    base: u64,
    last_flush: Instant,
    max_age: Duration,
}

impl SegmentWriter {
    pub fn new(file: SparseFile, start: u64) -> Self {
        Self {
            file,
            buf: Vec::with_capacity(WRITE_BUFFER),
            base: start,
            last_flush: Instant::now(),
            max_age: MAX_BUFFER_AGE,
        }
    }

    /// Override how long data may sit unflushed. Mainly for tests.
    pub fn with_max_age(mut self, max_age: Duration) -> Self {
        self.max_age = max_age;
        self
    }

    /// Offset just past the last byte accepted, flushed or not.
    pub fn position(&self) -> u64 {
        self.base + self.buf.len() as u64
    }

    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    /// Accept data, flushing when the buffer fills or when it has sat too long. Returns the
    /// number of bytes that reached the disk during this call, which the caller reports to the
    /// chunk map only once it is durable enough to resume from.
    pub async fn push(&mut self, data: &[u8]) -> io::Result<u64> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= WRITE_BUFFER || self.last_flush.elapsed() >= self.max_age {
            self.flush().await
        } else {
            Ok(0)
        }
    }

    /// Write everything buffered and return how many bytes were written.
    pub async fn flush(&mut self) -> io::Result<u64> {
        self.last_flush = Instant::now();
        if self.buf.is_empty() {
            return Ok(0);
        }
        let n = self.buf.len() as u64;
        let block = std::mem::replace(&mut self.buf, Vec::with_capacity(WRITE_BUFFER));
        self.file.write_block(block, self.base).await?;
        self.base += n;
        Ok(n)
    }
}

/// Paths used while a download is in flight.
///
/// The payload lands in `<name>.fdmpart` and the resume state beside it in `<name>.fdmstate`,
/// both in the destination directory, so a move to another drive carries the progress with it.
#[derive(Debug, Clone)]
pub struct DownloadPaths {
    pub final_path: PathBuf,
    pub part_path: PathBuf,
    pub state_path: PathBuf,
}

impl DownloadPaths {
    pub fn new(final_path: impl Into<PathBuf>) -> Self {
        let final_path: PathBuf = final_path.into();
        Self {
            part_path: with_suffix(&final_path, ".fdmpart"),
            state_path: with_suffix(&final_path, ".fdmstate"),
            final_path,
        }
    }

    /// Move the finished payload onto its real name and drop the state file.
    pub async fn finish(&self) -> io::Result<()> {
        let (part, dest, state) =
            (self.part_path.clone(), self.final_path.clone(), self.state_path.clone());
        tokio::task::spawn_blocking(move || {
            std::fs::rename(&part, &dest)?;
            // A leftover state file would make a complete download look resumable.
            let _ = std::fs::remove_file(&state);
            Ok::<_, io::Error>(())
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Delete the partial payload and its state, for a cancelled download.
    pub async fn discard(&self) -> io::Result<()> {
        let (part, state) = (self.part_path.clone(), self.state_path.clone());
        tokio::task::spawn_blocking(move || {
            let _ = std::fs::remove_file(&part);
            let _ = std::fs::remove_file(&state);
        })
        .await
        .map_err(io::Error::other)
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// Pick a name that is not taken yet by inserting ` (1)`, ` (2)`, ... before the extension.
///
/// The partial and state files are checked too, so a new download never adopts the name of one
/// already in flight.
pub fn unique_path(desired: &Path) -> PathBuf {
    let taken = |p: &Path| {
        p.exists() || with_suffix(p, ".fdmpart").exists() || with_suffix(p, ".fdmstate").exists()
    };
    if !taken(desired) {
        return desired.to_path_buf();
    }
    let parent = desired.parent().unwrap_or_else(|| Path::new("."));
    let stem = desired.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let ext = desired.extension().map(|s| s.to_string_lossy().to_string());
    for n in 1..10_000 {
        let name = match &ext {
            Some(ext) => format!("{stem} ({n}).{ext}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = parent.join(name);
        if !taken(&candidate) {
            return candidate;
        }
    }
    desired.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("freedm-test-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn out_of_order_segment_writes_rebuild_the_exact_file() {
        let dir = tmpdir("ooo");
        let path = dir.join("out.bin");
        let total: u64 = 300_000;
        let expected: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();

        let file = SparseFile::open(&path, Some(total)).unwrap();

        // Three segments written in reverse order, each through its own buffered writer, the
        // way three racing connections would.
        let bounds = [(200_000u64, total), (100_000, 200_000), (0, 100_000)];
        for (start, end) in bounds {
            let mut w = SegmentWriter::new(file.clone(), start);
            // Push in awkward 7000-byte pieces so flushes land mid-segment.
            let mut at = start;
            while at < end {
                let n = (7000).min(end - at) as usize;
                w.push(&expected[at as usize..at as usize + n]).await.unwrap();
                at += n as u64;
            }
            w.flush().await.unwrap();
        }
        file.sync().await.unwrap();

        let got = std::fs::read(&path).unwrap();
        assert_eq!(got.len() as u64, total);
        assert_eq!(got, expected, "reassembled file must match byte for byte");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn writer_reports_only_bytes_that_reached_disk() {
        let dir = tmpdir("flush");
        let file = SparseFile::open(dir.join("x.bin"), Some(WRITE_BUFFER as u64 * 2)).unwrap();
        // A long age limit isolates the size-based rule under test.
        let mut w = SegmentWriter::new(file, 0).with_max_age(Duration::from_secs(3600));

        // Small pushes stay in the buffer and report nothing written yet.
        assert_eq!(w.push(&[1u8; 1000]).await.unwrap(), 0);
        assert_eq!(w.buffered(), 1000);
        assert_eq!(w.position(), 1000);

        // Crossing the buffer threshold flushes the whole accumulation.
        let big = vec![2u8; WRITE_BUFFER];
        let written = w.push(&big).await.unwrap();
        assert_eq!(written, 1000 + WRITE_BUFFER as u64);
        assert_eq!(w.buffered(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_trickle_still_reports_progress() {
        // Regression test. Flushing only on a full buffer meant that a connection slower than
        // one buffer per progress tick reported zero bytes transferred indefinitely: the
        // progress bar sat still and, because resume state is only written when progress
        // advances, no resume state was ever saved at all.
        let dir = tmpdir("trickle");
        let file = SparseFile::open(dir.join("slow.bin"), Some(WRITE_BUFFER as u64)).unwrap();
        let mut w = SegmentWriter::new(file, 0).with_max_age(Duration::from_millis(500));

        // A few kilobytes, far short of the 1 MiB buffer.
        assert_eq!(w.push(&[1u8; 4096]).await.unwrap(), 0, "buffered, nothing on disk yet");

        // Once the buffer has sat longer than its age limit, the next push commits it even
        // though the buffer is nowhere near full.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let written = w.push(&[1u8; 128]).await.unwrap();
        assert_eq!(written, 4096 + 128, "a stale buffer must be committed");
        assert_eq!(w.buffered(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn opening_an_existing_partial_never_truncates_it() {
        let dir = tmpdir("grow");
        let path = dir.join("p.bin");
        std::fs::write(&path, vec![7u8; 5000]).unwrap();

        // Re-opening with a smaller declared total must leave the existing bytes alone.
        let f = SparseFile::open(&path, Some(1000)).unwrap();
        assert_eq!(f.size().unwrap(), 5000);

        // A larger total grows the file.
        let f = SparseFile::open(&path, Some(9000)).unwrap();
        assert_eq!(f.size().unwrap(), 9000);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unique_path_avoids_names_held_by_an_in_flight_download() {
        let dir = tmpdir("uniq");
        let wanted = dir.join("movie.mkv");
        assert_eq!(unique_path(&wanted), wanted, "a free name is used as is");

        // Only the partial exists: the name is still reserved.
        std::fs::write(dir.join("movie.mkv.fdmpart"), b"").unwrap();
        assert_eq!(unique_path(&wanted), dir.join("movie (1).mkv"));

        std::fs::write(dir.join("movie (1).mkv"), b"").unwrap();
        assert_eq!(unique_path(&wanted), dir.join("movie (2).mkv"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn finish_renames_the_payload_and_clears_resume_state() {
        let dir = tmpdir("finish");
        let paths = DownloadPaths::new(dir.join("done.zip"));
        std::fs::write(&paths.part_path, b"payload").unwrap();
        std::fs::write(&paths.state_path, b"{}").unwrap();

        paths.finish().await.unwrap();

        assert_eq!(std::fs::read(&paths.final_path).unwrap(), b"payload");
        assert!(!paths.part_path.exists());
        assert!(!paths.state_path.exists(), "a finished file must not look resumable");
        std::fs::remove_dir_all(&dir).ok();
    }
}
