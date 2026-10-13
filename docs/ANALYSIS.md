# How Internet Download Manager works, and what FreeDM does instead

This is a architectural study of Internet Download Manager 6.43 (Tonec FZE), written to inform
a clean-room reimplementation. It records **what IDM does and how it is structured**, derived
from the shipped module layout, the symbol and string tables in its binaries, its configuration
and data files, and its published browser extension.

No IDM code was decompiled into source, translated, or copied. Nothing here concerns its
licensing or activation. The point of the exercise was to understand the *problem* IDM solves
well, so that a free implementation could solve the same problem — and, where IDM's approach
has aged badly, solve it better.

Analysis target:

```
IDMan.exe   6,201,712 bytes   PE32 x86
sha256      e3fc655a335e17a6fdd09a9b315291d0c8863180f0d801e5dd7433c9e56afbd8
```

---

## 1. Module layout

IDM is not one program. The install directory holds about forty binaries with clearly separated
jobs:

| Module | Role |
| --- | --- |
| `IDMan.exe` | Main application: MFC UI, queue, scheduler, and the whole transfer engine |
| `IDMNetMon*.dll` | Network monitor; the user-mode half of traffic interception |
| `idmwfp*.sys` | **Kernel-mode Windows Filtering Platform callout driver** |
| `idmtdi*.sys` | Legacy kernel-mode TDI filter driver (pre-WFP path) |
| `IDMVMPrs*.dll` | DASH/MPD manifest parser, built on libxml2 |
| `IDMGrHlp.exe` | Site grabber: crawls a site and collects links |
| `idmvconv.dll` | Media conversion |
| `IDMMsgHost.exe` | Native-messaging host bridging browser extensions to `IDMan.exe` |
| `IDMGCExt.crx`, `IDMEdgeExt.crx`, `idmmzcc3.xpi`, `IDMOpExt.nex` | Per-browser extensions |
| `idmcchandler*.dll`, `IDMIECC.dll` | Per-browser content/connection handlers |
| `IDMShellExt*.dll`, `downlWithIDM*.dll` | Explorer shell integration |
| `idmBroker.exe`, `MediumILStart.exe` | UAC elevation broker and medium-integrity launcher |
| `libssl.dll`, `libcrypto.dll` | OpenSSL 1.1.x (bundled, dated 2020) |

The single most significant structural fact: **IDM ships kernel drivers.** `idmwfp64.sys` is a
WFP callout driver, which lets IDM observe and divert network traffic system-wide, from any
process, with no browser cooperation. That is how IDM captures downloads from browsers it has
no extension for. It is also why IDM needs administrator rights to install, why it can break
after Windows updates, and why a kernel defect in it is a system-wide problem.

FreeDM has no kernel component and never will. Browser integration is done entirely through the
browser's own extension APIs. That is strictly less powerful — FreeDM cannot capture traffic
from an arbitrary third-party application — and strictly safer.

---

## 2. The transfer engine

The C++ symbol names in `IDMan.exe` lay the engine out plainly:

```
Session            HTTPSession / FTPSession
  InitSession, ReceiveData, CalcSpeed, GetSlowestConnection,
  TryStartNewOptionalConnections, CreateOptionalConnections,
  SaveDownloadedInRegistry, CheckFormat, set_fileSize

Connection         HTTPConnection / FTPConnection / ProxyConnection
  CheckConnect, OnChangeBounds, OnComplete, ProcessRecv,
  ReceiveData, SelectForRead, SetCompleted

ChunksList / ChunksListItem
  FindMaxFreeChunk, NeedNewConnection, CreateNewItem, JoinWithNext,
  JoinFile, AppendFile, InsertFromReg, EraseRegSubKey,
  AddAlternative, ApplyAlternative, CheckAlternative,
  FindChunkWithZipCentralDir
```

### 2.1 Dynamic segmentation is the core idea

The marketing phrase is "intelligent dynamic file segmentation". The symbols show what it
actually is:

- `ChunksList::FindMaxFreeChunk` — find the chunk with the most work outstanding.
- `ChunksList::NeedNewConnection` — decide whether another connection is warranted.
- `Connection::OnChangeBounds` — tell a connection already in flight that its end boundary has
  moved backwards.
- `ChunksListItem::JoinWithNext` — merge finished neighbours.

So IDM does **not** split a file into *N* equal parts up front. It starts with the whole file as
one chunk and, whenever a connection slot frees up, carves a new chunk out of the tail of
whichever chunk has the most bytes left. The connection that owned that tail is told to stop
early.

This is work stealing, and it is the right design, because the failure mode of static splitting
is severe: with eight fixed parts, the download takes as long as the slowest eighth. One
connection routed badly, or throttled, and the other seven sit idle at 99% waiting. Dynamic
splitting makes a slow connection *accumulate* outstanding bytes, which automatically makes it
the next steal target. `GetSlowestConnection` exists as an explicit helper, but the chunk-list
design already produces the behaviour for free.

**FreeDM implements exactly this**, in `crates/freedm-core/src/chunks.rs`. The steal splits the
*remaining* range of the victim rather than its whole range, so a chunk that is 90% done is not
cut at its midpoint. The chunk list is maintained as an exact tiling of `[0, total)` with a
`validate()` invariant checked in tests and on every state load.

### 2.2 Segment files and the reassembly pass

`JoinFile`, `AppendFile`, `set_fileName` per chunk, plus the user-facing strings:

> "An error has occurred during assembling the file"
> "IDM has received 'Access Denied' error while assembling file parts."
> "If you have several physical drives on your computer, you should select different physical
> drives for temporary directory and 'Save To' folders for faster assembling of downloaded files."

tell the story: **each chunk is written to its own temporary file, and at the end IDM
concatenates them all into the output.** That is a complete extra read and an extra write of the
whole file after the transfer is already finished. It is why a large IDM download appears to
hang at 100%, why IDM needs a configurable temp directory, why it warns about putting the temp
directory on a different physical drive, and why "assembling" has its own family of error
messages.

**FreeDM does not do this.** Every segment writes straight into its final position in one sparse
file, using positional writes (`seek_write` on Windows, `pwrite` on Unix) which take `&File`, so
all connections share one handle with no lock and no shared cursor. When the last byte lands the
file is complete; finishing is a rename. There is no assembly phase, no temp directory to
configure, and no second pass over the data.

The `storage::tests::out_of_order_segment_writes_rebuild_the_exact_file` test writes three
segments in reverse order through separate buffered writers and asserts the result is
byte-identical to the expected file.

### 2.3 Resume state

`SaveDownloadedInRegistry`, `ChunksList::InsertFromReg`, `ChunksList::EraseRegSubKey`: IDM keeps
per-chunk resume state **in the Windows registry**, separate from the temp files on disk. This
is why IDM downloads do not survive being moved to another machine, and why the orphan-cleanup
feature exists:

> "IDM has found unreferenced and not deleted parts of old downloads in IDM temporary folder"

FreeDM writes a small JSON state file next to the payload (`name.ext.fdmstate`), so progress
travels with the file, is inspectable, and cannot be orphaned from it.

### 2.4 Resume safety

IDM clearly validates resumes, and clearly has to handle the link-expiry case:

> "The web site sent a web page instead of a file when IDM requested this file second time.
> Probably this site uses temporary links and does not allow requesting the same address twice."
> "IDM is waiting for the new address to resume the file"
> "The data transfer has been interrupted and the server does not support 'resume'."

`CContentTypeChecker::CheckHTML` is the detector: an HTML body where a file was expected means a
signed URL lapsed, not a file to save. IDM's response is to reopen the page in the browser and
wait for the extension to capture a fresh URL — a genuinely good idea.

FreeDM detects the same condition (`Error::LinkExpired`) and says so with advice rather than
saving the error page as a `.mkv`. The automatic recapture handshake is not implemented yet; it
is listed as future work in the README.

