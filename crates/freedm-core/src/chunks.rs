//! Dynamic work-stealing segmentation.
//!
//! A download is modelled as an ordered tiling of `[0, total)` by [`Chunk`]s. Every byte of
//! the file belongs to exactly one chunk, and a chunk tracks how far into its own range the
//! transfer has progressed. There are no gaps and no overlaps at any point in the lifetime of
//! the map; every mutation preserves that invariant, which is what makes the on-disk state
//! safe to resume from.
//!
//! Unlike a fixed "split into N parts up front" scheme, segments here are created lazily by
//! [`ChunkMap::split_largest`]: when a connection slot comes free, the chunk with the most
//! bytes still outstanding gives up the second half of its *remaining* range. A connection
//! that stalls or crawls therefore accumulates outstanding bytes and becomes the next steal
//! target on its own, with no separate "find the slow connection" pass. The victim's end
//! bound moves backwards while its worker is mid-flight, so workers must re-read their bound
//! between writes and stop once the cursor reaches it.

use serde::{Deserialize, Serialize};

/// Smallest range worth giving its own connection. A split must leave at least this many
/// bytes on both sides, so `split_largest` needs twice this to act.
pub const DEFAULT_MIN_SPLIT: u64 = 1 << 20; // 1 MiB

/// One contiguous range of the output file and the transfer progress within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    pub id: u64,
    /// First byte of the range.
    pub start: u64,
    /// Next byte to be written. Always `start <= cursor <= end`.
    pub cursor: u64,
    /// One past the last byte of the range.
    pub end: u64,
    /// Whether a worker currently owns this chunk.
    ///
    /// Deliberately never persisted. Ownership is a fact about a live process, so a run that
    /// is killed mid-transfer must not leave chunks marked as owned in the state file: they
    /// would be skipped by `claim_idle` forever and the download could never complete.
    #[serde(skip)]
    pub active: bool,
}

impl Chunk {
    /// Bytes already transferred for this chunk.
    pub fn done(&self) -> u64 {
        self.cursor - self.start
    }

    /// Bytes still outstanding before this chunk is finished.
    pub fn remaining(&self) -> u64 {
        self.end.saturating_sub(self.cursor)
    }

    pub fn is_complete(&self) -> bool {
        self.cursor >= self.end
    }
}

/// The result of a successful steal: an existing chunk was truncated and a new one carved out
/// of its tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Split {
    /// Chunk whose `end` moved backwards. Its worker, if any, must stop at the new bound.
    pub victim: u64,
    /// The newly created, idle chunk.
    pub created: Chunk,
}

/// Ordered tiling of a download's byte range.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkMap {
    /// Total size, or `None` when the server would not tell us (no `Content-Length`, or a
    /// `Transfer-Encoding: chunked` response). An unsized map is a single unbounded chunk and
    /// is never split.
    total: Option<u64>,
    /// Sorted by `start`, tiling `[0, total)` with no gaps or overlaps.
    chunks: Vec<Chunk>,
    next_id: u64,
    min_split: u64,
}

impl ChunkMap {
    /// A map for a download of known length, starting as one chunk covering the whole file.
    pub fn sized(total: u64) -> Self {
        let chunks = if total == 0 {
            Vec::new()
        } else {
            vec![Chunk { id: 0, start: 0, cursor: 0, end: total, active: false }]
        };
        Self { total: Some(total), chunks, next_id: 1, min_split: DEFAULT_MIN_SPLIT }
    }

    /// A map for a download of unknown length: one chunk, never split, grown as bytes arrive.
    pub fn unsized_stream() -> Self {
        Self {
            total: None,
            chunks: vec![Chunk { id: 0, start: 0, cursor: 0, end: u64::MAX, active: false }],
            next_id: 1,
            min_split: DEFAULT_MIN_SPLIT,
        }
    }

    pub fn with_min_split(mut self, min_split: u64) -> Self {
        self.min_split = min_split.max(1);
        self
    }

    pub fn total(&self) -> Option<u64> {
        self.total
    }

    pub fn is_sized(&self) -> bool {
        self.total.is_some()
    }

    pub fn min_split(&self) -> u64 {
        self.min_split
    }

    pub fn chunks(&self) -> &[Chunk] {
        &self.chunks
    }

    /// Bytes transferred across every chunk.
    pub fn downloaded(&self) -> u64 {
        self.chunks.iter().map(Chunk::done).sum()
    }

