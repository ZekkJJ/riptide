//! Resource probing: size, range support, filename and resume validators.

use crate::error::{Error, Result};
use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, ACCEPT, ACCEPT_ENCODING, ACCEPT_RANGES, AUTHORIZATION,
    CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, COOKIE, ETAG,
    LAST_MODIFIED, RANGE, REFERER, USER_AGENT,
};
use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// Default identity string. Some CDNs serve different content, or refuse ranges, for clients
/// they do not recognise, so this mimics an ordinary browser.
pub const DEFAULT_USER_AGENT: &str = concat!(
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 ",
    "(KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36 FreeDM/",
    env!("CARGO_PKG_VERSION"),
);

/// Everything needed to re-issue a request for one resource.
///
/// This is the unit the browser extension hands over: without the original referer, cookies
/// and user agent, a link captured from a logged-in page will not fetch from outside the
/// browser.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestSpec {
    pub url: String,
    #[serde(default)]
    pub referer: Option<String>,
    #[serde(default)]
    pub cookies: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Extra headers, applied after the defaults so they can override them.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// HTTP Basic credentials for sites behind a login.
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    /// Mirrors to fall back on, tried in order when the primary URL fails.
    #[serde(default)]
    pub mirrors: Vec<String>,
}

impl RequestSpec {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            referer: None,
            cookies: None,
            user_agent: None,
            headers: BTreeMap::new(),
            username: None,
            password: None,
            mirrors: Vec::new(),
        }
    }

    pub fn with_referer(mut self, referer: impl Into<String>) -> Self {
        self.referer = Some(referer.into());
        self
    }

    pub fn with_cookies(mut self, cookies: impl Into<String>) -> Self {
        self.cookies = Some(cookies.into());
        self
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    /// Build the header set for a request, optionally for a byte range.
    ///
    /// `Accept-Encoding: identity` is deliberate and load-bearing. With compression in play a
    /// server may return `Content-Length` for the *encoded* body while `Range` offsets address
    /// the decoded one, so segment arithmetic silently corrupts the output. Asking for no
    /// transfer compression keeps byte offsets and lengths talking about the same thing.
    pub fn headers(&self, range: Option<(u64, Option<u64>)>) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            USER_AGENT,
            HeaderValue::from_str(self.user_agent.as_deref().unwrap_or(DEFAULT_USER_AGENT))
                .unwrap_or_else(|_| HeaderValue::from_static(DEFAULT_USER_AGENT)),
        );
        h.insert(ACCEPT, HeaderValue::from_static("*/*"));
        h.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));

        if let Some(r) = &self.referer {
            if let Ok(v) = HeaderValue::from_str(r) {
                h.insert(REFERER, v);
            }
        }
        if let Some(c) = &self.cookies {
            if let Ok(v) = HeaderValue::from_str(c) {
                h.insert(COOKIE, v);
            }
        }
        if let Some(user) = &self.username {
            let raw = format!("{}:{}", user, self.password.as_deref().unwrap_or(""));
            if let Ok(v) = HeaderValue::from_str(&format!("Basic {}", base64(raw.as_bytes()))) {
                h.insert(AUTHORIZATION, v);
            }
        }
        for (k, v) in &self.headers {
            if let (Ok(name), Ok(val)) = (k.parse::<HeaderName>(), HeaderValue::from_str(v)) {
                h.insert(name, val);
            }
        }
        if let Some((start, end)) = range {
            let spec = match end {
                Some(e) => format!("bytes={start}-{e}"),
                None => format!("bytes={start}-"),
            };
            if let Ok(v) = HeaderValue::from_str(&spec) {
                h.insert(RANGE, v);
            }
        }
        h
    }
}

/// What a probe learned about a resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceInfo {
    /// The URL actually served, after redirects. Segment requests use this so that each
    /// connection does not repeat the redirect chain.
    pub final_url: String,
    /// Total length, or `None` when the server would not say.
    pub total: Option<u64>,
    /// Whether the server honoured a byte range, which decides if segmentation is possible.
    pub supports_ranges: bool,
    pub filename: String,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

