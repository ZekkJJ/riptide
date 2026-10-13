<div align="center">

# FreeDM

**A free, open source download manager.**
Multi-connection downloads, real resume, browser capture, and HLS/DASH video — with no licence,
no trial, and no kernel driver.

[![licence](https://img.shields.io/badge/licence-GPL--3.0--or--later-blue)](LICENSE)
[![rust](https://img.shields.io/badge/rust-1.82%2B-orange)](https://www.rust-lang.org)

</div>

---

## What it does

- **Dynamic multi-connection downloads.** A file is not cut into fixed parts. FreeDM starts with
  one range and, each time a connection frees up, carves a new segment out of whichever one has
  the most work left. A connection that slows down accumulates outstanding bytes and becomes the
  next thing to split, so one bad route cannot hold the whole download back.
- **No reassembly pass.** Every segment writes straight into its final position in one sparse
  file. When the last byte lands the download is finished — no temp directory, no concatenation
  step, no sitting at 100% while gigabytes are copied.
- **Resume that is actually safe.** Progress is checkpointed beside the file. Before reusing a
  partial download FreeDM re-probes and compares the `ETag`, `Last-Modified` and size, and
  refuses if the remote file changed, rather than splicing new bytes into stale ones.
- **Browser capture.** A Manifest V3 extension hands downloads from Chrome, Edge or Brave to
  FreeDM, carrying the referer and (optionally) the site's cookies so links from logged-in pages
  still work.
- **HLS and DASH.** Adaptive streams are parsed, quality variants listed, and segments fetched
  with look-ahead into a single file. Plain AES-128 is decrypted; DRM is detected and refused.
- **Queue, speed limits, categories, mirrors, proxies.** A local web UI to drive it all, and a
  CLI that works on its own.

Cross-platform: Windows, macOS and Linux. One static binary each, no installer, no admin rights.

## Quick start

```bash
git clone https://github.com/Bloxive/freedm
cd freedm
cargo build --release
```

Two binaries land in `target/release/`:

```bash
./target/release/freedm https://example.com/big-file.iso
```

```bash
./target/release/freedmd --open
```

`freedm` is the command line tool. `freedmd` is the background service: it runs the queue, opens
a web UI at <http://127.0.0.1:7739/>, and is what the browser extension talks to.

## Command line

```bash
freedm <url>                                   # download, 8 connections by default
freedm <url> -n 16 -o name.iso -d ~/Downloads  # connections, name, folder
freedm <url> -l 2M                             # cap at 2 MiB/s
freedm <url> --categorize                      # file into Video/, Audio/, Archives/, ...
freedm resume ./partly-downloaded.iso          # pick up where it stopped
freedm variants https://site/stream.m3u8       # list the quality levels
freedm https://site/stream.m3u8 --variant 1080 # fetch a stream as one file
freedm <url> --referer <page> --cookies "s=…"  # links behind a login
freedm <url> --mirror <url2> --mirror <url3>   # fall back between mirrors
freedm <url> --proxy socks5://127.0.0.1:1080
```

Interrupting with Ctrl-C leaves the `.fdmpart` and `.fdmstate` files in place; `freedm resume`
continues from there.

## Browser extension

1. Start the service: `freedmd`. It prints a connection token.
2. Open `chrome://extensions`, turn on **Developer mode**, choose **Load unpacked**, and select
   the `extension/` folder.
3. Open the extension's options and paste the token. Press **Save and test**.

Downloads the browser starts are then handed to FreeDM automatically, and
**right-click → Download with FreeDM** always works on any link.

Cookie capture for logged-in sites is a separate, optional permission you grant from the options
page. Without it FreeDM still downloads; some sites will just refuse links that need a session.

### A note on the local port

The extension reaches FreeDM over `http://127.0.0.1:7739`, which means any page in your browser
can send requests to it too. Two things prevent that mattering:

- Every API route requires the `X-FreeDM-Token` header. The token is generated per install,
  lives only in your config file, and is never served over HTTP.
- The web UI has the token inlined and is served with no CORS headers, so a cross-origin
  `fetch` of it is blocked by the browser from reading the response.

The listener binds `127.0.0.1` explicitly, so nothing is reachable from your network. If you
would rather not have a local port at all, use the CLI — it talks to no one.

## Configuration

`freedmd config-path` prints the location (`%APPDATA%\freedm\config.json` on Windows,
`~/.config/freedm/config.json` elsewhere). Everything is editable from the web UI's Settings
dialog, or directly:

| Setting | Meaning |
| --- | --- |
| `download_dir` | Where finished files go |
| `categorize` | File into `Video/`, `Audio/`, `Archives/`, … |
| `connections_per_download` | Segments per file (default 8) |
| `max_concurrent_downloads` | Files transferring at once (default 3) |
| `global_speed_limit` | Bytes per second across everything, 0 for unlimited |
| `proxy` | `http://host:8080` or `socks5://host:1080` |
| `capture_min_size` | Smallest response worth taking over from the browser |
| `capture_exclusions` | Host globs never captured |
| `categories` | Extension → folder mapping |

## Why eight connections, and not eighty

More connections stop helping once the link is saturated, and many servers rate-limit or
blacklist clients that open a lot of them at once — especially across several simultaneous
downloads. Eight is a good default. FreeDM will not split a range below 1 MiB, so small files
quietly use fewer.

If a server will not serve byte ranges, or will not state a length, FreeDM detects that during
the probe and uses a single connection rather than producing a corrupt file.

## How it is put together

```
crates/freedm-core/      the engine
  chunks.rs              dynamic work-stealing segmentation  (the heart)
  probe.rs               size, range support, filename, resume validators
  download.rs            connection pool, retries, mirrors, pause/resume
  storage.rs             sparse file, positional writes, no reassembly
  state.rs               resume state and the "did the file change?" check
  media.rs               HLS and DASH manifest parsing
  stream.rs              segment fetching, AES-128, stream resume
  category.rs            extension → folder
  throttle.rs            token bucket and speed measurement
crates/freedm-cli/       the `freedm` command
crates/freedm-daemon/    the `freedmd` service, HTTP API and queue
extension/               Manifest V3 browser extension
web/                     the local web UI
docs/ANALYSIS.md         how IDM works, and why FreeDM differs
```

```bash
cargo test --workspace
```

126 tests, covering the chunk-tiling invariants, out-of-order segment reassembly, resume
validation, filename sanitisation (including traversal attempts in server-supplied names),
manifest parsing, AES-128 round trips, rate limiting under a paused clock, and API
authentication.

## What it does not do yet

Being straight about the gaps:

- **FTP** — the engine is HTTP/HTTPS only so far.
- **Automatic link recapture.** When a signed URL expires FreeDM says so clearly, but does not
  yet ask the browser to go and capture a fresh one the way IDM does.
- **Site grabber** — no crawler for pulling every linked file off a page.
- **ZIP preview** — IDM's trick of fetching the central directory first to show you the contents
  mid-download is a good one and is not implemented.
- **Media conversion** — no transcoding. Stream segments are concatenated, which gives a
  playable file for fMP4 and MPEG-TS but is not a remux.
- **Live streams** are detected and refused rather than recorded.
- **Scheduler** — no "start at 2am" yet; the queue is manual.
- **Firefox** — the extension is Chromium-only for now; the API it uses is standard, so a port
  is mostly manifest work.

## Licence

GPL-3.0-or-later. See [LICENSE](LICENSE).

FreeDM is an independent, clean-room implementation. It is not affiliated with, derived from, or
endorsed by Tonec FZE or Internet Download Manager. [docs/ANALYSIS.md](docs/ANALYSIS.md) records
the architectural study behind it: what was examined, what was learned, and what was
deliberately done differently.
