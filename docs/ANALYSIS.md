# Design notes: how download managers solve this, and what Riptide does

Riptide was written to do well what Internet Download Manager (IDM) does well, and to avoid
what it does badly. This document records the research behind those design decisions so that
future contributors understand *why* the engine is shaped the way it is, and do not
"simplify" away choices that are load-bearing.

It is a design rationale, not a porting guide. Riptide shares no code with IDM.

---

## Method, and what was and was not done

The study looked at **observable facts about a shipped product**: the names and sizes of the
files it installs, the configuration and data files it ships in plain text, the publicly
published browser extension and its manifest, the identifiers present in its binaries'
symbol and string tables, and its documented, user-visible behaviour.

What was **not** done, and must never be done by a contributor to this project:

- No IDM code was decompiled and transcribed, paraphrased, or translated into Rust.
- No IDM source, binary, resource, icon, string table or data file is included in, or
  distributed with, Riptide.
- Nothing in this project circumvents, disables, or interferes with IDM's licensing,
  activation, or trial period, and no patch or contribution doing so will be accepted.

Riptide's implementation is independent. Where this document says Riptide "does the same
thing", it means the two programs solve the same problem in a comparable way — as every
download manager, browser and `curl` must, because the behaviour is dictated by HTTP
(RFC 9110 §14 byte ranges), by HLS (RFC 8216) and by the DASH specification, not by anyone's
private design.

Facts quoted below — file names, a list of file extensions, a handful of short user-facing
status messages — are quoted for the purpose of analysis and commentary. They are statements
about how a program behaves. The creative expression of IDM is its code, and none of it is
here.

### Why studying a program this way is lawful

Riptide's contributors believe this work is squarely within the long-settled right to study
and reimplement the *behaviour* of a program:

- **Interfaces and behaviour are not copyrightable expression.** US law excludes from
  copyright any "procedure, process, system, method of operation" (17 U.S.C. §102(b)).
  Reimplementing what a program *does*, without copying how it is written, is the ordinary
  way compatible software gets built.
- **Reverse engineering to understand an unprotectable element has repeatedly been held
  fair use** in the United States — *Sega v. Accolade* (9th Cir. 1992) and *Sony v.
  Connectix* (9th Cir. 2000) being the canonical cases.
- **In the EU, the right is statutory and cannot be signed away.** Directive 2009/24/EC
  Article 5(3) permits a lawful user to observe, study and test a program to determine the
  ideas and principles behind it, and Article 8 makes contractual terms purporting to
  override that right null and void.
- **No technological protection measure was circumvented.** Nothing here concerns IDM's
  licensing or activation.

This is a good-faith summary of the reasoning, written by software engineers. It is not
legal advice, and it is not a warranty. See [NOTICE.md](../NOTICE.md).

---

## 1. What a download manager is actually made of

IDM installs around forty binaries with clearly separated jobs: a main application carrying
the UI and transfer engine, a network monitor, per-browser extensions and content handlers,
a DASH manifest parser, a site crawler, a media converter, a shell extension, an elevation
broker, a bundled OpenSSL — and, significantly, **two kernel-mode drivers**: a Windows
Filtering Platform callout driver and a legacy TDI filter driver.

Those drivers are the single most consequential design decision in the product. They let it
observe network traffic from *any* process, so it can capture downloads from a browser it has
no extension for. They are also why it needs administrator rights to install, why it can
break after a Windows update, and why a defect in it is a system-wide problem rather than an
application one.

**Riptide has no kernel component and will not accept one.** Browser integration happens
entirely through the browser's own extension APIs. That is strictly less powerful — Riptide
cannot capture traffic from an arbitrary third-party application — and strictly safer. It is
also why Riptide runs without administrator rights, and on macOS and Linux.

---

## 2. The transfer engine

### 2.1 Dynamic segmentation is the idea worth keeping

The engine's structure is visible in the identifiers its binary carries: a `Session` split
into HTTP and FTP implementations, a `Connection` likewise, and — the interesting part — a
`ChunksList` with operations named along the lines of *find the largest free chunk*, *decide
whether a new connection is needed*, *change a connection's bounds*, and *join a chunk with
its neighbour*.

That set of operations describes a specific strategy, and it is the right one. The program
does **not** split a file into *N* equal parts up front. It begins with the whole file as a
single range and, whenever a connection slot frees up, carves a new range out of the tail of
whichever range has the most work outstanding. The connection already streaming that tail is
told its end boundary has moved, and stops early.

This matters because the failure mode of static splitting is severe. With eight fixed parts,
the download takes as long as the slowest eighth: one connection routed badly or throttled,
and the other seven sit idle at 99% waiting for it. Dynamic splitting makes a slow connection
*accumulate* outstanding bytes, which automatically makes it the next thing to be split. The
load balancing falls out of the data structure.

