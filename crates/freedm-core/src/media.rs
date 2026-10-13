//! HLS and DASH manifest parsing.
//!
//! Adaptive streams are not single files: the manifest names a set of quality variants, and
//! each variant is a list of segments to be fetched in order and concatenated. This module
//! turns a manifest into that ordered list so the rest of the engine can treat it as ordinary
//! work.
//!
//! Encryption is reported, not worked around. Plain AES-128 HLS, where the key is served in
//! the clear to any client that asks, is part of RFC 8216 and is handled. Streams protected by
//! a DRM system (Widevine, PlayReady, FairPlay) are detected and refused: breaking those is
//! both out of scope and not something this project will do.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamKind {
    Hls,
    Dash,
}

/// One quality level of an adaptive stream.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Variant {
    pub id: String,
    /// Declared peak bitrate, used to rank variants.
    pub bandwidth: Option<u64>,
    pub resolution: Option<(u32, u32)>,
    pub codecs: Option<String>,
    pub mime: Option<String>,
    /// For HLS, the media playlist to fetch next. For DASH the segments are already known.
    pub url: Option<String>,
    /// Audio-only variants are listed separately so a caller can mux video with audio.
    pub is_audio_only: bool,
}

impl Variant {
    /// A short human label such as `1920x1080 (5.2 Mbps)`.
    pub fn label(&self) -> String {
        let res = self.resolution.map(|(w, h)| format!("{w}x{h}"));
        let rate = self.bandwidth.map(|b| format!("{:.1} Mbps", b as f64 / 1_000_000.0));
        match (res, rate) {
            (Some(r), Some(b)) => format!("{r} ({b})"),
            (Some(r), None) => r,
            (None, Some(b)) => b,
            (None, None) => self.id.clone(),
        }
    }
}

/// One fetchable piece of a stream.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Segment {
    pub url: String,
    /// `(offset, length)` when several segments share one file.
    pub byte_range: Option<(u64, u64)>,
    pub duration: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Encryption {
    None,
    /// Standard HLS AES-128 with an openly served key.
    Aes128 { key_uri: String, iv: Option<String> },
    /// A DRM system. The engine refuses these.
    Drm { system: String },
}

impl Encryption {
    pub fn is_drm(&self) -> bool {
        matches!(self, Encryption::Drm { .. })
    }
}

/// An ordered segment list for one variant.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SegmentPlan {
    /// Initialisation segment that must be written before any media segment.
    pub init: Option<Segment>,
    pub segments: Vec<Segment>,
    pub encryption: Option<Encryption>,
    /// Sum of the segment durations, in seconds.
    pub duration: f64,
    /// A live stream has no end, so it cannot be fetched to completion.
    pub live: bool,
}

impl SegmentPlan {
    /// Reject a plan the engine must not attempt.
    pub fn check_supported(&self) -> Result<()> {
        if let Some(enc) = &self.encryption {
            if let Encryption::Drm { system } = enc {
                return Err(Error::Manifest {
                    kind: "stream",
                    detail: format!("the stream is protected by {system} DRM and cannot be downloaded"),
                });
            }
        }
        if self.segments.is_empty() {
            return Err(Error::Manifest { kind: "stream", detail: "no segments in manifest".into() });
        }
        Ok(())
    }

    pub fn total_segments(&self) -> usize {
        self.segments.len() + usize::from(self.init.is_some())
    }
}

/// What a manifest turned out to describe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Manifest {
    /// An HLS master playlist: variants must be fetched to get their segment lists.
    HlsMaster { variants: Vec<Variant> },
    /// An HLS media playlist: segments are known already.
    HlsMedia { plan: SegmentPlan },
    /// A DASH MPD, where variants and their segments come together.
    Dash { streams: Vec<(Variant, SegmentPlan)> },
}

impl Manifest {
    pub fn kind(&self) -> StreamKind {
        match self {
            Manifest::Dash { .. } => StreamKind::Dash,
            _ => StreamKind::Hls,
        }
    }
}

/// Detect and parse a manifest from its bytes.
pub fn parse(body: &str, base_url: &str) -> Result<Manifest> {
    let trimmed = body.trim_start_matches('\u{feff}').trim_start();
    if trimmed.starts_with("#EXTM3U") {
        return parse_hls(trimmed, base_url);
    }
    if trimmed.starts_with('<') && trimmed.contains("MPD") {
        return parse_dash(trimmed, base_url).map(|streams| Manifest::Dash { streams });
    }
    Err(Error::Manifest { kind: "stream", detail: "not an HLS or DASH manifest".into() })
}

/// Whether a URL or media type looks like an adaptive manifest.
pub fn looks_like_manifest(url: &str, content_type: Option<&str>) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or(url).to_ascii_lowercase();
    if path.ends_with(".m3u8") || path.ends_with(".m3u") || path.ends_with(".mpd") {
        return true;
    }
    content_type.is_some_and(|ct| {
        let ct = ct.split(';').next().unwrap_or(ct).trim().to_ascii_lowercase();
        matches!(
            ct.as_str(),
            "application/vnd.apple.mpegurl"
                | "application/x-mpegurl"
                | "audio/mpegurl"
                | "audio/x-mpegurl"
                | "application/dash+xml"
        )
    })
}

// ---------------------------------------------------------------------------- HLS