    /// Bytes outstanding, or `None` when the total is unknown.
    pub fn remaining(&self) -> Option<u64> {
        self.total.map(|t| t.saturating_sub(self.downloaded()))
    }

    pub fn is_complete(&self) -> bool {
        match self.total {
            Some(_) => self.chunks.iter().all(Chunk::is_complete),
            // An unsized stream is only complete once the transfer ends, which the map cannot
            // observe on its own; the caller decides and calls `finalize_unsized`.
            None => false,
        }
    }

    pub fn active_count(&self) -> usize {
        self.chunks.iter().filter(|c| c.active && !c.is_complete()).count()
    }

    pub fn get(&self, id: u64) -> Option<&Chunk> {
        self.chunks.iter().find(|c| c.id == id)
    }

    fn index_of(&self, id: u64) -> Option<usize> {
        self.chunks.iter().position(|c| c.id == id)
    }

    /// Take ownership of an unfinished chunk that no worker holds. Used to pick up chunks left
    /// behind by a previous run, and to fill connection slots before resorting to a steal.
    pub fn claim_idle(&mut self) -> Option<Chunk> {
        // Prefer the idle chunk with the most outstanding bytes, so the first connections
        // after a resume go where the work is.
        let idx = self
            .chunks
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.active && !c.is_complete())
            .max_by_key(|(_, c)| c.remaining())
            .map(|(i, _)| i)?;
        self.chunks[idx].active = true;
        Some(self.chunks[idx])
    }

    /// Carve a new chunk out of the tail of whichever chunk has the most bytes outstanding.
    ///
    /// Returns `None` when no chunk holds enough outstanding bytes to be worth dividing, or
    /// when the total size is unknown. The victim's `end` moves back to the split point, so a
    /// worker already streaming that chunk must stop early.
    pub fn split_largest(&mut self) -> Option<Split> {
        self.total?;
        let idx = self
            .chunks
            .iter()
            .enumerate()
            .filter(|(_, c)| !c.is_complete())
            .max_by_key(|(_, c)| c.remaining())
            .map(|(i, _)| i)?;

        let remaining = self.chunks[idx].remaining();
        // Both halves must be worth a connection of their own.
        if remaining < self.min_split.saturating_mul(2) {
            return None;
        }

        let victim = self.chunks[idx];
        let split_at = victim.cursor + remaining / 2;
        debug_assert!(split_at > victim.cursor && split_at < victim.end);

        self.chunks[idx].end = split_at;

        let created =
            Chunk { id: self.next_id, start: split_at, cursor: split_at, end: victim.end, active: false };
        self.next_id += 1;
        self.chunks.insert(idx + 1, created);

        Some(Split { victim: victim.id, created })
    }

    /// Mark a chunk as owned by a worker.
    pub fn mark_active(&mut self, id: u64, active: bool) {
        if let Some(i) = self.index_of(id) {
            self.chunks[i].active = active;
        }
    }

    /// Release every chunk, as when loading state from disk or shutting workers down.
    pub fn release_all(&mut self) {
        for c in &mut self.chunks {
            c.active = false;
        }
    }

    /// Record `n` bytes written at the chunk's cursor. Returns the chunk's new bound so the
    /// caller can notice a steal that moved it, and stop.
    pub fn advance(&mut self, id: u64, n: u64) -> Option<ChunkBound> {
        let i = self.index_of(id)?;
        let c = &mut self.chunks[i];
        c.cursor = c.cursor.saturating_add(n).min(c.end);
        Some(ChunkBound { cursor: c.cursor, end: c.end })
    }

    /// The current bound of a chunk, which may have moved since the worker last looked.
    pub fn bound(&self, id: u64) -> Option<ChunkBound> {
        self.get(id).map(|c| ChunkBound { cursor: c.cursor, end: c.end })
    }

    /// Fix up the total once an unsized stream has ended, turning the map into a sized one.
    pub fn finalize_unsized(&mut self, received: u64) {
        if self.total.is_none() {
            self.total = Some(received);
            if let Some(c) = self.chunks.first_mut() {
                c.end = received;
                c.cursor = received;
            }
        }
    }

    /// Merge runs of adjacent finished chunks so the persisted state stays small. Chunks still
    /// owned by a worker are left alone even when complete, so a worker's id stays resolvable
    /// until it reports in.
    pub fn coalesce(&mut self) {
        let mut i = 0;
        while i + 1 < self.chunks.len() {
            let a = self.chunks[i];
            let b = self.chunks[i + 1];
            if a.is_complete() && b.is_complete() && !a.active && !b.active && a.end == b.start {
                self.chunks[i].end = b.end;
                self.chunks[i].cursor = b.cursor;
                self.chunks.remove(i + 1);
            } else {
                i += 1;
            }
        }
    }

    /// Check the tiling invariant. Used by tests and by state loading, which must reject a
    /// corrupt or hand-edited state file rather than write bytes to the wrong offsets.
    pub fn validate(&self) -> Result<(), String> {
        let Some(total) = self.total else {
            return if self.chunks.len() == 1 {
                Ok(())
            } else {
                Err("unsized map must hold exactly one chunk".into())
            };
        };
        if self.chunks.is_empty() {
            return if total == 0 {
                Ok(())
            } else {
                Err(format!("empty chunk list for a {total}-byte download"))
            };
        }
        let mut expect = 0u64;
        for c in &self.chunks {
            if c.start != expect {
                return Err(format!("gap or overlap at {}: expected start {expect}", c.start));
            }
            if c.end < c.start {
                return Err(format!("inverted range [{}, {})", c.start, c.end));
            }
            if c.cursor < c.start || c.cursor > c.end {
                return Err(format!("cursor {} outside [{}, {}]", c.cursor, c.start, c.end));
            }
            expect = c.end;
        }
        if expect != total {
            return Err(format!("chunks cover {expect} bytes, expected {total}"));
        }
        Ok(())
    }
}

