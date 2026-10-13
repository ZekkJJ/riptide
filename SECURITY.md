# Security policy

## Reporting a vulnerability

**Please do not open a public issue for a security problem.**

Report it privately through GitHub's [private vulnerability
reporting](https://github.com/ZekkJJ/riptide/security/advisories/new) on this repository. If
that is unavailable to you, open an issue titled `security: request contact` with no details,
and a maintainer will arrange a private channel.

Please include what an attacker can achieve, the steps to reproduce it, and the version and
platform you tested. A proof of concept helps enormously.

You will get an acknowledgement within a few days. Riptide is a volunteer project with no paid
security team, so please be patient with the fix timeline. Credit is given in the advisory and
the changelog unless you would rather stay anonymous.

## Supported versions

Riptide is pre-1.0. Fixes land on `main` and in the next release; older releases are not
patched.

## What is in scope

The parts of Riptide that face something untrusted:

- **The local HTTP API** (`riptide-daemon`). It binds `127.0.0.1` only and every route
  requires the `X-Riptide-Token` secret. Anything that reaches the API without the token,
  leaks the token, or lets a web page drive the daemon, is in scope.
- **Filenames supplied by a server.** `Content-Disposition` values and URL path names are
  attacker controlled and are sanitised before touching the filesystem. A path traversal, an
  absolute path, a reserved device name, or any other way to write outside the download
  directory is in scope. There are tests for this, and a bypass of them is a real finding.
- **Response parsing.** HLS and DASH manifests, HTTP headers and resume-state files are all
  untrusted input. Crashes, hangs, unbounded memory growth or worse are in scope.
- **The browser extension.** Anything that lets a page other than the options page drive it,
  read the token, or exfiltrate cookies is in scope.
- **Resume state.** A `.rtstate` file that causes writes outside the intended file, or that
  silently corrupts output, is in scope.

## What is not in scope

- Downloading a file that turns out to be malicious. Riptide transfers bytes; it is not an
  antivirus and does not inspect content.
- Someone with local access reading the token out of the config file. The token defends
  against web pages, not against a local attacker who has already won.
- Rate limits, bans or blocks applied by a site you download from.
- Scanner output with no demonstrated impact.

## Design notes relevant to security

- The daemon binds the loopback interface explicitly and is never exposed to the network.
- The web UI carries the token inline and is served with **no** CORS headers, so a
  cross-origin `fetch` cannot read it. The API's CORS preflight is permissive on purpose: the
  request behind it must still carry the secret, so answering the preflight gives nothing away.
- The token comparison is constant-time.
- Only `http`, `https` and `ftp` URLs are accepted; other schemes are rejected before reaching
  the engine.
- Riptide ships no kernel component and needs no administrator rights.
- There is no telemetry. Riptide makes no network request you did not ask for.