fn parse_hls(body: &str, base_url: &str) -> Result<Manifest> {
    if body.contains("#EXT-X-STREAM-INF") {
        Ok(Manifest::HlsMaster { variants: parse_hls_master(body, base_url) })
    } else {
        Ok(Manifest::HlsMedia { plan: parse_hls_media(body, base_url)? })
    }
}

/// Parse a master playlist into its variants, video and audio alike.
pub fn parse_hls_master(body: &str, base_url: &str) -> Vec<Variant> {
    let mut variants = Vec::new();
    let mut pending: Option<Variant> = None;

    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let a = parse_attributes(attrs);
            pending = Some(Variant {
                id: a.get("NAME").cloned().unwrap_or_default(),
                bandwidth: a
                    .get("BANDWIDTH")
                    .or_else(|| a.get("AVERAGE-BANDWIDTH"))
                    .and_then(|v| v.parse().ok()),
                resolution: a.get("RESOLUTION").and_then(|v| parse_resolution(v)),
                codecs: a.get("CODECS").cloned(),
                mime: None,
                url: None,
                is_audio_only: false,
            });
        } else if let Some(attrs) = line.strip_prefix("#EXT-X-MEDIA:") {
            // Alternative renditions, which carry audio for video-only variants.
            let a = parse_attributes(attrs);
            if a.get("TYPE").map(String::as_str) == Some("AUDIO") {
                if let Some(uri) = a.get("URI") {
                    variants.push(Variant {
                        id: a.get("NAME").cloned().unwrap_or_else(|| "audio".into()),
                        bandwidth: None,
                        resolution: None,
                        codecs: None,
                        mime: None,
                        url: resolve(base_url, uri),
                        is_audio_only: true,
                    });
                }
            }
        } else if !line.starts_with('#') {
            if let Some(mut v) = pending.take() {
                v.url = resolve(base_url, line);
                if v.id.is_empty() {
                    v.id = v.label();
                }
                variants.push(v);
            }
        }
    }

    // Best quality first, so a caller taking `variants[0]` gets the obvious choice.
    variants.sort_by(|a, b| {
        b.is_audio_only
            .cmp(&a.is_audio_only)
            .reverse()
            .then(b.resolution.map(|(_, h)| h).cmp(&a.resolution.map(|(_, h)| h)))
            .then(b.bandwidth.cmp(&a.bandwidth))
    });
    variants
}

/// Parse a media playlist into an ordered segment list.
pub fn parse_hls_media(body: &str, base_url: &str) -> Result<SegmentPlan> {
    let mut plan = SegmentPlan { live: true, ..Default::default() };
    let mut next_duration = 0.0f64;
    let mut next_range: Option<(u64, u64)> = None;
    // Where the previous byte-range segment ended, for a bare `EXT-X-BYTERANGE:len`.
    let mut range_cursor = 0u64;

    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            next_duration =
                rest.split(',').next().unwrap_or("0").trim().parse().unwrap_or(0.0);
        } else if let Some(rest) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            next_range = parse_byterange(rest.trim(), range_cursor);
        } else if let Some(attrs) = line.strip_prefix("#EXT-X-MAP:") {
            let a = parse_attributes(attrs);
            if let Some(uri) = a.get("URI") {
                plan.init = resolve(base_url, uri).map(|url| Segment {
                    url,
                    byte_range: a.get("BYTERANGE").and_then(|r| parse_byterange(r, 0)),
                    duration: 0.0,
                });
            }
        } else if let Some(attrs) = line.strip_prefix("#EXT-X-KEY:") {
            plan.encryption = Some(parse_hls_key(&parse_attributes(attrs)));
        } else if line.starts_with("#EXT-X-SESSION-KEY:") {
            // A session key names the protection system without encrypting this playlist.
            let a = parse_attributes(line.trim_start_matches("#EXT-X-SESSION-KEY:"));
            let enc = parse_hls_key(&a);
            if enc.is_drm() {
                plan.encryption = Some(enc);
            }
        } else if line == "#EXT-X-ENDLIST" {
            plan.live = false;
        } else if !line.starts_with('#') {
            if let Some(url) = resolve(base_url, line) {
                if let Some((off, len)) = next_range {
                    range_cursor = off + len;
                }
                plan.segments.push(Segment {
                    url,
                    byte_range: next_range.take(),
                    duration: next_duration,
                });
                plan.duration += next_duration;
                next_duration = 0.0;
            }
        }
    }
    Ok(plan)
}

fn parse_hls_key(a: &std::collections::HashMap<String, String>) -> Encryption {
    let method = a.get("METHOD").map(String::as_str).unwrap_or("NONE");
    if method == "NONE" {
        return Encryption::None;
    }
    // A key served over a DRM key system, rather than as raw bytes over HTTP.
    if let Some(fmt) = a.get("KEYFORMAT") {
        let f = fmt.to_ascii_lowercase();
        // KEYFORMAT names the system either as a URN carrying the system UUID, or by name.
        if let Some(system) = drm_system_name(&f) {
            return Encryption::Drm { system };
        }
        if f.starts_with("com.apple.streamingkeydelivery") {
            return Encryption::Drm { system: "FairPlay".into() };
        }
    }
    if method.starts_with("SAMPLE-AES") {
        // SAMPLE-AES in practice means FairPlay.
        return Encryption::Drm { system: "SAMPLE-AES".into() };
    }
    match a.get("URI") {
        Some(uri) => Encryption::Aes128 { key_uri: uri.clone(), iv: a.get("IV").cloned() },
        None => Encryption::Drm { system: method.to_string() },
    }
}

