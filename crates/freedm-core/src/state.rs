//! Resume state persisted beside the partial payload.
//!
//! The state file records the chunk tiling plus the validators the server gave at probe time.
//! On resume those validators are compared against a fresh probe: if the remote file has
//! changed, the bytes already on disk belong to a different file and splicing new ranges into
//! them would produce a plausible-looking but corrupt result. That check is the difference
//! between a resume that works and one that quietly ruins the download.

use crate::chunks::ChunkMap;
use crate::download::DownloadConfig;
use crate::probe::ResourceInfo;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Bumped when the on-disk shape changes incompatibly.
pub const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeState {
    pub version: u32,
    /// The URL originally requested, kept so a stale `final_url` can be re-resolved.
    pub url: String,
    /// The URL the probe settled on after redirects.
    pub final_url: String,
    pub total: Option<u64>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub map: ChunkMap,
    pub max_connections: usize,
    #[serde(default)]
    pub mirrors: Vec<String>,
}

impl ResumeState {
    pub fn new(info: &ResourceInfo, map: &ChunkMap, config: &DownloadConfig) -> Self {
        Self {
            version: STATE_VERSION,
            url: info.final_url.clone(),
            final_url: info.final_url.clone(),
            total: info.total,
            etag: info.etag.clone(),
            last_modified: info.last_modified.clone(),
            map: map.clone(),
            max_connections: config.max_connections,
            mirrors: Vec::new(),
        }
    }

    /// The validators this state was recorded with, shaped for comparison against a probe.
    fn as_info(&self) -> ResourceInfo {
        ResourceInfo {
            final_url: self.final_url.clone(),
            total: self.total,
            supports_ranges: true,
            filename: String::new(),
            content_type: None,
            etag: self.etag.clone(),
            last_modified: self.last_modified.clone(),
        }
    }

    /// Whether the partial data on disk may be extended using `fresh`.
    pub fn reusable(&self, fresh: &ResourceInfo) -> Result<(), String> {
        if self.version != STATE_VERSION {
            return Err(format!("state written by an incompatible version ({})", self.version));
        }
        self.map.validate().map_err(|e| format!("state file is inconsistent: {e}"))?;
        if !fresh.supports_ranges && self.map.downloaded() > 0 {
            return Err("the server no longer supports resuming".into());
        }
        if let Some(reason) = fresh.resume_conflict(&self.as_info()) {
            return Err(reason);
        }
        Ok(())
    }

    /// Write the state file atomically, so an interrupted save cannot leave a truncated file
    /// that would be rejected on the next resume.
    pub async fn save(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let path = path.as_ref().to_path_buf();
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        tokio::task::spawn_blocking(move || {
            let tmp = path.with_extension("fdmstate.new");
            std::fs::write(&tmp, &json)?;
            std::fs::rename(&tmp, &path)
        })
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?
    }

    /// Read the state file, treating anything unreadable or malformed as simply absent: a
    /// damaged state file should start a clean download, never abort one.
    pub async fn load(path: impl AsRef<Path>) -> Option<Self> {
        let path = path.as_ref().to_path_buf();
        let bytes = tokio::fs::read(&path).await.ok()?;
        match serde_json::from_slice::<Self>(&bytes) {
            Ok(state) => Some(state),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "ignoring unreadable resume state");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(etag: Option<&str>, total: Option<u64>, ranges: bool) -> ResourceInfo {
        ResourceInfo {
            final_url: "https://example.com/f.bin".into(),
            total,
            supports_ranges: ranges,
            filename: "f.bin".into(),
            content_type: None,
            etag: etag.map(Into::into),
            last_modified: None,
        }
    }

    fn state(etag: Option<&str>, total: u64, done: u64) -> ResumeState {
        let mut map = ChunkMap::sized(total);
        if done > 0 {
            let c = map.claim_idle().unwrap();
            map.advance(c.id, done);
            map.release_all();
        }
        ResumeState::new(&info(etag, Some(total), true), &map, &DownloadConfig::default())
    }

    #[test]
    fn a_matching_file_is_reusable() {
        let s = state(Some("\"abc\""), 1000, 400);
        s.reusable(&info(Some("\"abc\""), Some(1000), true)).unwrap();
    }

    #[test]
    fn a_replaced_file_is_rejected() {
        let s = state(Some("\"v1\""), 1000, 400);
        let err = s.reusable(&info(Some("\"v2\""), Some(1000), true)).unwrap_err();
        assert!(err.contains("ETag"), "{err}");
    }

    #[test]
    fn a_resized_file_is_rejected_without_a_validator() {
        let s = state(None, 1000, 400);
        let err = s.reusable(&info(None, Some(2000), true)).unwrap_err();
        assert!(err.contains("size changed"), "{err}");
    }

    #[test]
    fn losing_range_support_blocks_a_partial_resume() {
        let s = state(Some("\"abc\""), 1000, 400);
        let err = s.reusable(&info(Some("\"abc\""), Some(1000), false)).unwrap_err();
        assert!(err.contains("resuming"), "{err}");
    }

    #[test]
    fn losing_range_support_is_fine_when_nothing_was_downloaded_yet() {
        let s = state(Some("\"abc\""), 1000, 0);
        s.reusable(&info(Some("\"abc\""), Some(1000), false)).unwrap();
    }

    #[test]
    fn a_future_state_version_is_rejected() {
        let mut s = state(None, 1000, 400);
        s.version = STATE_VERSION + 1;
        assert!(s.reusable(&info(None, Some(1000), true)).is_err());
    }

    #[test]
    fn an_inconsistent_chunk_tiling_is_rejected() {
        let s = state(None, 1000, 400);
        let mut json = serde_json::to_value(&s).unwrap();
        json["map"]["chunks"][0]["end"] = serde_json::json!(50);
        let tampered: ResumeState = serde_json::from_value(json).unwrap();
        let err = tampered.reusable(&info(None, Some(1000), true)).unwrap_err();
        assert!(err.contains("inconsistent"), "{err}");
    }

    #[tokio::test]
    async fn state_round_trips_through_a_file() {
        let dir = std::env::temp_dir().join(format!("freedm-state-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.bin.fdmstate");

        let s = state(Some("\"abc\""), 4096, 1024);
        s.save(&path).await.unwrap();
        let back = ResumeState::load(&path).await.unwrap();

        assert_eq!(back.map.downloaded(), 1024);
        assert_eq!(back.etag.as_deref(), Some("\"abc\""));
        back.reusable(&info(Some("\"abc\""), Some(4096), true)).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_corrupt_state_file_reads_as_absent_rather_than_failing() {
        let dir = std::env::temp_dir().join(format!("freedm-bad-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.fdmstate");
        std::fs::write(&path, b"{ this is not json").unwrap();

        // A damaged state file must start a clean download, not abort one.
        assert!(ResumeState::load(&path).await.is_none());
        assert!(ResumeState::load(dir.join("missing.fdmstate")).await.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