impl ResourceInfo {
    /// Whether this resource can be fetched by several connections at once.
    pub fn is_segmentable(&self) -> bool {
        self.supports_ranges && self.total.is_some_and(|t| t > 0)
    }

    /// Compare against a previous probe to decide whether bytes on disk are still valid.
    ///
    /// A strong validator settles it on its own. With no validator at all, a matching size is
    /// weak evidence but the best available, and is what every mainstream download manager
    /// relies on in that case.
    pub fn resume_conflict(&self, saved: &ResourceInfo) -> Option<String> {
        if let (Some(a), Some(b)) = (&self.etag, &saved.etag) {
            // A weak ETag ("W/...") does not promise byte equality, so ignore it here.
            if !a.starts_with("W/") && !b.starts_with("W/") && a != b {
                return Some(format!("ETag changed from {b} to {a}"));
            }
        }
        if let (Some(a), Some(b)) = (&self.last_modified, &saved.last_modified) {
            if a != b {
                return Some(format!("Last-Modified changed from {b} to {a}"));
            }
        }
        if let (Some(a), Some(b)) = (self.total, saved.total) {
            if a != b {
                return Some(format!("size changed from {b} to {a} bytes"));
            }
        }
        None
    }
}

/// Build a client with the connection behaviour the engine expects.
pub fn client(proxy: Option<&str>, timeout: Duration) -> Result<Client> {
    let mut b = Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(timeout)
        .pool_idle_timeout(Duration::from_secs(90))
        // Segment requests resolve redirects once during the probe and then address the final
        // URL, so a redirect mid-transfer is a change of content, not something to follow.
        .redirect(reqwest::redirect::Policy::limited(10));
    if let Some(p) = proxy {
        b = b.proxy(reqwest::Proxy::all(p)?);
    }
    Ok(b.build()?)
}

/// Ask the server what it will give us.
///
/// A single-byte ranged `GET` is used rather than `HEAD`, because plenty of servers either
/// reject `HEAD`, report a different length for it, or claim `Accept-Ranges` on `HEAD` while
/// ignoring `Range` on `GET`. Asking for `bytes=0-0` tests the thing we actually depend on:
/// a `206` with a `Content-Range` proves both range support and the total size in one trip.
pub async fn probe(client: &Client, spec: &RequestSpec) -> Result<ResourceInfo> {
    let resp = client
        .get(&spec.url)
        .headers(spec.headers(Some((0, Some(0)))))
        .send()
        .await?;

    let status = resp.status();
    if status == StatusCode::PARTIAL_CONTENT {
        let total = content_range_total(&resp);
        return finish(resp, total, true);
    }
    if status.is_success() {
        // The range was ignored, so the whole body is on its way. Its length, if given, is the
        // total size, but we cannot segment.
        let total = resp.headers().get(CONTENT_LENGTH).and_then(parse_u64);
        // Some servers advertise ranges but answer 200 to a range request only for small
        // bodies; trust the header as a secondary signal.
        let claims_ranges = resp
            .headers()
            .get(ACCEPT_RANGES)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("bytes"));
        return finish(resp, total, claims_ranges && total.is_some());
    }
    if status == StatusCode::RANGE_NOT_SATISFIABLE {
        // Falling back to an unranged request still tells us the size and filename.
        let resp = client.get(&spec.url).headers(spec.headers(None)).send().await?;
        if !resp.status().is_success() {
            return Err(Error::BadStatus { status: resp.status().as_u16(), url: spec.url.clone() });
        }
        let total = resp.headers().get(CONTENT_LENGTH).and_then(parse_u64);
        return finish(resp, total, false);
    }
    Err(Error::BadStatus { status: status.as_u16(), url: spec.url.clone() })
}

