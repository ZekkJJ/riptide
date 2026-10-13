//! Fetching an adaptive stream into a single file.
//!
//! Segment lists are ordered and must be concatenated in order, so this is a different problem
//! from a ranged download: the parallelism available is in fetching *ahead*, not in splitting a
//! range. Segments are fetched with a bounded look-ahead and written strictly in order, which
//! keeps a fast link busy without holding the whole stream in memory.
//!
//! Plain AES-128, where the key is served in the clear to any client that asks for it, is part
//! of RFC 8216 and is decrypted here. DRM systems are refused by
//! [`SegmentPlan::check_supported`] before this module is reached.

use crate::error::{Error, Result};
use crate::media::{Encryption, Segment, SegmentPlan};
use crate::probe::RequestSpec;
use crate::storage::{unique_path, DownloadPaths};
use crate::DownloadConfig;
use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use futures_util::{stream, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

/// How many segments to fetch concurrently while writing in order.
const LOOK_AHEAD: usize = 6;

/// Resume state for a stream, which advances by whole segments rather than by byte ranges.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StreamState {
    version: u32,
    /// Segments already appended, counting the init segment.
    segments_done: usize,
    /// Bytes written, used to trim a partial trailing segment before resuming.
    bytes: u64,
    total_segments: usize,
}

/// Fetch every segment of `plan` into `dir/name`, reporting progress as segments complete.
///
/// `on_progress` receives `(segments_done, segments_total, bytes_written)`.
pub async fn download_plan<F>(
    plan: &SegmentPlan,
    dir: impl AsRef<Path>,
    name: &str,
    spec: &RequestSpec,
    config: DownloadConfig,
    mut on_progress: F,
) -> Result<PathBuf>
where
    F: FnMut(usize, usize, u64),
{
    plan.check_supported()?;

    let dir = dir.as_ref();
    tokio::fs::create_dir_all(dir).await.map_err(|e| Error::io(dir, e))?;

    // The full work list is the init segment followed by the media segments.
    let mut work: Vec<Segment> = Vec::with_capacity(plan.total_segments());
    if let Some(init) = &plan.init {
        work.push(init.clone());
    }
    work.extend(plan.segments.iter().cloned());
    let total = work.len();

    let client = crate::probe::client(config.proxy.as_deref(), config.timeout)?;

    // Resume if a previous run left state for this exact stream.
    let desired = dir.join(name);
    let state_path = DownloadPaths::new(&desired).state_path;
    let resumed = load_stream_state(&state_path, total).await;
    let paths = match &resumed {
        Some(_) => DownloadPaths::new(&desired),
        None => DownloadPaths::new(unique_path(&desired)),
    };
    let mut done = resumed.as_ref().map(|s| s.segments_done).unwrap_or(0);
    let mut bytes = resumed.as_ref().map(|s| s.bytes).unwrap_or(0);

    let mut file = open_append(&paths.part_path, bytes).await?;
    if done > 0 {
        tracing::info!(done, total, "resuming stream download");
    }

    // The decryption key is served over HTTP like any other resource.
    let key = match &plan.encryption {
        Some(Encryption::Aes128 { key_uri, iv }) => {
            let key_url = crate::media::resolve(&spec.url, key_uri).unwrap_or_else(|| key_uri.clone());
            Some((fetch_key(&client, spec, &key_url).await?, iv.clone()))
        }
        Some(Encryption::Drm { system }) => {
            return Err(Error::Manifest {
                kind: "stream",
                detail: format!("{system} DRM is not supported"),
            })
        }
        _ => None,
    };

    on_progress(done, total, bytes);

    // Fetch ahead, yield in order. `buffered` keeps up to LOOK_AHEAD requests in flight while
    // preserving the original ordering, which is exactly what concatenation needs.
    let remaining: Vec<(usize, Segment)> =
        work.into_iter().enumerate().skip(done).collect();

    let mut fetches = stream::iter(remaining.into_iter().map(|(index, seg)| {
        let client = client.clone();
        let spec = spec.clone();
        let retries = config.max_retries;
        async move { (index, fetch_segment(&client, &spec, &seg, retries).await) }
    }))
    .buffered(LOOK_AHEAD);

    while let Some((index, result)) = fetches.next().await {
        let data = result?;
        let plain = match &key {
            Some((key_bytes, iv)) => decrypt_segment(&data, key_bytes, iv.as_deref(), index)?,
            None => data,
        };

        use tokio::io::AsyncWriteExt;
        file.write_all(&plain).await.map_err(|e| Error::io(&paths.part_path, e))?;
        bytes += plain.len() as u64;
        done = index + 1;

        on_progress(done, total, bytes);

        // Checkpoint periodically so an interrupted stream resumes near where it stopped.
        if done % 20 == 0 {
            file.flush().await.map_err(|e| Error::io(&paths.part_path, e))?;
            save_stream_state(&state_path, done, bytes, total).await;
        }
    }

    use tokio::io::AsyncWriteExt;
    file.flush().await.map_err(|e| Error::io(&paths.part_path, e))?;
    file.sync_all().await.map_err(|e| Error::io(&paths.part_path, e))?;
    drop(file);

    paths.finish().await.map_err(|e| Error::io(&paths.final_path, e))?;
    Ok(paths.final_path)
}

