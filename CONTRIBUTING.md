# Contributing to Riptide

Thanks for considering it. Bug reports, tests and documentation are as welcome as code.

---

## The one rule that matters most

**Riptide is a clean-room implementation. Do not contribute code derived from any
proprietary program.**

Concretely, by submitting a contribution you confirm that:

- You did **not** decompile, disassemble or otherwise reverse engineer a proprietary download
  manager and transcribe, paraphrase or translate what you found into your contribution.
- Your contribution contains no source code, binary code, resources, icons, string tables or
  data files copied from Internet Download Manager or any other proprietary product.
- You have the right to contribute the code under the GPL — it is your own work, or it comes
  from a GPL-compatible source that you have credited.

Studying *behaviour* is fine and is how [docs/ANALYSIS.md](docs/ANALYSIS.md) was written:
reading a protocol specification, observing what a server or program does over the wire,
reading a published browser-extension manifest, or noting which files a product installs.
Copying *expression* — someone's actual code — is not, whatever route you took to see it.

If you are unsure whether something crosses the line, open an issue and ask before writing
the patch.

## Things that will not be merged

These are settled project decisions, not open questions:

- **DRM circumvention.** Widevine, PlayReady and FairPlay are detected and refused on
  purpose. Plain AES-128 HLS, whose key is served in the clear per RFC 8216, is supported and
  is a different thing.
- **Anything touching another program's licensing**, activation, registration or trial.
- **Kernel drivers or system-wide traffic interception.** Browser integration goes through
  browser extension APIs. The security trade-off is deliberate; see the analysis document.
- **Broadening the browser extension's permissions** beyond what a feature genuinely needs.
  `<all_urls>` stays optional.
- **Bundled lists of sites to download from**, or any feature whose main use is infringement.
- **Telemetry, analytics, or any network request the user did not ask for.** Riptide talks to
  the servers you point it at, and nothing else.

## Certificate of origin

Riptide uses the [Developer Certificate of Origin](https://developercertificate.org/) 1.1.
Sign off each commit:

```bash
git commit -s -m "Your message"
```

That appends a `Signed-off-by:` line, which certifies you wrote the patch or otherwise have
the right to submit it under the project's licence. Commits without it will be asked for a
sign-off before merge.

Use a name you are willing to stand behind publicly. Contributions stay under
GPL-3.0-or-later; there is no copyright assignment, and contributors keep their copyright.

## Getting set up

```bash
git clone https://github.com/ZekkJJ/riptide
cd riptide
cargo build
cargo test --workspace
```

Rust 1.82 or newer. No other toolchain is needed — TLS is `rustls`, so there is no OpenSSL
dependency, and the crypto is pure Rust.

To run the service and its web UI while developing:

```bash
cargo run -p riptide-daemon -- --open
```

The browser extension is plain JavaScript in `extension/`; load it unpacked from
`chrome://extensions` with Developer mode on.

## Before you open a pull request

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all
```

All three must be clean. CI runs them on Linux, macOS and Windows.

### Tests

New behaviour needs a test. The existing suite is a reasonable guide to the house style:
tests are named as sentences describing the property being asserted, and they assert
*behaviour* rather than restating the implementation.

Pay particular attention to these, because they are where a subtle bug costs a user their
download:

- **`chunks.rs`** — the chunk list must remain an exact tiling of `[0, total)` with no gaps
  or overlaps after any operation. Call `validate()` in new tests.
- **`storage.rs`** — segments are written out of order into one file; a change here can
  corrupt output in ways that only show up on large downloads.
- **`state.rs`** — resuming onto a file that changed on the server produces silent
  corruption. The validator comparison is not optional.

Tests must not depend on the network. Where a test needs a server that behaves badly, prefer
constructing the condition directly over reaching for a real host.

### Style

- `cargo fmt` settles formatting; don't argue with it.
- Comments explain **why**, not what. A comment that restates the line above it is noise; a
  comment recording why an obvious-looking simplification is wrong is worth a great deal.
  There are several of those in the engine — `Accept-Encoding: identity`, `truncate(false)`,
  the never-persisted `active` flag — and each exists because getting it wrong caused a real
  bug.
- Errors should tell the user what to do next. `Error::LinkExpired` says the link has expired
  and suggests recapturing it, rather than printing a status code.

## Reporting bugs

Include the Riptide version, your OS, the command or URL pattern (redact anything private),
and what you expected versus what happened. `-v` adds engine logs, which usually contain the
answer.

For a corrupted or stalled download, the `.rtstate` file beside the partial payload is plain
JSON and is the most useful single thing you can attach.

## Security

Do not open a public issue for a security problem. See [SECURITY.md](SECURITY.md).