/// `length[@offset]`, where a missing offset continues from the previous segment.
fn parse_byterange(spec: &str, cursor: u64) -> Option<(u64, u64)> {
    let spec = spec.trim().trim_matches('"');
    let mut parts = spec.splitn(2, '@');
    let len: u64 = parts.next()?.trim().parse().ok()?;
    let off = match parts.next() {
        Some(o) => o.trim().parse().ok()?,
        None => cursor,
    };
    Some((off, len))
}

fn parse_resolution(v: &str) -> Option<(u32, u32)> {
    let (w, h) = v.split_once('x')?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

/// Parse `KEY=VALUE,KEY="quoted,value"` attribute lists.
fn parse_attributes(s: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let mut key = String::new();
    let mut val = String::new();
    let mut in_key = true;
    let mut quoted = false;

    for c in s.chars() {
        match c {
            '=' if in_key && !quoted => in_key = false,
            '"' => quoted = !quoted,
            ',' if !quoted => {
                if !key.trim().is_empty() {
                    out.insert(key.trim().to_uppercase(), val.trim().to_string());
                }
                key.clear();
                val.clear();
                in_key = true;
            }
            _ if in_key => key.push(c),
            _ => val.push(c),
        }
    }
    if !key.trim().is_empty() {
        out.insert(key.trim().to_uppercase(), val.trim().to_string());
    }
    out
}

// ---------------------------------------------------------------------------- DASH

/// Parse an MPD into a variant plus segment list per representation.
pub fn parse_dash(xml: &str, base_url: &str) -> Result<Vec<(Variant, SegmentPlan)>> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut p = DashParser::new(base_url);

    loop {
        match reader.read_event() {
            Err(e) => return Err(Error::Manifest { kind: "DASH", detail: e.to_string() }),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());
                p.start(&name, attributes(&e));
            }
            Ok(Event::Empty(e)) => {
                // A self-closing element gets no matching End event, so its scope has to be
                // opened and closed here. Representations are routinely written this way.
                let name = local_name(e.name().as_ref());
                p.start(&name, attributes(&e));
                p.end(&name);
            }
            Ok(Event::Text(t)) => p.text(&t.unescape().unwrap_or_default()),
            Ok(Event::End(e)) => {
                let name = local_name(e.name().as_ref());
                p.end(&name);
            }
            _ => {}
        }
    }
    p.finish()
}

/// Accumulates inherited MPD state while walking the document.
///
/// DASH lets most addressing information be declared at the MPD, period, adaptation set or
/// representation level, with inner declarations overriding outer ones, so each scope keeps
/// its own slot and a representation reads whichever is nearest.
struct DashParser {
    request_url: String,
    mpd_base: Option<String>,
    dynamic: bool,
    drm: Option<String>,

    set_mime: Option<String>,
    set_template: Option<Template>,
    set_timeline: Vec<(u64, f64)>,

    rep: Option<Variant>,
    rep_template: Option<Template>,
    rep_timeline: Vec<(u64, f64)>,
    rep_base: Option<String>,
    rep_segment_urls: Vec<(String, Option<(u64, u64)>)>,
    rep_init: Option<(String, Option<(u64, u64)>)>,

    /// Which scope the next character data belongs to, set when a `BaseURL` opens.
    base_text: Option<Scope>,
    out: Vec<(Variant, SegmentPlan)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Mpd,
    Representation,
}

impl DashParser {
    fn new(request_url: &str) -> Self {
        Self {
            request_url: request_url.to_string(),
            mpd_base: None,
            dynamic: false,
            drm: None,
            set_mime: None,
            set_template: None,
            set_timeline: Vec::new(),
            rep: None,
            rep_template: None,
            rep_timeline: Vec::new(),
            rep_base: None,
            rep_segment_urls: Vec::new(),
            rep_init: None,
            base_text: None,
            out: Vec::new(),
        }
    }

    fn in_rep(&self) -> bool {
        self.rep.is_some()
    }