FreeDM also refuses a resume when the remote file has changed, comparing strong `ETag`,
`Last-Modified` and size against what was recorded at probe time, and deliberately ignoring weak
(`W/`) ETags, which promise semantic equivalence rather than byte equality. Splicing fresh bytes
into stale ones produces a file that looks fine and is corrupt; this is the single highest-value
correctness check in a resumable downloader.

### 2.5 Protocol details worth copying

From the request-building strings in `IDMan.exe`:

```
Range: bytes=0-%I64d          64-bit offsets throughout
Accept-Encoding: identity     <- the important one
Accept-Encoding: gzip, deflate
If-Modified-Since: %s
Authorization: Basic %s
Proxy-Authorization:
Connection: Keep-Alive
Cookie: / Cookie2: %s / Referer: %s / User-Agent: %s
```

`Accept-Encoding: identity` is load-bearing and easy to get wrong. With transfer compression in
play, a server may report `Content-Length` for the *encoded* body while `Range` offsets address
the *decoded* one. Segment arithmetic then silently corrupts the output. Asking for no
compression keeps lengths and offsets talking about the same thing. FreeDM sets it on every
request and has a test asserting it.

Other details FreeDM adopted:

- **Probe with a ranged `GET`, not `HEAD`.** Many servers reject `HEAD`, report a different
  length for it, or advertise `Accept-Ranges` on `HEAD` while ignoring `Range` on `GET`. A
  `bytes=0-0` request that comes back `206` with a `Content-Range` proves range support *and*
  the total size in one trip, testing the exact thing the engine depends on.
- **Refuse a `200` reply to a ranged request with a non-zero start.** The server is sending the
  whole file from byte zero; writing that at the segment's offset corrupts the file. FreeDM
  returns `Error::ResumeUnsupported` rather than producing a broken download.
- **Mirrors.** `AddAlternative` / `ApplyAlternative` / `CheckAlternative` — FreeDM's `UrlPool`
  keeps a failure count per URL and always hands out the least-failed one, so a dead mirror
  drops out of rotation without being permanently blacklisted.
- **FTP.** IDM has a full state machine: `SendPasv`, `SendPort`, `SendRest` (resume), `SendSize`,
  `SendMDTM` (timestamp), `SendType`, and `SendTestSequences` to probe capabilities. FreeDM
  does not implement FTP yet; it is listed as future work.

### 2.6 One genuinely clever feature

`ChunksList::FindChunkWithZipCentralDir`, alongside `CDownload::OnZipPreview`: a ZIP file's
central directory lives at the *end* of the archive, so by prioritising the chunk containing it,
IDM can show you the contents of a ZIP before the download finishes. That is a lovely use of
having random access to the byte range. It is noted here as worth building; FreeDM does not have
it yet.

---

## 3. Browser integration

Two independent mechanisms, which is why IDM catches almost everything.

**The kernel driver.** `idmwfp*.sys` (WFP) and `idmtdi*.sys` (legacy TDI) let the user-mode
`IDMNetMon*.dll` see HTTP traffic from any process. This needs administrator rights and
survives having no browser extension at all.

**The extensions.** `IDMGCExt.crx`'s manifest requests:

```json
"permissions": [
  "scripting", "tabs", "cookies", "contextMenus", "webNavigation", "webRequest",
  "declarativeNetRequest", "downloads", "downloads.shelf", "downloads.ui",
  "management", "storage", "proxy", "nativeMessaging"
],
"host_permissions": ["<all_urls>"],
"content_scripts": [{ "js": ["content.js"], "matches": ["*://*/*"],
                      "all_frames": true, "run_at": "document_start" }]
```

It talks to `IDMMsgHost.exe` over native messaging (`"name": "com.tonec.idm"`). The content
script at `document_start` in all frames is the video grabber: it watches media elements and
blob URLs to find streams that never appear as ordinary downloads.

That is a very broad grant: read access to every site, the ability to change proxy settings, and
`management`. For a download manager, `cookies` and `<all_urls>` are genuinely needed — a link
from a logged-in page will not fetch without the session cookie — but it is a lot to ask up
front.