fn finish(resp: Response, total: Option<u64>, supports_ranges: bool) -> Result<ResourceInfo> {
    let final_url = resp.url().to_string();
    let headers = resp.headers();
    let content_type = header_string(headers, &CONTENT_TYPE);

    // A time-limited link that has lapsed typically answers 200 with a login or error page
    // rather than an error status. Treat an HTML body as an expired link unless the URL itself
    // asked for a page, so the caller can go and recapture instead of saving the error page.
    if let Some(ct) = &content_type {
        if is_html(ct) && !wants_html(&final_url) {
            return Err(Error::LinkExpired);
        }
    }

    let filename = resolve_filename(
        header_string(headers, &CONTENT_DISPOSITION).as_deref(),
        &final_url,
        content_type.as_deref(),
    );

    Ok(ResourceInfo {
        final_url,
        total,
        supports_ranges,
        filename,
        etag: header_string(headers, &ETAG),
        last_modified: header_string(headers, &LAST_MODIFIED),
        content_type,
    })
}

fn header_string(h: &HeaderMap, name: &HeaderName) -> Option<String> {
    h.get(name).and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string())
}

fn parse_u64(v: &HeaderValue) -> Option<u64> {
    v.to_str().ok()?.trim().parse().ok()
}

/// Pull the total size out of `Content-Range: bytes 0-0/12345`.
fn content_range_total(resp: &Response) -> Option<u64> {
    parse_content_range_total(resp.headers().get(CONTENT_RANGE)?.to_str().ok()?)
}

fn parse_content_range_total(value: &str) -> Option<u64> {
    // "bytes 0-0/12345", or "bytes 0-0/*" when the server does not know the total.
    let total = value.rsplit('/').next()?.trim();
    if total == "*" {
        None
    } else {
        total.parse().ok()
    }
}

fn is_html(content_type: &str) -> bool {
    let ct = content_type.to_ascii_lowercase();
    ct.starts_with("text/html") || ct.starts_with("application/xhtml")
}

/// Whether the URL looks like it was meant to fetch a page, in which case an HTML reply is the
/// expected answer rather than a sign of an expired link.
fn wants_html(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or(url).to_ascii_lowercase();
    path.ends_with(".html")
        || path.ends_with(".htm")
        || path.ends_with(".xhtml")
        || path.ends_with('/')
        || !path.rsplit('/').next().is_some_and(|s| s.contains('.'))
}

/// Decide the on-disk name, preferring what the server states over what the URL suggests.
pub fn resolve_filename(
    content_disposition: Option<&str>,
    url: &str,
    content_type: Option<&str>,
) -> String {
    if let Some(cd) = content_disposition {
        if let Some(name) = filename_from_disposition(cd) {
            return sanitize_filename(&name);
        }
    }
    if let Some(name) = filename_from_url(url) {
        let clean = sanitize_filename(&name);
        if !clean.is_empty() && clean != "download" {
            // Give an extensionless URL name a plausible suffix from the media type.
            if !clean.contains('.') {
                if let Some(ext) = extension_for_type(content_type) {
                    return format!("{clean}.{ext}");
                }
            }
            return clean;
        }
    }
    match extension_for_type(content_type) {
        Some(ext) => format!("download.{ext}"),
        None => "download".to_string(),
    }
}

/// Parse a `Content-Disposition` value, honouring RFC 5987 `filename*` over plain `filename`.
fn filename_from_disposition(value: &str) -> Option<String> {
    let mut plain: Option<String> = None;
    for part in split_disposition(value) {
        let Some((key, raw)) = part.split_once('=') else { continue };
        let key = key.trim().to_ascii_lowercase();
        let raw = raw.trim();
        if key == "filename*" {
            // ext-value: charset'language'pct-encoded
            let mut it = raw.splitn(3, '\'');
            let charset = it.next().unwrap_or("").to_ascii_lowercase();
            let _lang = it.next();
            if let Some(encoded) = it.next() {
                let bytes = percent_decode(encoded);
                let decoded = if charset == "iso-8859-1" {
                    bytes.iter().map(|&b| b as char).collect()
                } else {
                    String::from_utf8_lossy(&bytes).to_string()
                };
                if !decoded.trim().is_empty() {
                    // filename* wins outright, per RFC 6266.
                    return Some(decoded);
                }
            }
        } else if key == "filename" && plain.is_none() {
            let unquoted = unquote(raw);
            if !unquoted.trim().is_empty() {
                plain = Some(unquoted);
            }
        }
    }
    plain
}