    fn start(&mut self, name: &str, attrs: std::collections::HashMap<String, String>) {
        match name {
            "MPD" => self.dynamic = attrs.get("type").map(String::as_str) == Some("dynamic"),
            "BaseURL" => {
                self.base_text =
                    Some(if self.in_rep() { Scope::Representation } else { Scope::Mpd });
            }
            "ContentProtection" => {
                if let Some(system) = drm_system(&attrs) {
                    self.drm = Some(system);
                }
            }
            "AdaptationSet" => {
                self.set_mime = attrs.get("mimeType").cloned();
                self.set_template = None;
                self.set_timeline.clear();
            }
            "Representation" => {
                let mime = attrs.get("mimeType").cloned().or_else(|| self.set_mime.clone());
                self.rep = Some(Variant {
                    id: attrs.get("id").cloned().unwrap_or_default(),
                    bandwidth: attrs.get("bandwidth").and_then(|v| v.parse().ok()),
                    resolution: match (attrs.get("width"), attrs.get("height")) {
                        (Some(w), Some(h)) => match (w.parse(), h.parse()) {
                            (Ok(w), Ok(h)) => Some((w, h)),
                            _ => None,
                        },
                        _ => None,
                    },
                    codecs: attrs.get("codecs").cloned(),
                    is_audio_only: mime.as_deref().is_some_and(|m| m.starts_with("audio/")),
                    mime,
                    url: None,
                });
                self.rep_template = None;
                self.rep_timeline.clear();
                self.rep_base = None;
                self.rep_segment_urls.clear();
                self.rep_init = None;
            }
            "SegmentTemplate" => {
                let t = Template {
                    media: attrs.get("media").cloned(),
                    initialization: attrs.get("initialization").cloned(),
                    start_number: attrs
                        .get("startNumber")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(1),
                    duration: attrs.get("duration").and_then(|v| v.parse().ok()),
                    timescale: attrs.get("timescale").and_then(|v| v.parse().ok()).unwrap_or(1.0),
                };
                if self.in_rep() {
                    self.rep_template = Some(t);
                } else {
                    self.set_template = Some(t);
                }
            }
            "S" => {
                // One SegmentTimeline entry: t = start time, d = duration, r = extra repeats.
                let d: f64 = attrs.get("d").and_then(|v| v.parse().ok()).unwrap_or(0.0);
                let t: u64 = attrs.get("t").and_then(|v| v.parse().ok()).unwrap_or(0);
                let r: i64 = attrs.get("r").and_then(|v| v.parse().ok()).unwrap_or(0);
                let in_rep = self.in_rep();
                let target =
                    if in_rep { &mut self.rep_timeline } else { &mut self.set_timeline };
                // Only the first entry needs an explicit time; later ones follow on.
                let mut time = if t > 0 {
                    t
                } else {
                    target.last().map(|&(lt, ld)| lt + ld as u64).unwrap_or(0)
                };
                // A negative repeat count means "until the end of the period", which only a
                // live stream uses and which cannot be enumerated; take it as one entry.
                for _ in 0..=r.max(0) {
                    target.push((time, d));
                    time += d as u64;
                }
            }
            "Initialization" | "SegmentURL" => {
                let range = attrs
                    .get("range")
                    .or_else(|| attrs.get("mediaRange"))
                    .and_then(|r| parse_dash_range(r));
                let uri = attrs.get("sourceURL").or_else(|| attrs.get("media")).cloned();
                if name == "Initialization" {
                    // With no URI the range addresses the representation's own BaseURL.
                    if uri.is_some() || range.is_some() {
                        self.rep_init = Some((uri.unwrap_or_default(), range));
                    }
                } else if let Some(u) = uri {
                    self.rep_segment_urls.push((u, range));
                }
            }
            _ => {}
        }
    }

    fn text(&mut self, value: &str) {
        let Some(scope) = self.base_text.take() else { return };
        let value = value.trim();
        if value.is_empty() {
            return;
        }
        match scope {
            Scope::Representation => self.rep_base = Some(value.to_string()),
            Scope::Mpd => self.mpd_base = Some(value.to_string()),
        }
    }

    fn end(&mut self, name: &str) {
        if name == "BaseURL" {
            self.base_text = None;
            return;
        }
        if name != "Representation" {
            return;
        }
        let Some(mut variant) = self.rep.take() else { return };

        let base =
            join_bases(&self.request_url, self.mpd_base.as_deref(), self.rep_base.as_deref());
        let template = self.rep_template.take().or_else(|| self.set_template.clone());
        let timeline = if self.rep_timeline.is_empty() {
            self.set_timeline.clone()
        } else {
            std::mem::take(&mut self.rep_timeline)
        };

        let mut plan = build_dash_plan(
            &variant,
            &base,
            template.as_ref(),
            &timeline,
            &self.rep_segment_urls,
            self.rep_init.as_ref(),
            self.rep_base.as_deref(),
        );
        plan.live = self.dynamic;
        plan.encryption = self.drm.clone().map(|system| Encryption::Drm { system });
        if variant.id.is_empty() {
            variant.id = variant.label();
        }
        self.out.push((variant, plan));
    }

    fn finish(mut self) -> Result<Vec<(Variant, SegmentPlan)>> {
        if self.out.is_empty() {
            return Err(Error::Manifest {
                kind: "DASH",
                detail: "no representations found in the manifest".into(),
            });
        }
        // Video before audio, then best quality first.
        self.out.sort_by(|a, b| {
            a.0.is_audio_only
                .cmp(&b.0.is_audio_only)
                .then(b.0.resolution.map(|(_, h)| h).cmp(&a.0.resolution.map(|(_, h)| h)))
                .then(b.0.bandwidth.cmp(&a.0.bandwidth))
        });
        Ok(self.out)
    }
}

#[derive(Debug, Clone)]
struct Template {
    media: Option<String>,
    initialization: Option<String>,
    start_number: u64,
    duration: Option<f64>,
    timescale: f64,
}