async fn open_append(path: &Path, truncate_to: u64) -> Result<tokio::fs::File> {
    let mut opts = tokio::fs::OpenOptions::new();
    opts.create(true).write(true).read(true);
    let file = opts.open(path).await.map_err(|e| Error::io(path, e))?;
    // A checkpoint records whole segments only, so trim any partial tail written after it.
    file.set_len(truncate_to).await.map_err(|e| Error::io(path, e))?;
    let mut file = file;
    use tokio::io::AsyncSeekExt;
    file.seek(std::io::SeekFrom::Start(truncate_to)).await.map_err(|e| Error::io(path, e))?;
    Ok(file)
}

async fn load_stream_state(path: &Path, total: usize) -> Option<StreamState> {
    let bytes = tokio::fs::read(path).await.ok()?;
    let state: StreamState = serde_json::from_slice(&bytes).ok()?;
    // A different segment count means a different stream or variant; do not splice them.
    if state.version != 1 || state.total_segments != total || state.segments_done >= total {
        return None;
    }
    Some(state)
}

async fn save_stream_state(path: &Path, done: usize, bytes: u64, total: usize) {
    let state = StreamState { version: 1, segments_done: done, bytes, total_segments: total };
    if let Ok(json) = serde_json::to_vec(&state) {
        let _ = tokio::fs::write(path, json).await;
    }
}

async fn fetch_key(client: &Client, spec: &RequestSpec, url: &str) -> Result<[u8; 16]> {
    let resp = client.get(url).headers(spec.headers(None)).send().await?;
    if !resp.status().is_success() {
        return Err(Error::BadStatus { status: resp.status().as_u16(), url: url.to_string() });
    }
    let bytes = resp.bytes().await?;
    if bytes.len() != 16 {
        return Err(Error::Manifest {
            kind: "stream",
            detail: format!("expected a 16-byte AES-128 key, got {} bytes", bytes.len()),
        });
    }
    let mut key = [0u8; 16];
    key.copy_from_slice(&bytes);
    Ok(key)
}

async fn fetch_segment(
    client: &Client,
    spec: &RequestSpec,
    seg: &Segment,
    retries: u32,
) -> Result<Vec<u8>> {
    let range = seg.byte_range.map(|(off, len)| (off, Some(off + len - 1)));
    let mut attempt = 0;
    loop {
        match try_fetch_segment(client, spec, &seg.url, range).await {
            Ok(data) => return Ok(data),
            Err(e) => {
                if !e.is_retryable() || attempt >= retries {
                    return Err(e);
                }
                attempt += 1;
                let delay = std::time::Duration::from_millis(300 * (1u64 << attempt.min(5)));
                tokio::time::sleep(delay).await;
            }
        }
    }
}

async fn try_fetch_segment(
    client: &Client,
    spec: &RequestSpec,
    url: &str,
    range: Option<(u64, Option<u64>)>,
) -> Result<Vec<u8>> {
    let resp = client.get(url).headers(spec.headers(range)).send().await?;
    if !resp.status().is_success() {
        return Err(Error::BadStatus { status: resp.status().as_u16(), url: url.to_string() });
    }
    Ok(resp.bytes().await?.to_vec())
}

