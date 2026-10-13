<div align="center">

# Riptide

**A free, open source download manager.**

Multi-connection downloads, resume that cannot corrupt your file, browser capture,
and HLS/DASH video — with no licence key, no trial, and no kernel driver.

[![CI](https://github.com/ZekkJJ/riptide/actions/workflows/ci.yml/badge.svg)](https://github.com/ZekkJJ/riptide/actions/workflows/ci.yml)
[![Licence: GPL-3.0-or-later](https://img.shields.io/badge/licence-GPL--3.0--or--later-blue)](LICENSE)
[![Rust 1.82+](https://img.shields.io/badge/rust-1.82%2B-orange)](https://www.rust-lang.org)
[![Platforms](https://img.shields.io/badge/platforms-Windows%20%7C%20macOS%20%7C%20Linux-lightgrey)](#quick-start)

</div>

---

## What it does

- **Dynamic multi-connection downloads.** A file is not cut into fixed parts. Riptide starts
  with one range and, each time a connection frees up, carves a new segment out of whichever
  range has the most work left. A connection that slows down accumulates outstanding bytes and
  becomes the next thing to split, so one bad route cannot hold the whole download back.
- **No reassembly pass.** Every segment writes straight into its final position in one sparse
  file. When the last byte lands the download is finished — no temp directory, no
  concatenation step, no sitting at 100% while gigabytes are copied around.
- **Resume that is actually safe.** Progress is checkpointed beside the file. Before reusing a
  partial download Riptide re-probes the URL and compares the `ETag`, `Last-Modified` and size,
  and refuses if the remote file changed rather than splicing new bytes into stale ones.
- **Browser capture.** A Manifest V3 extension hands downloads from Chrome, Edge or Brave to
  Riptide, carrying the referer and — if you allow it — the site's cookies, so links from
  logged-in pages still work.
- **HLS and DASH.** Adaptive streams are parsed, quality variants listed, and segments fetched
  with look-ahead into a single file. Plain AES-128 is decrypted; DRM is detected and refused.
- **Queue, speed limits, categories, mirrors, proxies.** A local web UI to drive it all, and a
  CLI that works entirely on its own.

No telemetry. Riptide makes no network request you did not ask for.

## Quick start

```bash
git clone https://github.com/ZekkJJ/riptide
cd riptide
cargo build --release
```

Two binaries land in `target/release/`:

```bash
./target/release/riptide https://example.com/big-file.iso
```

```bash
./target/release/riptided --open
```

`riptide` is the command line tool. `riptided` is the background service: it runs the queue,
serves a web UI at <http://127.0.0.1:7739/>, and is what the browser extension talks to.

Rust 1.82 or newer is the only requirement. TLS is `rustls` and the crypto is pure Rust, so
there is no OpenSSL to install.

## Command line

```bash
riptide <url>                                   # download, 8 connections by default
riptide <url> -n 16 -o name.iso -d ~/Downloads  # connections, name, folder
riptide <url> -l 2M                             # cap at 2 MiB/s
riptide <url> --categorize                      # file into Video/, Audio/, Archives/, ...
riptide resume ./partly-downloaded.iso          # pick up where it stopped
riptide variants https://site/stream.m3u8       # list the quality levels
riptide https://site/stream.m3u8 --variant 1080 # fetch a stream as one file
riptide <url> --referer <page> --cookies "s=…"  # links behind a login
riptide <url> --mirror <url2> --mirror <url3>   # fall back between mirrors
riptide <url> --proxy socks5://127.0.0.1:1080
```

Interrupting with Ctrl-C leaves the `.rtpart` and `.rtstate` files in place; `riptide resume`
continues from there.

## Browser extension

1. Start the service: `riptided`. It prints a connection token.
2. Open `chrome://extensions`, turn on **Developer mode**, choose **Load unpacked**, and select
   the `extension/` folder.
3. Open the extension's options, paste the token, and press **Save and test**.

Downloads the browser starts are then handed to Riptide automatically, and
**right-click → Download with Riptide** always works on any link.

Cookie capture for logged-in sites is a separate, **optional** permission you grant from the
options page. Without it Riptide still downloads; some sites will simply refuse links that
need a session.

### A note on the local port

The extension reaches Riptide over `http://127.0.0.1:7739`, which means any page in your
browser can send requests to it too. Two things stop that mattering:

- Every API route requires the `X-Riptide-Token` header. The token is generated per install,
  lives only in your config file, is compared in constant time, and is never served over HTTP.
- The web UI has the token inlined and is served with no CORS headers, so a cross-origin
  `fetch` of it is blocked by the browser from reading the response.

The listener binds `127.0.0.1` explicitly, so nothing is reachable from your network. If you
would rather not have a local port at all, use the CLI — it talks to no one.

## Configuration

`riptided config-path` prints the location (`%APPDATA%\riptide\config.json` on Windows,
`~/.config/riptide/config.json` elsewhere). Everything is editable from the web UI's Settings
dialog, or directly:

| Setting | Meaning |
| --- | --- |
| `download_dir` | Where finished files go |
| `categorize` | File into `Video/`, `Audio/`, `Archives/`, … |
| `connections_per_download` | Segments per file (default 8) |
| `max_concurrent_downloads` | Files transferring at once (default 3) |
| `global_speed_limit` | Bytes per second across everything, 0 for unlimited |
| `max_retries` | Attempts per segment, and for the initial probe |
| `proxy` | `http://host:8080` or `socks5://host:1080` |
| `capture_min_size` | Smallest response worth taking over from the browser |
| `capture_exclusions` | Host globs never captured |
| `categories` | Extension → folder mapping |

## Why eight connections, and not eighty

More connections stop helping once the link is saturated, and many servers rate-limit or
blacklist clients that open a lot of them at once — especially across several simultaneous
downloads. Eight is a good default. Riptide will not split a range below 1 MiB, so small files
quietly use fewer.

If a server will not serve byte ranges, or will not state a length, Riptide detects that during
the probe and uses a single connection rather than producing a corrupt file.

## How it is put together

```
crates/riptide-core/     the engine
  chunks.rs              dynamic work-stealing segmentation  (the heart)
  probe.rs               size, range support, filename, resume validators
  download.rs            connection pool, retries, mirrors, pause/resume
  storage.rs             sparse file, positional writes, no reassembly
  state.rs               resume state and the "did the file change?" check
  media.rs               HLS and DASH manifest parsing
  stream.rs              segment fetching, AES-128, stream resume
  category.rs            extension → folder
  throttle.rs            token bucket and speed measurement
crates/riptide-cli/      the `riptide` command
crates/riptide-daemon/   the `riptided` service, HTTP API and queue
extension/               Manifest V3 browser extension
web/                     the local web UI
docs/ANALYSIS.md         design rationale: why the engine is shaped this way
```

```bash
cargo test --workspace
```

126 tests, covering the chunk-tiling invariants, out-of-order segment reassembly, resume
validation, filename sanitisation (including traversal attempts in server-supplied names),
manifest parsing, AES-128 round trips, rate limiting under a paused clock, and API
authentication. No test touches the network.

Verified end to end: an eight-connection download is SHA-256 identical to the same file fetched
over one connection; a download killed at 18% resumes to the same hash; and a 64-segment 1080p
HLS stream produces a valid 502 MB transport stream with correct packet sync at both ends.

## What it does not do yet

Being straight about the gaps:

- **FTP** — the engine is HTTP/HTTPS only so far.
- **Automatic link recapture.** When a signed URL expires Riptide says so clearly, but does not
  yet ask the browser to go and capture a fresh one.
- **Site grabber** — no crawler for pulling every linked file off a page.
- **ZIP preview** — fetching the central directory first to show a ZIP's contents mid-download
  is a good trick and is not implemented.
- **Media conversion** — no transcoding. Stream segments are concatenated, which gives a
  playable file for fMP4 and MPEG-TS but is not a remux.
- **Live streams** are detected and refused rather than recorded.
- **Scheduler** — no "start at 2am" yet; the queue is manual.
- **Firefox** — the extension is Chromium-only for now. The APIs it uses are standard, so a
  port is mostly manifest work.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Briefly: sign your commits off
([DCO](https://developercertificate.org/)), keep `cargo test`, `cargo clippy` and `cargo fmt`
clean, and **never contribute code derived from a proprietary program**.

A few things are settled decisions and will not be merged: DRM circumvention, anything touching
another program's licensing, kernel drivers, broader extension permissions, and telemetry.

## Licence and legal

Riptide is licensed under the **GNU General Public License, version 3 or later** — see
[LICENSE](LICENSE). Dependency licences are inventoried in
[THIRD-PARTY-LICENSES.md](THIRD-PARTY-LICENSES.md) and enforced in CI by `cargo deny`.

**Riptide is an independent project, not affiliated with, endorsed by, or derived from Tonec
FZE or Internet Download Manager.** It contains no code, resources or data from any proprietary
download manager. [docs/ANALYSIS.md](docs/ANALYSIS.md) is a design-rationale document recording
why the engine is built the way it is; it describes behaviour, not anyone's source code.

Riptide **does not circumvent DRM** — Widevine, PlayReady and FairPlay are detected and refused —
and does not interact with any other software's licensing or activation.

Riptide is a general-purpose file transfer tool, like `curl`, `wget` or a web browser. **You are
responsible for what you download**, and for respecting the law and the terms of the sites you
download from.

Full notices, including trademarks and cryptography export, are in [NOTICE.md](NOTICE.md).