**FreeDM's extension** requests `downloads`, `cookies`, `contextMenus`, `storage`,
`notifications`, and host access *only* to `http://127.0.0.1/*`. `<all_urls>` is an **optional**
permission the user grants from the options page if they want cookie capture for logged-in
sites; without it everything still works, some sites just refuse. No `proxy`, no `management`,
no `webRequest`, no content script.

Interception uses `chrome.downloads.onCreated` rather than blocking `webRequest`, which
Manifest V3 removed. When a download matches the rules the service supplies, the extension hands
it to FreeDM *first* and only cancels the browser's copy once FreeDM has accepted it — so a
service that is down or misconfigured can never make a download disappear.

Instead of native messaging (which needs registry writes on Windows to register the host),
FreeDM's extension talks to the local service over `127.0.0.1` with a per-install secret. That
trade-off is documented in the README: it avoids an installer, but it means a local port exists,
so every API route requires the `X-FreeDM-Token` header, and the web UI is served with no CORS
headers so a hostile page cannot read the token out of it.

---

## 4. Other behaviour worth recording

**Default capture extensions** (from the string table) — a reasonable, battle-tested list that
FreeDM's default categories closely follow:

```
3GP 7Z AAC ACE AIF APK ARJ ASF AVI BIN BZ2 DOC DOCX EXE FLV GZ GZIP IMG ISO LZH
M4A M4V MKV MOV MP3 MP4 MPA MPE MPEG MPG MSI MSU OGG OGV PDF PLJ PPS PPT QT RA
RAR RM RMVB SEA SIT SITX TAR TIF TIFF WAV WEBM WMA WMV
```

**`defexclist.txt`** is a 90-line list of URL patterns IDM must *not* capture — Zynga game
assets, Skype notification sounds, WhatsApp Web audio, Widevine CDM downloads, Windows Update,
Opera's autoupdate. This is the accumulated scar tissue of a download manager that grabs too
eagerly and breaks web applications. The lesson FreeDM takes: **default to not capturing.**
FreeDM only captures known extensions above a size floor (256 KiB by default), excludes update
hosts out of the box, and leaves an explicit right-click always available.

**Streaming.** `IDMVMPrs64.dll` is a DASH parser built on libxml2, handling `SegmentTimeline`,
`SegmentTemplate`, `SegmentList` and `SegmentBase`. FreeDM implements DASH and HLS in
`media.rs`, including `$Number%05d$`-style template expansion, `SegmentTimeline` repeat counts,
HLS byte-range segments, and alternative audio renditions.

**Encryption.** FreeDM decrypts plain AES-128 HLS, where the key is served in the clear to any
client that asks and which is part of RFC 8216. It detects Widevine, PlayReady and FairPlay and
**refuses**, with a clear message. Note that `urn:mpeg:dash:mp4protection:2011` on its own is
*not* treated as DRM, since cleartext-key streams declare it too.

---

## 5. Summary of the differences

| | IDM | FreeDM |
| --- | --- | --- |
| Segmentation | Dynamic work stealing | Same approach, splits the *remaining* range |
| Output | Per-chunk temp files + reassembly pass | One sparse file, positional writes, no reassembly |
| Resume state | Windows registry | JSON file beside the payload |
| Resume safety | Validates; detects expired links | Same, plus strong/weak ETag distinction |
| Capture | Kernel WFP driver + extensions | Extension only |
| Extension grants | `<all_urls>`, `proxy`, `management`, … | Loopback only; `<all_urls>` optional |
| Interception | Blocking `webRequest` + native messaging | `downloads.onCreated` + loopback API with a token |
| Platform | Windows only | Windows, macOS, Linux |
| Price / licence | Commercial, 30-day trial | Free, GPL-3.0-or-later |
| FTP, site grabber, ZIP preview, media conversion | Yes | Not yet |

IDM's engine design is genuinely good, and the dynamic chunk list is the part worth keeping. Its
weaknesses are a reassembly pass that doubles the I/O of every download, resume state welded to
the Windows registry, and a security posture — a kernel driver plus a very broad browser
grant — that buys universal capture at a price not everyone would choose to pay.