**Riptide implements this strategy**, in [`chunks.rs`](../crates/riptide-core/src/chunks.rs),
independently written. Two refinements:

- The split divides the victim's **remaining** range, not its whole range, so a chunk that is
  90% complete is not cut at its midpoint.
- The chunk list is maintained as an exact tiling of `[0, total)`, with a `validate()`
  invariant asserted in tests and re-checked whenever state is loaded from disk. A gap or an
  overlap is a corrupt download; the invariant makes that unrepresentable rather than
  unlikely.

### 2.2 The reassembly pass, which Riptide does not have

IDM gives each chunk its own temporary file and concatenates them when the transfer ends. Its
own interface says so, warning the user that

> "If you have several physical drives on your computer, you should select different physical
> drives for temporary directory and 'Save To' folders for faster assembling"

and carrying a family of errors about failures "during assembling the file".

That design costs a complete extra read and an extra write of the whole file *after* the
download has already finished. It is why a large download appears to hang at 100%, why a
temporary directory has to be configured at all, and why moving the output to another drive
is slow.

**Riptide writes every segment straight into its final position in one sparse file**, using
positional writes (`seek_write` on Windows, `pwrite` on Unix). Those take `&File` rather than
`&mut File`, so every connection shares one handle with no lock and no shared cursor. When
the last byte lands, the file is complete; finishing is a rename. There is no assembly phase,
no temporary directory, and no second pass over the data.

The test `out_of_order_segment_writes_rebuild_the_exact_file` writes three segments in
reverse order through separate buffered writers and asserts the result is byte-identical to
the expected file. End to end, an eight-connection download is SHA-256 identical to the same
file fetched over one connection.

### 2.3 Resume state belongs with the file

IDM keeps per-chunk resume state in the Windows registry, separate from the partial data on
disk. That is why its partial downloads do not survive being moved to another machine, and
why it needs a feature to find "unreferenced and not deleted parts of old downloads".

**Riptide writes a small JSON file next to the payload** (`name.ext.rtstate`). Progress
travels with the file, can be inspected and repaired by hand, and cannot be orphaned from the
data it describes.

### 2.4 The resume check that actually matters

Resuming is easy. Resuming *safely* is the part that separates a working download manager
from one that silently produces corrupt files.

Before reusing partial data, Riptide re-probes the URL and compares the strong `ETag`,
`Last-Modified` and total size against what was recorded when the download began. If the
remote file changed, it refuses, because splicing fresh bytes into stale ones yields a file
that passes every casual check and is wrong. Weak validators (`W/"..."`) are deliberately
ignored: they promise semantic equivalence, not byte equality, so they are evidence of
nothing either way and must not block an otherwise sound resume.

Riptide also detects the case where a time-limited link has lapsed and the server answers
`200` with a login or error page instead of the file. Saving that as a `.mkv` is the
unhelpful outcome; Riptide reports `LinkExpired` and tells the user to fetch a fresh URL. IDM
goes one better here, reopening the page in the browser so its extension can capture a new
address automatically — a genuinely good idea that Riptide has not implemented yet.

### 2.5 Protocol details that are easy to get wrong

**`Accept-Encoding: identity` on every request.** This one is load-bearing. With transfer
compression in play, a server may report `Content-Length` for the *encoded* body while
`Range` offsets address the *decoded* one. Segment arithmetic then silently corrupts the
output. Asking for no compression keeps lengths and offsets talking about the same thing.
There is a test asserting the header is present.

**Probe with a ranged `GET`, not `HEAD`.** Many servers reject `HEAD`, report a different
length for it, or advertise `Accept-Ranges` on `HEAD` while ignoring `Range` on `GET`. A
`bytes=0-0` request answered with `206` and a `Content-Range` proves range support *and* the
total size in one trip, testing exactly what the engine depends on.

**Refuse a `200` reply to a ranged request with a non-zero start.** The server ignored the
range and is sending the file from byte zero; writing that at the segment's offset corrupts
the output. Riptide returns `ResumeUnsupported` rather than producing a broken file. This is
verified against a real endpoint that ignores ranges — it falls back to one connection and
produces the exact expected byte count.

**Mirrors.** Riptide's `UrlPool` keeps a failure count per URL and always hands out the
least-failed one, so a dead mirror drops out of rotation without being permanently
blacklisted.

**Retry everything that can be retried, including the probe.** An early version retried every
segment five times but ran the initial probe exactly once, so a single connection blip before
any bytes moved killed the download outright — the one step with no second chance. Both now
back off and retry, and only for genuinely transient faults: an expired link, a `404` or a
changed file will not fix itself by asking again.