/// Split on semicolons that are not inside a quoted string.
fn split_disposition(value: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for c in value.chars() {
        match c {
            '\\' if quoted && !escaped => {
                escaped = true;
                cur.push(c);
            }
            '"' if !escaped => {
                quoted = !quoted;
                cur.push(c);
                escaped = false;
            }
            ';' if !quoted => {
                parts.push(std::mem::take(&mut cur));
                escaped = false;
            }
            _ => {
                cur.push(c);
                escaped = false;
            }
        }
    }
    parts.push(cur);
    parts
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        t[1..t.len() - 1].replace("\\\"", "\"").replace("\\\\", "\\")
    } else {
        t.to_string()
    }
}

fn percent_decode(s: &str) -> Vec<u8> {
    percent_encoding::percent_decode_str(s).collect()
}

fn filename_from_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let last = parsed.path_segments()?.filter(|s| !s.is_empty()).next_back()?;
    let decoded = percent_encoding::percent_decode_str(last).decode_utf8_lossy().to_string();
    if decoded.trim().is_empty() {
        None
    } else {
        Some(decoded)
    }
}

fn extension_for_type(content_type: Option<&str>) -> Option<&'static str> {
    let ct = content_type?.split(';').next()?.trim().to_ascii_lowercase();
    Some(match ct.as_str() {
        "application/zip" | "application/x-zip-compressed" => "zip",
        "application/x-7z-compressed" => "7z",
        "application/x-rar-compressed" | "application/vnd.rar" => "rar",
        "application/gzip" | "application/x-gzip" => "gz",
        "application/x-tar" => "tar",
        "application/pdf" => "pdf",
        "application/epub+zip" => "epub",
        "application/vnd.android.package-archive" => "apk",
        "application/x-msdownload" | "application/vnd.microsoft.portable-executable" => "exe",
        "application/x-msi" | "application/x-ms-installer" => "msi",
        "application/x-iso9660-image" => "iso",
        "application/json" => "json",
        "application/octet-stream" => return None,
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/x-matroska" => "mkv",
        "video/quicktime" => "mov",
        "video/x-msvideo" => "avi",
        "video/mp2t" => "ts",
        "audio/mpeg" => "mp3",
        "audio/mp4" | "audio/x-m4a" => "m4a",
        "audio/ogg" => "ogg",
        "audio/flac" | "audio/x-flac" => "flac",
        "audio/wav" | "audio/x-wav" => "wav",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "text/plain" => "txt",
        _ => return None,
    })
}

/// Strip anything that would be unsafe or invalid as a file name.
///
/// Path separators, the Windows reserved characters, control characters and traversal
/// sequences all have to go: the name comes from a remote server and is about to be joined
/// onto a local directory.
pub fn sanitize_filename(name: &str) -> String {
    // Keep only the last path component, so "../../etc/passwd" or "C:\evil\x" cannot escape.
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let mut out: String = base
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' | '/' | '\\' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();

    // Windows discards trailing dots and spaces, which would make the saved name differ from
    // the one we report.
    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
    }
    let trimmed = out.trim_start().to_string();
    out = trimmed;

    if out.is_empty() || out == "." || out == ".." {
        return "download".to_string();
    }

    // Device names are reserved on Windows whatever the extension.
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
        "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        out = format!("_{out}");
    }

    // Leave room for the ".fdmpart" suffix and a " (12)" disambiguator within the usual
    // 255-byte component limit.
    const MAX: usize = 200;
    if out.len() > MAX {
        let ext = Path::new(&out)
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        let keep = MAX.saturating_sub(ext.len());
        let mut head: String = out.chars().take(keep).collect();
        // Trimming by chars can still exceed MAX bytes for wide scripts.
        while head.len() > keep {
            head.pop();
        }
        out = format!("{head}{ext}");
    }
    out
}

fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    for c in input.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if c.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if c.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downloads_always_ask_for_an_unencoded_body() {
        let h = RequestSpec::new("https://example.com/f.bin").headers(None);
        // Compression would decouple Content-Length and Range offsets, corrupting segments.
        assert_eq!(h.get(ACCEPT_ENCODING).unwrap(), "identity");
    }

    #[test]
    fn range_headers_are_formatted_both_ways() {
        let s = RequestSpec::new("https://example.com/f.bin");
        assert_eq!(s.headers(Some((100, Some(199)))).get(RANGE).unwrap(), "bytes=100-199");
        assert_eq!(s.headers(Some((100, None))).get(RANGE).unwrap(), "bytes=100-");
    }

    #[test]
    fn capture_context_is_carried_into_the_request() {
        let s = RequestSpec::new("https://cdn.example.com/v.mp4")
            .with_referer("https://site.example/watch?v=1")
            .with_cookies("session=abc; lang=en")
            .with_header("Origin", "https://site.example");
        let h = s.headers(None);
        assert_eq!(h.get(REFERER).unwrap(), "https://site.example/watch?v=1");
        assert_eq!(h.get(COOKIE).unwrap(), "session=abc; lang=en");
        assert_eq!(h.get("origin").unwrap(), "https://site.example");
    }

    #[test]
    fn basic_auth_is_encoded_correctly() {
        let mut s = RequestSpec::new("https://example.com/f");
        s.username = Some("Aladdin".into());
        s.password = Some("open sesame".into());
        // The canonical RFC 7617 example.
        assert_eq!(
            s.headers(None).get(AUTHORIZATION).unwrap(),
            "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
    }

    #[test]
    fn base64_handles_every_padding_case() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
    }

    #[test]
    fn content_range_yields_the_total() {
        assert_eq!(parse_content_range_total("bytes 0-0/12345"), Some(12345));
        assert_eq!(parse_content_range_total("bytes 0-0/*"), None, "unknown total");
        assert_eq!(parse_content_range_total("nonsense"), None);
    }

    #[test]
    fn disposition_filename_beats_the_url() {
        let n = resolve_filename(
            Some("attachment; filename=\"Report Q3.pdf\""),
            "https://example.com/dl?id=99",
            None,
        );
        assert_eq!(n, "Report Q3.pdf");
    }

    #[test]
    fn rfc5987_filename_star_wins_over_plain_filename() {
        // The plain parameter appears first and is the ASCII fallback; filename* must win even
        // so, and its percent-encoding must be decoded.
        let n = resolve_filename(
            Some("attachment; filename=\"report.pdf\"; filename*=UTF-8''%E2%82%AC%20rates.pdf"),
            "https://example.com/dl",
            None,
        );
        assert_eq!(n, "\u{20ac} rates.pdf");
    }

    #[test]
    fn semicolons_inside_a_quoted_filename_do_not_split_it() {
        let n = resolve_filename(
            Some("attachment; filename=\"a; b (final).zip\""),
            "https://example.com/x",
            None,
        );
        assert_eq!(n, "a; b (final).zip");
    }

    #[test]
    fn url_filenames_are_percent_decoded() {
        let n = resolve_filename(None, "https://example.com/files/My%20Album%20%232.zip", None);
        assert_eq!(n, "My Album #2.zip");
    }

    #[test]
    fn query_strings_are_not_part_of_the_name() {
        let n = resolve_filename(None, "https://example.com/a/video.mp4?token=xyz&e=123", None);
        assert_eq!(n, "video.mp4");
    }

    #[test]
    fn an_extensionless_url_borrows_a_suffix_from_the_media_type() {
        let n = resolve_filename(None, "https://example.com/stream/abc123", Some("video/mp4"));
        assert_eq!(n, "abc123.mp4");
        // octet-stream says nothing useful, so the bare name stands.
        let n = resolve_filename(None, "https://example.com/s/raw", Some("application/octet-stream"));
        assert_eq!(n, "raw");
    }

    #[test]
    fn a_nameless_url_falls_back_to_the_media_type() {
        assert_eq!(resolve_filename(None, "https://example.com/", Some("application/pdf")), "download.pdf");
        assert_eq!(resolve_filename(None, "https://example.com/", None), "download");
    }

    #[test]
    fn traversal_in_a_server_supplied_name_is_defeated() {
        // The name arrives from a remote server and must not be able to escape the directory.
        assert_eq!(sanitize_filename("../../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("..\\..\\Windows\\System32\\evil.dll"), "evil.dll");
        assert_eq!(sanitize_filename("/absolute/path.bin"), "path.bin");
        assert_eq!(sanitize_filename(".."), "download");
        assert_eq!(sanitize_filename(""), "download");
    }

    #[test]
    fn reserved_and_invalid_names_are_made_safe() {
        assert_eq!(sanitize_filename("a<b>c:d\"e|f?g*h.txt"), "a_b_c_d_e_f_g_h.txt");
        assert_eq!(sanitize_filename("file\u{1}name.bin"), "file_name.bin");
        // Windows silently drops trailing dots and spaces.
        assert_eq!(sanitize_filename("report.pdf..  "), "report.pdf");
        // Device names are reserved whatever the extension.
        assert_eq!(sanitize_filename("CON"), "_CON");
        assert_eq!(sanitize_filename("nul.txt"), "_nul.txt");
        assert_eq!(sanitize_filename("console.txt"), "console.txt", "only exact matches");
    }

    #[test]
    fn overlong_names_are_shortened_but_keep_their_extension() {
        let long = format!("{}.mkv", "x".repeat(400));
        let out = sanitize_filename(&long);
        assert!(out.len() <= 200, "length {}", out.len());
        assert!(out.ends_with(".mkv"), "extension must survive truncation: {out}");
    }

    #[test]
    fn html_detection_distinguishes_a_file_url_from_a_page_url() {
        // A file URL answering with HTML means the link lapsed.
        assert!(is_html("text/html; charset=utf-8"));
        assert!(!wants_html("https://example.com/big-movie.mkv"));
        // These genuinely asked for a page.
        assert!(wants_html("https://example.com/index.html"));
        assert!(wants_html("https://example.com/path/"));
        assert!(wants_html("https://example.com/articles/some-slug"));
    }

    #[test]
    fn segmentable_needs_both_ranges_and_a_size() {
        let base = ResourceInfo {
            final_url: "https://e/f".into(),
            total: Some(1000),
            supports_ranges: true,
            filename: "f".into(),
            content_type: None,
            etag: None,
            last_modified: None,
        };
        assert!(base.is_segmentable());
        assert!(!ResourceInfo { supports_ranges: false, ..base.clone() }.is_segmentable());
        assert!(!ResourceInfo { total: None, ..base.clone() }.is_segmentable());
        assert!(!ResourceInfo { total: Some(0), ..base }.is_segmentable());
    }

    fn info(etag: Option<&str>, lm: Option<&str>, total: Option<u64>) -> ResourceInfo {
        ResourceInfo {
            final_url: "https://e/f".into(),
            total,
            supports_ranges: true,
            filename: "f".into(),
            content_type: None,
            etag: etag.map(Into::into),
            last_modified: lm.map(Into::into),
        }
    }

    #[test]
    fn a_changed_validator_blocks_a_resume() {
        let saved = info(Some("\"v1\""), None, Some(100));
        assert!(info(Some("\"v2\""), None, Some(100)).resume_conflict(&saved).is_some());
        assert!(info(Some("\"v1\""), None, Some(100)).resume_conflict(&saved).is_none());
    }

    #[test]
    fn weak_etags_are_not_treated_as_byte_identity() {
        // A weak validator says the content is semantically equivalent, not byte-identical, so
        // it is no evidence either way and must not block an otherwise fine resume.
        let saved = info(Some("W/\"a\""), None, Some(100));
        assert!(info(Some("W/\"b\""), None, Some(100)).resume_conflict(&saved).is_none());
    }

    #[test]
    fn a_changed_size_blocks_a_resume_when_there_is_no_validator() {
        let saved = info(None, None, Some(100));
        let conflict = info(None, None, Some(250)).resume_conflict(&saved).unwrap();
        assert!(conflict.contains("size changed"));
    }

    #[test]
    fn a_changed_last_modified_blocks_a_resume() {
        let saved = info(None, Some("Mon, 01 Jan 2024 00:00:00 GMT"), Some(100));
        assert!(info(None, Some("Tue, 02 Jan 2024 00:00:00 GMT"), Some(100))
            .resume_conflict(&saved)
            .is_some());
    }
}