/// A chunk's live cursor and end bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkBound {
    pub cursor: u64,
    pub end: u64,
}

impl ChunkBound {
    /// Whether the worker holding this chunk should stop, either because the range is finished
    /// or because a steal moved the end bound down to the cursor.
    pub fn is_done(&self) -> bool {
        self.cursor >= self.end
    }

    pub fn remaining(&self) -> u64 {
        self.end.saturating_sub(self.cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1 << 20;

    #[test]
    fn sized_map_starts_as_one_whole_chunk() {
        let m = ChunkMap::sized(100 * MB);
        assert_eq!(m.chunks().len(), 1);
        assert_eq!(m.chunks()[0].start, 0);
        assert_eq!(m.chunks()[0].end, 100 * MB);
        assert_eq!(m.downloaded(), 0);
        m.validate().unwrap();
    }

    #[test]
    fn split_halves_the_remaining_range_not_the_whole_range() {
        let mut m = ChunkMap::sized(100 * MB);
        let first = m.claim_idle().unwrap();
        // Simulate 60 MiB already transferred, so 40 MiB is outstanding.
        m.advance(first.id, 60 * MB);

        let split = m.split_largest().expect("40 MiB is well above 2x min split");
        // The new chunk starts at the midpoint of what is LEFT (60 + 20), not of the file.
        assert_eq!(split.created.start, 80 * MB);
        assert_eq!(split.created.end, 100 * MB);
        assert_eq!(m.get(split.victim).unwrap().end, 80 * MB);
        m.validate().unwrap();
    }

    #[test]
    fn split_refuses_when_the_halves_would_be_too_small() {
        let mut m = ChunkMap::sized(MB).with_min_split(MB);
        // 1 MiB outstanding, but a split needs 2 MiB to leave 1 MiB on each side.
        assert!(m.split_largest().is_none());
        assert_eq!(m.chunks().len(), 1);
    }

    #[test]
    fn steal_targets_the_slowest_connection() {
        let mut m = ChunkMap::sized(120 * MB).with_min_split(MB);
        m.split_largest().unwrap();
        m.split_largest().unwrap();
        let ids: Vec<u64> = m.chunks().iter().map(|c| c.id).collect();
        assert_eq!(ids.len(), 3);
        for &id in &ids {
            m.mark_active(id, true);
        }

        // Two connections race ahead; the middle one barely moves.
        let per = m.get(ids[0]).unwrap().remaining();
        m.advance(ids[0], per - MB / 2);
        m.advance(ids[1], MB); // the laggard
        let per2 = m.get(ids[2]).unwrap().remaining();
        m.advance(ids[2], per2 - MB / 2);

        // The next steal comes out of the laggard, with no explicit "slowest" search.
        let split = m.split_largest().unwrap();
        assert_eq!(split.victim, ids[1]);
        m.validate().unwrap();
    }

    #[test]
    fn repeated_splits_keep_the_file_exactly_tiled() {
        let mut m = ChunkMap::sized(64 * MB).with_min_split(64 * 1024);
        for _ in 0..500 {
            if m.split_largest().is_none() {
                break;
            }
            m.validate().unwrap();
        }
        assert!(m.chunks().len() > 8, "expected many segments, got {}", m.chunks().len());
        let covered: u64 = m.chunks().iter().map(|c| c.end - c.start).sum();
        assert_eq!(covered, 64 * MB, "no byte may be claimed twice or dropped");
    }

    #[test]
    fn advance_reports_a_bound_that_a_steal_moved_backwards() {
        let mut m = ChunkMap::sized(100 * MB);
        let c = m.claim_idle().unwrap();
        m.advance(c.id, 10 * MB);
        let split = m.split_largest().unwrap();
        assert_eq!(split.victim, c.id);

        // The worker on `c` asks again and learns its range shrank.
        let bound = m.bound(c.id).unwrap();
        assert_eq!(bound.end, 55 * MB);
        assert!(!bound.is_done());

        // Once it reaches the new bound it must stop, though the file is far from done.
        m.advance(c.id, 45 * MB);
        assert!(m.bound(c.id).unwrap().is_done());
        assert!(!m.is_complete());
    }

    #[test]
    fn advance_never_runs_past_the_end_bound() {
        let mut m = ChunkMap::sized(10);
        let c = m.claim_idle().unwrap();
        let bound = m.advance(c.id, 999).unwrap();
        assert_eq!(bound.cursor, 10);
        assert_eq!(m.downloaded(), 10);
        m.validate().unwrap();
    }

    #[test]
    fn coalesce_merges_finished_neighbours() {
        let mut m = ChunkMap::sized(8 * MB).with_min_split(MB);
        m.split_largest().unwrap();
        m.split_largest().unwrap();
        let ids: Vec<u64> = m.chunks().iter().map(|c| c.id).collect();
        for id in ids {
            let r = m.get(id).unwrap().remaining();
            m.advance(id, r);
        }
        assert!(m.is_complete());
        m.coalesce();
        assert_eq!(m.chunks().len(), 1, "a finished download should persist as one chunk");
        m.validate().unwrap();
    }

    #[test]
    fn claim_idle_prefers_the_biggest_outstanding_range() {
        let mut m = ChunkMap::sized(100 * MB).with_min_split(MB);
        m.split_largest().unwrap(); // [0,50) [50,100)
        m.advance(0, 49 * MB); // chunk 0 is nearly done
        m.release_all();
        let got = m.claim_idle().unwrap();
        assert_eq!(got.start, 50 * MB, "should pick the untouched half, not the nearly-done one");
    }

    #[test]
    fn unsized_streams_are_never_split() {
        let mut m = ChunkMap::unsized_stream();
        assert!(m.split_largest().is_none());
        let c = m.claim_idle().unwrap();
        m.advance(c.id, 4096);
        assert_eq!(m.downloaded(), 4096);
        assert_eq!(m.remaining(), None);
        assert!(!m.is_complete());

        m.finalize_unsized(4096);
        assert_eq!(m.total(), Some(4096));
        assert!(m.is_complete());
        m.validate().unwrap();
    }

    #[test]
    fn zero_byte_downloads_are_immediately_complete() {
        let m = ChunkMap::sized(0);
        assert!(m.is_complete());
        assert_eq!(m.downloaded(), 0);
        m.validate().unwrap();
    }

    #[test]
    fn validate_rejects_a_tampered_state_file() {
        let mut m = ChunkMap::sized(100);
        m.split_largest();
        let mut json: serde_json::Value = serde_json::to_value(&m).unwrap();
        // Punch a hole in the middle of the tiling.
        json["chunks"][0]["end"] = serde_json::json!(10);
        let broken: ChunkMap = serde_json::from_value(json).unwrap();
        assert!(broken.validate().is_err());
    }

    #[test]
    fn state_survives_a_round_trip_through_json() {
        let mut m = ChunkMap::sized(32 * MB).with_min_split(MB);
        m.split_largest().unwrap();
        m.claim_idle().unwrap();
        m.advance(0, 1234);
        let json = serde_json::to_string(&m).unwrap();
        let back: ChunkMap = serde_json::from_str(&json).unwrap();
        back.validate().unwrap();
        assert_eq!(back.downloaded(), m.downloaded());
        assert_eq!(back.chunks().len(), m.chunks().len());
        // Ownership is a runtime fact, not a persisted one: nothing is held after a reload.
        assert_eq!(back.active_count(), 0);
    }
}