### 2.6 Ideas worth stealing that Riptide has not built yet

A ZIP file's central directory lives at the *end* of the archive. By prioritising the chunk
containing it, a download manager can show the contents of a ZIP before the download
finishes. IDM does this. It is a lovely use of having random access to the byte range, and it
is on Riptide's list.

---

## 3. Browser integration, and how much access to ask for

IDM's Chrome extension requests, among others, `webRequest`, `declarativeNetRequest`,
`downloads`, `cookies`, `proxy`, `management`, `nativeMessaging` and `<all_urls>`, and injects
a content script into every frame of every page at `document_start`. Combined with the kernel
driver, it catches essentially everything.

For a download manager, `cookies` and host access are genuinely needed — a link from a
logged-in page will not fetch without the session cookie. But read access to every site, plus
the ability to change proxy settings and inspect other extensions, is a great deal to ask up
front.

**Riptide's extension** requests `downloads`, `cookies`, `contextMenus`, `storage` and
`notifications`, with host access to `http://127.0.0.1/*` only. `<all_urls>` is an *optional*
permission, requested from the options page by someone who wants cookie capture for
logged-in sites; without it everything still works, and some sites simply refuse. There is no
`proxy`, no `management`, no `webRequest` and no content script.

Interception uses `chrome.downloads.onCreated` rather than blocking `webRequest`, which
Manifest V3 removed. When a download matches the configured rules, the extension hands it to
Riptide **first** and only cancels the browser's own copy once Riptide has accepted it, so a
service that is stopped or misconfigured can never make a download vanish.

Riptide's extension talks to the local service over `127.0.0.1` with a per-install secret,
rather than through a native-messaging host that would need registry writes to register. The
trade-off — a local port exists — is handled in [`api.rs`](../crates/riptide-daemon/src/api.rs):
every route requires the `X-Riptide-Token` header, and the web UI that carries the token is
served with no CORS headers, so a hostile page cannot read it out.

---

## 4. Capture should be conservative

IDM ships a ninety-line list of URL patterns it must *not* capture: game assets, notification
sounds, web-app audio, browser component updates, Windows Update. That file is the
accumulated scar tissue of a download manager that grabs too eagerly and breaks web
applications.

The lesson Riptide takes is to **default to not capturing**. It takes over only known file
extensions above a size floor (256 KiB by default), excludes update hosts out of the box, and
always leaves an explicit right-click available for anything it declined. Being slightly too
quiet is a far better failure than hijacking a page's own network requests.

---

## 5. Streaming, and where the line is

Riptide parses HLS (RFC 8216) and DASH, including `$Number%05d$`-style template expansion,
`SegmentTimeline` repeat counts, byte-range segments, and alternative audio renditions. A
64-segment 1080p stream downloads to a valid 502 MB transport stream with correct packet sync
at both ends.

On encryption, the line is drawn deliberately and is not open for contribution:

- **Plain AES-128 HLS is supported.** The key is served in the clear over HTTP to any client
  that asks for it. It is a documented part of RFC 8216, it is how `ffmpeg` and VLC have
  always handled it, and the key is not a protection measure that anything has to be
  circumvented to obtain.
- **DRM is detected and refused.** Widevine, PlayReady and FairPlay are identified from the
  manifest and the download stops with a clear message. Riptide will not implement DRM
  circumvention, and pull requests adding it will be closed.
- **`urn:mpeg:dash:mp4protection:2011` alone is not treated as DRM**, because streams with
  cleartext keys declare it too; treating it as DRM would refuse ordinary content.

---

## 6. Summary

| | IDM | Riptide |
| --- | --- | --- |
| Segmentation | Dynamic work stealing | Same strategy; splits the *remaining* range, tiling invariant enforced |
| Output | Per-chunk temp files, reassembly pass | One sparse file, positional writes, no reassembly |
| Resume state | Windows registry | JSON beside the payload |
| Resume safety | Validates; detects expired links | Same, plus strong/weak validator distinction |
| Capture | Kernel WFP driver + extensions | Extension only |
| Extension grants | `<all_urls>`, `proxy`, `management`, … | Loopback only; `<all_urls>` optional |
| Platform | Windows | Windows, macOS, Linux |
| DRM | — | Detected and refused |
| Licence | Commercial | GPL-3.0-or-later |
| FTP, site grabber, ZIP preview, scheduler | Yes | Not yet |

The engine design worth keeping is the dynamic chunk list. The weaknesses worth avoiding are
a reassembly pass that doubles the I/O of every download, resume state welded to the host
operating system's registry, and a security posture that buys universal capture with a kernel
driver and a very broad browser grant.
