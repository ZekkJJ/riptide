# Legal notices

Riptide is free software licensed under the **GNU General Public License, version 3 or
later**. The full text is in [LICENSE](LICENSE). Third-party components and their licences
are listed in [THIRD-PARTY-LICENSES.md](THIRD-PARTY-LICENSES.md).

Nothing on this page is legal advice.

---

## Independence

**Riptide is an independent project. It is not affiliated with, authorised by, endorsed by,
sponsored by, or derived from Tonec FZE or Internet Download Manager.**

Riptide contains no source code, binary code, resources, icons, strings or data files from
Internet Download Manager or any other proprietary download manager. It was written from
scratch in Rust.

[docs/ANALYSIS.md](docs/ANALYSIS.md) is a design-rationale document. It records research into
how download managers solve particular problems — including publicly observable facts about
how Internet Download Manager behaves — in order to explain why Riptide's engine is built the
way it is. It describes behaviour; it is not a port, and it contains no transcribed or
translated code.

## Trademarks

All trademarks are the property of their respective owners and are used here nominatively,
only to identify the products discussed.

- *Internet Download Manager* and *IDM* are trademarks of Tonec FZE.
- *Windows* and *Microsoft Edge* are trademarks of Microsoft Corporation.
- *Chrome* and *Android* are trademarks of Google LLC.
- *Firefox* is a trademark of the Mozilla Foundation.
- *macOS* is a trademark of Apple Inc.
- *Widevine* is a trademark of Google LLC; *PlayReady* of Microsoft Corporation; *FairPlay*
  of Apple Inc.

Use of these names does not imply any affiliation with or endorsement by their owners.

The name "Riptide" and the Riptide logo are not claimed as registered trademarks. You may
redistribute modified versions under the GPL; please do not present a modified build as the
official one in a way that would mislead users about its origin.

## Digital rights management

**Riptide does not circumvent digital rights management, and will not.**

It detects Widevine, PlayReady and FairPlay protection in HLS and DASH manifests and refuses
to download such streams, with a message saying so. It supports plain AES-128 HLS, where the
decryption key is served in the clear over HTTP to any client that requests it, as specified
in RFC 8216 and as handled by ffmpeg, VLC and every standards-compliant player.

Pull requests implementing DRM circumvention will be closed without merge. See
[CONTRIBUTING.md](CONTRIBUTING.md).

## Licensing and activation of other software

Riptide does not interact with, modify, patch, emulate, bypass or interfere with the
licensing, activation, registration or trial period of any other software. Contributions
doing so will not be accepted.

## Intended use, and your responsibility

Riptide is a general-purpose tool for transferring files over HTTP, HTTPS and adaptive
streaming protocols. Like a web browser, `curl`, `wget` or `ffmpeg`, it has substantial
legitimate uses and no feature designed to facilitate infringement: it ships no index of
content, no piracy-oriented site list, no DRM circumvention and no anonymity or
detection-evasion features.

**You are responsible for what you download.** Downloading material you do not have the right
to copy may be unlawful where you live, and may breach the terms of service of the site you
download it from. Riptide's authors do not condone infringement and accept no responsibility
for how the software is used.

## Cryptography

Riptide links the pure-Rust `aes` and `cbc` crates to decrypt AES-128 HLS segments, and
`rustls` for TLS. It is published as open-source software; in the United States this is
eligible for the License Exception TSU / publicly-available-encryption notification path
under the Export Administration Regulations. Import, export, possession and use of encryption
software may be restricted in some jurisdictions. **Check the law where you are before
downloading, using or redistributing this software.**

## No warranty

As stated at greater length in sections 15 and 16 of the GPL:

> THERE IS NO WARRANTY FOR THE PROGRAM, TO THE EXTENT PERMITTED BY APPLICABLE LAW.

Riptide writes files to your disk and makes network requests on your behalf. Review what it
does before running it on anything you cannot afford to lose.

## Reporting a problem

- Security issues: see [SECURITY.md](SECURITY.md). Please do not open a public issue first.
- Licensing, trademark or other legal concerns: open an issue titled `legal:` and the
  maintainers will respond. If you believe something here infringes your rights, say what and
  why, and it will be removed or corrected while the question is resolved.