#[allow(clippy::too_many_arguments)]
fn build_dash_plan(
    variant: &Variant,
    base: &str,
    template: Option<&Template>,
    timeline: &[(u64, f64)],
    segment_urls: &[(String, Option<(u64, u64)>)],
    init: Option<&(String, Option<(u64, u64)>)>,
    rep_base: Option<&str>,
) -> SegmentPlan {
    let mut plan = SegmentPlan::default();
    let bandwidth = variant.bandwidth.unwrap_or(0);

    // An explicit SegmentList wins over a template.
    if !segment_urls.is_empty() {
        if let Some((uri, range)) = init {
            let url = if uri.is_empty() { base.to_string() } else { resolve(base, uri).unwrap_or_default() };
            plan.init = Some(Segment { url, byte_range: *range, duration: 0.0 });
        }
        for (uri, range) in segment_urls {
            if let Some(url) = resolve(base, uri) {
                plan.segments.push(Segment { url, byte_range: *range, duration: 0.0 });
            }
        }
        return plan;
    }

    if let Some(t) = template {
        if let Some(init_tpl) = &t.initialization {
            let expanded = expand(init_tpl, &variant.id, bandwidth, 0, 0);
            if let Some(url) = resolve(base, &expanded) {
                plan.init = Some(Segment { url, byte_range: None, duration: 0.0 });
            }
        }
        if let Some(media) = &t.media {
            if !timeline.is_empty() {
                // SegmentTimeline: one segment per entry, addressed by $Time$ or $Number$.
                for (i, &(time, d)) in timeline.iter().enumerate() {
                    let n = t.start_number + i as u64;
                    let expanded = expand(media, &variant.id, bandwidth, n, time);
                    if let Some(url) = resolve(base, &expanded) {
                        plan.segments.push(Segment {
                            url,
                            byte_range: None,
                            duration: d / t.timescale,
                        });
                    }
                }
            } else if let Some(d) = t.duration {
                // A fixed segment duration, so the count comes from the period length. Without
                // a declared period duration the caller must stop on the first 404, which is
                // how players handle this case too.
                let seg_secs = d / t.timescale;
                plan.segments = Vec::new();
                plan.duration = 0.0;
                // Emit a generous bounded run; the fetcher stops at the first missing segment.
                for i in 0..MAX_TEMPLATE_SEGMENTS {
                    let n = t.start_number + i;
                    let expanded = expand(media, &variant.id, bandwidth, n, 0);
                    if let Some(url) = resolve(base, &expanded) {
                        plan.segments.push(Segment { url, byte_range: None, duration: seg_secs });
                    }
                }
            }
        }
        plan.duration = plan.segments.iter().map(|s| s.duration).sum();
        return plan;
    }

    // No template and no list: a single-file representation addressed by its BaseURL.
    if let Some(rb) = rep_base {
        if let Some(url) = resolve(base, rb).or_else(|| Some(base.to_string())) {
            if let Some((_, range)) = init {
                plan.init = Some(Segment { url: url.clone(), byte_range: *range, duration: 0.0 });
            }
            plan.segments.push(Segment { url, byte_range: None, duration: 0.0 });
        }
    }
    plan
}

/// Upper bound on segments generated from a duration-only `SegmentTemplate`, which carries no
/// count. At a typical 4-second segment this covers about 11 hours.
const MAX_TEMPLATE_SEGMENTS: u64 = 10_000;

/// Expand `$Number$`, `$Time$`, `$RepresentationID$` and `$Bandwidth$`, including the
/// `%0Nd` width form such as `$Number%05d$`.
fn expand(template: &str, rep_id: &str, bandwidth: u64, number: u64, time: u64) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;

    while let Some(start) = rest.find('$') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('$') else {
            out.push('$');
            rest = after;
            continue;
        };
        let token = &after[..end];
        rest = &after[end + 1..];

        if token.is_empty() {
            // `$$` is a literal dollar sign.
            out.push('$');
            continue;
        }
        let (name, fmt) = match token.split_once('%') {
            Some((n, f)) => (n, Some(f)),
            None => (token, None),
        };
        let value: Option<u64> = match name {
            "Number" => Some(number),
            "Time" => Some(time),
            "Bandwidth" => Some(bandwidth),
            "RepresentationID" => {
                out.push_str(rep_id);
                continue;
            }
            _ => None,
        };
        match value {
            Some(v) => out.push_str(&format_width(v, fmt)),
            None => {
                // Unknown token: leave it alone rather than mangling the URL.
                out.push('$');
                out.push_str(token);
                out.push('$');
            }
        }
    }
    out.push_str(rest);
    out
}

/// Apply a `%05d`-style width to a number.
fn format_width(value: u64, fmt: Option<&str>) -> String {
    let Some(f) = fmt else { return value.to_string() };
    let digits: String = f.chars().take_while(|c| c.is_ascii_digit()).collect();
    match digits.parse::<usize>() {
        Ok(width) => format!("{value:0width$}"),
        Err(_) => value.to_string(),
    }
}

fn parse_dash_range(r: &str) -> Option<(u64, u64)> {
    // DASH ranges are "first-last", inclusive, unlike HLS which gives a length.
    let (a, b) = r.trim().split_once('-')?;
    let start: u64 = a.trim().parse().ok()?;
    let end: u64 = b.trim().parse().ok()?;
    if end < start {
        return None;
    }
    Some((start, end - start + 1))
}

