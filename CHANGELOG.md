# Changelog

All notable changes to this project are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] — 2026-10-13

First public release.

### Added

- **Download engine** with dynamic work-stealing segmentation. A freed connection carves a new
  segment out of whichever range has the most work outstanding, so a slow connection cannot
  hold the transfer back. The chunk list is maintained as an exact tiling of the file, and the
  invariant is enforced on every mutation and on every state load.
- **No reassembly pass.** Segments are written straight into their final position in one
  sparse file using positional writes, so a finished transfer needs only a rename.
- **Resume**, checkpointed beside the payload as JSON. Before reusing partial data the remote
  file is re-probed and its strong `ETag`, `Last-Modified` and size compared; a changed file is
  refused rather than spliced into. Weak validators are deliberately ignored.
- **Expired-link detection** — a web page served where a file was expected is reported as such
  instead of being saved as the file.
- **HLS and DASH** manifest parsing: quality variants, `SegmentTimeline` repeat counts,
  `$Number%05d$` template expansion, byte-range segments and alternative audio renditions.
  Streams are fetched with look-ahead into a single file and resume by segment.
- **AES-128 HLS decryption**, for streams whose key is served in the clear per RFC 8216.
- **DRM detection and refusal** for Widevine, PlayReady and FairPlay.
- **Mirrors** with per-URL failure counts, **proxies** (HTTP and SOCKS5), **speed limiting**
  through a shared token bucket, and **categories** that file downloads into folders by type.
- **`riptide` CLI** — download, resume, list stream variants, categorise.
- **`riptided` service** — a queue with a concurrency limit, an HTTP API, server-sent progress
  events, and a local web UI.
- **Browser extension** (Manifest V3) for Chrome, Edge and Brave. Hands downloads to Riptide
  with their referer and, optionally, the site's cookies. Host access is limited to the
  loopback address; `<all_urls>` is an opt-in permission for logged-in sites.
- Token-authenticated local API bound to `127.0.0.1`, with a constant-time comparison and a
  web UI served without CORS headers so the token cannot be read cross-origin.

### Notes

- 126 tests covering chunk tiling, out-of-order reassembly, resume validation, filename
  sanitisation including traversal attempts, manifest parsing, AES-128 round trips, rate
  limiting and API authentication.
- Verified end to end: an eight-connection download is SHA-256 identical to the same file
  fetched over one connection; a download killed at 18% resumes to the same hash; and a
  64-segment 1080p HLS stream produces a valid 502 MB transport stream with correct packet
  sync at both ends.

### Not yet implemented

FTP, site grabber, ZIP preview, scheduler, Firefox extension, automatic link recapture on
expiry, and media conversion.

[Unreleased]: https://github.com/Bloxive/riptide/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Bloxive/riptide/releases/tag/v0.1.0