/// Decrypt one AES-128-CBC segment.
///
/// Per RFC 8216, when no IV is declared the media sequence number is used as a 16-byte
/// big-endian IV, which is why the segment index has to be threaded through here.
fn decrypt_segment(
    data: &[u8],
    key: &[u8; 16],
    declared_iv: Option<&str>,
    index: usize,
) -> Result<Vec<u8>> {
    let iv = match declared_iv {
        Some(hex) => parse_hex_iv(hex).ok_or_else(|| Error::Manifest {
            kind: "stream",
            detail: format!("could not read the declared IV {hex:?}"),
        })?,
        None => {
            let mut iv = [0u8; 16];
            iv[8..].copy_from_slice(&(index as u64).to_be_bytes());
            iv
        }
    };
    Aes128CbcDec::new(key.into(), &iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(data)
        .map_err(|e| Error::Manifest {
            kind: "stream",
            detail: format!("segment {index} failed to decrypt: {e}"),
        })
}

/// Parse a `0x`-prefixed 32-digit hex IV.
fn parse_hex_iv(s: &str) -> Option<[u8; 16]> {
    let hex = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    if hex.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::block_padding::Pkcs7 as Pad;
    use aes::cipher::BlockEncryptMut;

    type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

    #[test]
    fn hex_ivs_are_parsed_and_rejected_appropriately() {
        let iv = parse_hex_iv("0x000102030405060708090a0b0c0d0e0f").unwrap();
        assert_eq!(iv, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
        // Case and the prefix are both flexible.
        assert_eq!(parse_hex_iv("0XAABB00000000000000000000000000FF").unwrap()[0], 0xAA);
        assert!(parse_hex_iv("0x1234").is_none(), "wrong length");
        assert!(parse_hex_iv("0xzz0102030405060708090a0b0c0d0e0f").is_none(), "not hex");
    }

    #[test]
    fn a_segment_encrypted_with_a_declared_iv_round_trips() {
        let key = [7u8; 16];
        let iv = [3u8; 16];
        let plain = b"the quick brown fox jumps over the lazy dog, twice over".to_vec();
        let ct = Aes128CbcEnc::new(&key.into(), &iv.into()).encrypt_padded_vec_mut::<Pad>(&plain);

        let got = decrypt_segment(&ct, &key, Some("0x03030303030303030303030303030303"), 0).unwrap();
        assert_eq!(got, plain);
    }

    #[test]
    fn without_a_declared_iv_the_sequence_number_is_used() {
        // RFC 8216: the media sequence number becomes a big-endian 16-byte IV.
        let key = [9u8; 16];
        let index = 42usize;
        let mut iv = [0u8; 16];
        iv[8..].copy_from_slice(&(index as u64).to_be_bytes());

        let plain = b"segment forty two payload".to_vec();
        let ct = Aes128CbcEnc::new(&key.into(), &iv.into()).encrypt_padded_vec_mut::<Pad>(&plain);

        assert_eq!(decrypt_segment(&ct, &key, None, index).unwrap(), plain);

        // A wrong sequence number cannot be detected: in CBC the IV only feeds the first
        // block, so the rest decrypts cleanly and the PKCS7 padding in the final block still
        // validates. The damage is silent and confined to the first 16 bytes, which is why
        // the index must be threaded through correctly rather than guessed at.
        let wrong = decrypt_segment(&ct, &key, None, index + 1)
            .expect("a wrong IV still passes the padding check");
        assert_ne!(wrong[..16], plain[..16], "the first block is corrupted");
        assert_eq!(wrong[16..], plain[16..], "later blocks are unaffected");
    }

    #[test]
    fn the_wrong_key_is_reported_rather_than_returning_garbage() {
        let plain = b"abcdefghijklmnop".to_vec();
        let ct = Aes128CbcEnc::new(&[1u8; 16].into(), &[2u8; 16].into())
            .encrypt_padded_vec_mut::<Pad>(&plain);
        let err = decrypt_segment(&ct, &[5u8; 16], Some("0x02020202020202020202020202020202"), 0);
        assert!(err.is_err(), "a bad key must fail the padding check");
    }

    #[tokio::test]
    async fn stream_state_is_only_reused_for_a_matching_segment_count() {
        let dir = std::env::temp_dir().join(format!("freedm-sst-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.mp4.fdmstate");

        save_stream_state(&path, 10, 4096, 50).await;
        assert_eq!(load_stream_state(&path, 50).await.unwrap().segments_done, 10);
        // A different variant has a different segment count and must not be spliced in.
        assert!(load_stream_state(&path, 73).await.is_none());

        // A finished stream is not resumable.
        save_stream_state(&path, 50, 4096, 50).await;
        assert!(load_stream_state(&path, 50).await.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_checkpoint_trims_a_partial_trailing_segment() {
        let dir = std::env::temp_dir().join(format!("freedm-trim-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.part");
        // More bytes on disk than the checkpoint records, as a kill mid-write would leave.
        std::fs::write(&path, vec![1u8; 5000]).unwrap();

        let file = open_append(&path, 4096).await.unwrap();
        drop(file);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 4096);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_drm_plan_is_refused_before_any_request_is_made() {
        let plan = SegmentPlan {
            segments: vec![Segment { url: "https://e/x.ts".into(), ..Default::default() }],
            encryption: Some(Encryption::Drm { system: "Widevine".into() }),
            ..Default::default()
        };
        let err = download_plan(
            &plan,
            std::env::temp_dir(),
            "x.ts",
            &RequestSpec::new("https://e/m.m3u8"),
            DownloadConfig::default(),
            |_, _, _| {},
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("Widevine"), "{err}");
    }
}