/// Map a protection scheme identifier to a DRM system name.
///
/// `urn:mpeg:dash:mp4protection:2011` is deliberately not a match: it declares MPEG Common
/// Encryption without naming a key system, and streams whose key is served in the clear
/// declare it too, so treating it as DRM would refuse perfectly ordinary streams.
fn drm_system_name(scheme: &str) -> Option<String> {
    let id = scheme.trim().to_ascii_lowercase();
    if id.contains("edef8ba9-79d6-4ace-a3c8-27dcd51d21ed") || id.contains("widevine") {
        return Some("Widevine".into());
    }
    if id.contains("9a04f079-9840-4286-ab92-e65be0885f95") || id.contains("playready") {
        return Some("PlayReady".into());
    }
    if id.contains("94ce86fb-07ff-4f43-adb8-93d2fa968ca2") || id.contains("fairplay") {
        return Some("FairPlay".into());
    }
    if id == "urn:mpeg:dash:mp4protection:2011" {
        return None;
    }
    if id.starts_with("urn:uuid:") {
        return Some(format!("an unrecognised DRM system ({id})"));
    }
    None
}

fn drm_system(attrs: &std::collections::HashMap<String, String>) -> Option<String> {
    drm_system_name(attrs.get("schemeIdUri")?)
}

fn local_name(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    s.rsplit(':').next().unwrap_or(&s).to_string()
}

fn attributes(e: &quick_xml::events::BytesStart<'_>) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for a in e.attributes().flatten() {
        let key = local_name(a.key.as_ref());
        let val = a.unescape_value().map(|v| v.to_string()).unwrap_or_default();
        out.insert(key, val);
    }
    out
}

/// Combine the request URL with any `BaseURL` elements, innermost last.
fn join_bases(request_url: &str, mpd_base: Option<&str>, rep_base: Option<&str>) -> String {
    let mut base = request_url.to_string();
    for part in [mpd_base, rep_base].into_iter().flatten() {
        // A representation BaseURL that names the media file itself must not become the base
        // for resolving that same file, so only directory-like parts extend the base.
        if part.ends_with('/') || Url::parse(part).is_ok() {
            if let Some(joined) = resolve(&base, part) {
                base = joined;
            }
        }
    }
    base
}

/// Resolve `reference` against `base`, returning `None` only if neither is a usable URL.
pub fn resolve(base: &str, reference: &str) -> Option<String> {
    if let Ok(absolute) = Url::parse(reference) {
        if absolute.scheme() != "" && reference.contains("://") {
            return Some(absolute.to_string());
        }
    }
    Url::parse(base).ok()?.join(reference).ok().map(|u| u.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = r#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud",NAME="English",URI="audio/en.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS="avc1.42c01e,mp4a.40.2"
360/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=5200000,RESOLUTION=1920x1080,CODECS="avc1.640028,mp4a.40.2"
1080/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1280x720
720/index.m3u8
"#;

    #[test]
    fn master_playlists_are_ranked_best_first() {
        let v = parse_hls_master(MASTER, "https://cdn.example.com/video/master.m3u8");
        let video: Vec<_> = v.iter().filter(|x| !x.is_audio_only).collect();
        assert_eq!(video.len(), 3);
        assert_eq!(video[0].resolution, Some((1920, 1080)));
        assert_eq!(video[1].resolution, Some((1280, 720)));
        assert_eq!(video[2].resolution, Some((640, 360)));
        // Relative variant URIs resolve against the playlist location.
        assert_eq!(video[0].url.as_deref(), Some("https://cdn.example.com/video/1080/index.m3u8"));
        assert_eq!(video[0].bandwidth, Some(5_200_000));
        assert_eq!(video[0].label(), "1920x1080 (5.2 Mbps)");
    }

    #[test]
    fn alternative_audio_renditions_are_captured_separately() {
        let v = parse_hls_master(MASTER, "https://cdn.example.com/video/master.m3u8");
        let audio: Vec<_> = v.iter().filter(|x| x.is_audio_only).collect();
        assert_eq!(audio.len(), 1);
        assert_eq!(audio[0].id, "English");
        assert_eq!(audio[0].url.as_deref(), Some("https://cdn.example.com/video/audio/en.m3u8"));
    }

    #[test]
    fn a_vod_media_playlist_becomes_an_ordered_segment_list() {
        let body = r#"#EXTM3U
#EXT-X-VERSION:7
#EXT-X-TARGETDURATION:4
#EXT-X-MAP:URI="init.mp4"
#EXTINF:4.000,
seg1.m4s
#EXTINF:4.000,
seg2.m4s
#EXTINF:2.500,
seg3.m4s
#EXT-X-ENDLIST
"#;
        let plan = parse_hls_media(body, "https://e.com/s/index.m3u8").unwrap();
        assert!(!plan.live, "EXT-X-ENDLIST marks a finished stream");
        assert_eq!(plan.init.as_ref().unwrap().url, "https://e.com/s/init.mp4");
        assert_eq!(plan.segments.len(), 3);
        assert_eq!(plan.segments[0].url, "https://e.com/s/seg1.m4s");
        assert_eq!(plan.segments[2].url, "https://e.com/s/seg3.m4s");
        assert!((plan.duration - 10.5).abs() < 1e-9);
        assert_eq!(plan.total_segments(), 4, "the init segment counts as work");
        plan.check_supported().unwrap();
    }

    #[test]
    fn a_live_playlist_is_flagged_rather_than_parsed_as_complete() {
        let body = "#EXTM3U\n#EXTINF:4.0,\na.ts\n";
        let plan = parse_hls_media(body, "https://e.com/l.m3u8").unwrap();
        assert!(plan.live, "without EXT-X-ENDLIST the stream is still going");
    }

    #[test]
    fn byte_range_segments_share_one_file_and_chain_their_offsets() {
        let body = r#"#EXTM3U
#EXTINF:4.0,
#EXT-X-BYTERANGE:1000@0
all.ts
#EXTINF:4.0,
#EXT-X-BYTERANGE:2000@1000
all.ts
#EXTINF:4.0,
#EXT-X-BYTERANGE:1500
all.ts
#EXT-X-ENDLIST
"#;
        let plan = parse_hls_media(body, "https://e.com/s/i.m3u8").unwrap();
        assert_eq!(plan.segments[0].byte_range, Some((0, 1000)));
        assert_eq!(plan.segments[1].byte_range, Some((1000, 2000)));
        // A bare length continues from where the previous range ended.
        assert_eq!(plan.segments[2].byte_range, Some((3000, 1500)));
    }

    #[test]
    fn plain_aes128_is_supported_but_drm_is_refused() {
        let aes = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x1234\n#EXTINF:4,\na.ts\n#EXT-X-ENDLIST\n";
        let plan = parse_hls_media(aes, "https://e.com/i.m3u8").unwrap();
        assert_eq!(
            plan.encryption,
            Some(Encryption::Aes128 { key_uri: "key.bin".into(), iv: Some("0x1234".into()) })
        );
        plan.check_supported().expect("an openly served AES-128 key is standard HLS");

        let wv = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\",URI=\"x\"\n#EXTINF:4,\na.ts\n#EXT-X-ENDLIST\n";
        let plan = parse_hls_media(wv, "https://e.com/i.m3u8").unwrap();
        assert!(plan.encryption.as_ref().unwrap().is_drm());
        let err = plan.check_supported().unwrap_err().to_string();
        assert!(err.contains("Widevine"), "{err}");
    }

    #[test]
    fn quoted_commas_in_attributes_do_not_split_the_list() {
        let a = parse_attributes(r#"BANDWIDTH=800000,CODECS="avc1.42c01e,mp4a.40.2",RESOLUTION=640x360"#);
        assert_eq!(a.get("CODECS").unwrap(), "avc1.42c01e,mp4a.40.2");
        assert_eq!(a.get("RESOLUTION").unwrap(), "640x360");
    }

    #[test]
    fn absolute_segment_urls_are_left_alone() {
        let body = "#EXTM3U\n#EXTINF:4,\nhttps://other.cdn/x/seg.ts\n#EXT-X-ENDLIST\n";
        let plan = parse_hls_media(body, "https://e.com/s/i.m3u8").unwrap();
        assert_eq!(plan.segments[0].url, "https://other.cdn/x/seg.ts");
    }

    const MPD_TEMPLATE: &str = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT30S">
  <Period>
    <AdaptationSet mimeType="video/mp4">
      <SegmentTemplate timescale="1000" initialization="$RepresentationID$/init.mp4"
                       media="$RepresentationID$/seg-$Number%04d$.m4s" startNumber="1">
        <SegmentTimeline>
          <S t="0" d="4000" r="2"/>
          <S d="2000"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v720" bandwidth="2400000" width="1280" height="720" codecs="avc1.64001f"/>
      <Representation id="v1080" bandwidth="5000000" width="1920" height="1080" codecs="avc1.640028"/>
    </AdaptationSet>
    <AdaptationSet mimeType="audio/mp4">
      <SegmentTemplate timescale="1000" initialization="a/init.mp4" media="a/seg-$Number$.m4s" startNumber="1" duration="4000"/>
      <Representation id="a128" bandwidth="128000" codecs="mp4a.40.2"/>
    </AdaptationSet>
  </Period>
</MPD>
"#;

    #[test]
    fn dash_segment_timeline_expands_with_repeats_and_number_padding() {
        let streams = parse_dash(MPD_TEMPLATE, "https://e.com/dash/manifest.mpd").unwrap();
        let (v, plan) = streams.iter().find(|(v, _)| v.id == "v1080").unwrap();
        assert_eq!(v.resolution, Some((1920, 1080)));
        assert!(!v.is_audio_only);

        // r="2" means three segments from that entry, plus the trailing one: four in total.
        assert_eq!(plan.segments.len(), 4);
        assert_eq!(plan.init.as_ref().unwrap().url, "https://e.com/dash/v1080/init.mp4");
        // $Number%04d$ must be zero padded, and $RepresentationID$ substituted.
        assert_eq!(plan.segments[0].url, "https://e.com/dash/v1080/seg-0001.m4s");
        assert_eq!(plan.segments[3].url, "https://e.com/dash/v1080/seg-0004.m4s");
        // 4s + 4s + 4s + 2s against a 1000 timescale.
        assert!((plan.duration - 14.0).abs() < 1e-6, "duration was {}", plan.duration);
        assert!(!plan.live);
        plan.check_supported().unwrap();
    }

    #[test]
    fn dash_variants_are_ranked_with_audio_last() {
        let streams = parse_dash(MPD_TEMPLATE, "https://e.com/dash/manifest.mpd").unwrap();
        assert_eq!(streams[0].0.id, "v1080");
        assert_eq!(streams[1].0.id, "v720");
        assert!(streams[2].0.is_audio_only, "audio representations sort last");
        assert_eq!(streams[2].0.id, "a128");
    }

    #[test]
    fn dash_segment_list_with_byte_ranges_is_parsed() {
        let mpd = r#"<MPD type="static"><Period><AdaptationSet mimeType="video/mp4">
          <Representation id="v1" bandwidth="1000000" width="640" height="360">
            <BaseURL>video.mp4</BaseURL>
            <SegmentList>
              <Initialization range="0-999"/>
              <SegmentURL media="video.mp4" mediaRange="1000-4999"/>
              <SegmentURL media="video.mp4" mediaRange="5000-9999"/>
            </SegmentList>
          </Representation>
        </AdaptationSet></Period></MPD>"#;
        let streams = parse_dash(mpd, "https://e.com/d/m.mpd").unwrap();
        let (_, plan) = &streams[0];
        // DASH ranges are inclusive first-last, so 1000-4999 is 4000 bytes at offset 1000.
        assert_eq!(plan.segments.len(), 2);
        assert_eq!(plan.segments[0].byte_range, Some((1000, 4000)));
        assert_eq!(plan.segments[1].byte_range, Some((5000, 5000)));
        assert_eq!(plan.init.as_ref().unwrap().byte_range, Some((0, 1000)));
    }

    #[test]
    fn dash_drm_is_detected_and_refused() {
        let mpd = r#"<MPD type="static"><Period><AdaptationSet mimeType="video/mp4">
          <ContentProtection schemeIdUri="urn:mpeg:dash:mp4protection:2011" value="cenc"/>
          <ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"/>
          <Representation id="v1" bandwidth="1" width="2" height="3">
            <SegmentTemplate media="s-$Number$.m4s" startNumber="1" duration="4" timescale="1"/>
          </Representation>
        </AdaptationSet></Period></MPD>"#;
        let streams = parse_dash(mpd, "https://e.com/m.mpd").unwrap();
        let (_, plan) = &streams[0];
        let err = plan.check_supported().unwrap_err().to_string();
        assert!(err.contains("Widevine"), "{err}");
    }

    #[test]
    fn common_encryption_alone_is_not_treated_as_drm() {
        // mp4protection is declared by cleartext-key streams too, so on its own it must not
        // cause a refusal.
        let mut attrs = std::collections::HashMap::new();
        attrs.insert("schemeIdUri".to_string(), "urn:mpeg:dash:mp4protection:2011".to_string());
        assert_eq!(drm_system(&attrs), None);
    }

    #[test]
    fn a_dynamic_mpd_is_flagged_as_live() {
        let mpd = r#"<MPD type="dynamic"><Period><AdaptationSet mimeType="video/mp4">
          <Representation id="v" bandwidth="1" width="2" height="3">
            <SegmentTemplate media="s-$Number$.m4s" startNumber="1" duration="2" timescale="1"/>
          </Representation></AdaptationSet></Period></MPD>"#;
        let streams = parse_dash(mpd, "https://e.com/m.mpd").unwrap();
        assert!(streams[0].1.live);
    }

    #[test]
    fn template_tokens_expand_correctly() {
        assert_eq!(expand("s-$Number$.ts", "r", 0, 42, 0), "s-42.ts");
        assert_eq!(expand("s-$Number%05d$.ts", "r", 0, 42, 0), "s-00042.ts");
        assert_eq!(expand("$RepresentationID$/x-$Time$.m4s", "v720", 0, 1, 8000), "v720/x-8000.m4s");
        assert_eq!(expand("b$Bandwidth$/s.ts", "r", 128000, 1, 0), "b128000/s.ts");
        // A literal dollar sign, and an unknown token left intact.
        assert_eq!(expand("a$$b", "r", 0, 1, 0), "a$b");
        assert_eq!(expand("x-$Unknown$.ts", "r", 0, 1, 0), "x-$Unknown$.ts");
    }

    #[test]
    fn manifests_are_recognised_by_url_and_media_type() {
        assert!(looks_like_manifest("https://e.com/a/master.m3u8?t=1", None));
        assert!(looks_like_manifest("https://e.com/a/manifest.mpd", None));
        assert!(looks_like_manifest("https://e.com/stream", Some("application/x-mpegURL")));
        assert!(looks_like_manifest("https://e.com/s", Some("application/dash+xml; charset=utf-8")));
        assert!(!looks_like_manifest("https://e.com/movie.mp4", Some("video/mp4")));
    }

    #[test]
    fn the_parser_dispatches_on_content() {
        assert!(matches!(parse(MASTER, "https://e.com/m.m3u8"), Ok(Manifest::HlsMaster { .. })));
        assert!(matches!(
            parse("#EXTM3U\n#EXTINF:1,\na.ts\n#EXT-X-ENDLIST\n", "https://e.com/m.m3u8"),
            Ok(Manifest::HlsMedia { .. })
        ));
        assert!(matches!(parse(MPD_TEMPLATE, "https://e.com/m.mpd"), Ok(Manifest::Dash { .. })));
        assert!(parse("just some text", "https://e.com/x").is_err());
        // A UTF-8 BOM must not defeat detection.
        assert!(parse("\u{feff}#EXTM3U\n#EXTINF:1,\na.ts\n", "https://e.com/m.m3u8").is_ok());
    }

    #[test]
    fn an_empty_plan_is_rejected() {
        let plan = SegmentPlan::default();
        assert!(plan.check_supported().is_err());
    }
}
