// SPDX-FileCopyrightText: Riptide contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Riptide command line interface.

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use riptide_core::category::CategorySet;
use riptide_core::media::{self, Manifest};
use riptide_core::{Download, DownloadConfig, Progress, RequestSpec};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "riptide",
    version,
    about = "A free, open source multi-segment download manager",
    long_about = "Riptide downloads a file over several connections at once, splitting the work \
                  dynamically so that slow connections do not hold the transfer back. Downloads \
                  resume after an interruption, and HLS or DASH streams can be fetched as a \
                  single file."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// URL to download, when no subcommand is given.
    url: Option<String>,

    #[command(flatten)]
    opts: DownloadOpts,

    /// Print detailed engine logs.
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Download a URL.
    Get {
        url: String,
        #[command(flatten)]
        opts: DownloadOpts,
    },
    /// Resume a partial download from its state file.
    Resume {
        /// The partial file, its .rtstate file, or the final name.
        file: PathBuf,
        #[command(flatten)]
        opts: DownloadOpts,
    },
    /// List the quality variants of an HLS or DASH stream.
    Variants {
        url: String,
        #[command(flatten)]
        opts: DownloadOpts,
    },
    /// Show which folder a filename would be filed under.
    Categorize { filename: String },
}

#[derive(Args, Clone, Default)]
struct DownloadOpts {
    /// Save under this name instead of the one the server gives.
    #[arg(short = 'o', long)]
    output: Option<String>,

    /// Directory to save into.
    #[arg(short = 'd', long)]
    dir: Option<PathBuf>,

    /// File into category subfolders (Video, Audio, Archives, ...).
    #[arg(long)]
    categorize: bool,

    /// Maximum simultaneous connections.
    #[arg(short = 'n', long, default_value_t = 8, value_parser = clap::value_parser!(u16).range(1..=64))]
    connections: u16,

    /// Speed limit, such as 500k or 2M. Unlimited by default.
    #[arg(short = 'l', long, value_parser = parse_rate)]
    limit: Option<u64>,

    /// Smallest range worth a connection of its own, such as 512k.
    #[arg(long, value_parser = parse_rate)]
    min_split: Option<u64>,

    /// Attempts per segment before giving up.
    #[arg(long, default_value_t = 5)]
    retries: u32,

    /// Referer header, often required for links captured from a page.
    #[arg(long)]
    referer: Option<String>,

    /// Cookie header, for links behind a login.
    #[arg(long)]
    cookies: Option<String>,

    /// User-Agent header.
    #[arg(long)]
    user_agent: Option<String>,

    /// Extra header as "Name: value". May be repeated.
    #[arg(short = 'H', long = "header")]
    headers: Vec<String>,

    /// Alternative URL for the same file. May be repeated.
    #[arg(long = "mirror")]
    mirrors: Vec<String>,

    /// Proxy URL, such as http://host:8080 or socks5://host:1080.
    #[arg(long)]
    proxy: Option<String>,

    /// HTTP Basic username.
    #[arg(long)]
    user: Option<String>,

    /// HTTP Basic password.
    #[arg(long)]
    password: Option<String>,

    /// For a stream, the variant to take: a variant id, "best", "worst", or a height like 1080.
    #[arg(long, default_value = "best")]
    variant: String,

    /// Show the live segment map while downloading.
    #[arg(long)]
    segments: bool,
}

impl DownloadOpts {
    fn spec(&self, url: &str) -> Result<RequestSpec> {
        let mut spec = RequestSpec::new(url);
        spec.referer = self.referer.clone();
        spec.cookies = self.cookies.clone();
        spec.user_agent = self.user_agent.clone();
        spec.username = self.user.clone();
        spec.password = self.password.clone();
        spec.mirrors = self.mirrors.clone();
        for h in &self.headers {
            let (name, value) = h
                .split_once(':')
                .with_context(|| format!("header {h:?} is not in \"Name: value\" form"))?;
            spec.headers.insert(name.trim().to_string(), value.trim().to_string());
        }
        Ok(spec)
    }

    fn config(&self) -> DownloadConfig {
        DownloadConfig {
            max_connections: self.connections as usize,
            min_split: self.min_split.unwrap_or(riptide_core::chunks::DEFAULT_MIN_SPLIT),
            max_retries: self.retries,
            timeout: Duration::from_secs(30),
            proxy: self.proxy.clone(),
            speed_limit: self.limit.unwrap_or(0),
        }
    }

    fn dest_dir(&self, filename: &str) -> PathBuf {
        let root = self.dir.clone().unwrap_or_else(default_download_dir);
        if self.categorize {
            CategorySet::standard().folder_for(&root, filename)
        } else {
            root
        }
    }
}

fn default_download_dir() -> PathBuf {
    dirs_download().unwrap_or_else(|| PathBuf::from("."))
}

fn dirs_download() -> Option<PathBuf> {
    // The crate is pulled in by the core already; reuse it rather than guessing at the shell.
    #[allow(unused_imports)]
    use std::env;
    if let Some(home) = env::var_os("USERPROFILE").or_else(|| env::var_os("HOME")) {
        let candidate = PathBuf::from(home).join("Downloads");
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

/// Parse a size such as `500k`, `2M`, `1.5G` into bytes.
fn parse_rate(s: &str) -> Result<u64, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("empty value".into());
    }
    let (num, mult) = match t.chars().last().unwrap().to_ascii_lowercase() {
        'k' => (&t[..t.len() - 1], 1024u64),
        'm' => (&t[..t.len() - 1], 1024 * 1024),
        'g' => (&t[..t.len() - 1], 1024 * 1024 * 1024),
        'b' => (&t[..t.len() - 1], 1),
        _ => (t, 1),
    };
    let value: f64 = num.trim().parse().map_err(|_| format!("{s:?} is not a size"))?;
    if value < 0.0 {
        return Err("size cannot be negative".into());
    }
    Ok((value * mult as f64) as u64)
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn human_duration(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let filter = if cli.verbose { "riptide_core=debug,riptide_cli=debug" } else { "warn" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| filter.into()),
        )
        .with_target(false)
        .without_time()
        .init();

    match cli.command {
        Some(Command::Get { url, opts }) => download(&url, opts).await,
        Some(Command::Resume { file, opts }) => resume(file, opts).await,
        Some(Command::Variants { url, opts }) => list_variants(&url, opts).await,
        Some(Command::Categorize { filename }) => {
            let set = CategorySet::standard();
            let cat = set.categorize(&filename).map(|c| c.name.as_str()).unwrap_or("(none)");
            println!("{filename}");
            println!("  category: {cat}");
            println!(
                "  folder:   {}",
                set.folder_for(&default_download_dir(), &filename).display()
            );
            println!("  captured: {}", set.should_capture(&filename));
            Ok(())
        }
        None => match cli.url.clone() {
            Some(url) => download(&url, cli.opts).await,
            None => {
                // No arguments at all: show the help rather than failing silently.
                use clap::CommandFactory;
                Cli::command().print_help()?;
                println!();
                Ok(())
            }
        },
    }
}

async fn download(url: &str, opts: DownloadOpts) -> Result<()> {
    // An adaptive manifest is not a file; fetch and expand it instead.
    if media::looks_like_manifest(url, None) {
        return download_stream(url, opts).await;
    }
    run_one(url, &opts).await
}

async fn run_one(url: &str, opts: &DownloadOpts) -> Result<()> {
    let spec = opts.spec(url)?;
    let config = opts.config();

    // The destination depends on the filename, which only the probe knows, so probe into a
    // provisional directory and let `Download::prepare` settle the final path.
    let probe_dir = opts.dir.clone().unwrap_or_else(default_download_dir);
    let client = riptide_core::probe::client(config.proxy.as_deref(), config.timeout)?;
    let info = riptide_core::probe::probe(&client, &spec).await?;

    let name = opts.output.clone().unwrap_or_else(|| info.filename.clone());
    let dest = if opts.categorize { opts.dest_dir(&name) } else { probe_dir };

    let dl = Download::prepare(spec, &dest, opts.output.clone(), config).await?;
    let info = dl.info().clone();

    eprintln!("{}", dl.output_path().display());
    eprint!("  {}", info.total.map(human_bytes).unwrap_or_else(|| "unknown size".into()));
    if info.is_segmentable() {
        eprintln!(", up to {} connections", opts.connections);
    } else {
        eprintln!(", single connection (the server will not serve byte ranges)");
    }

    let handle = dl.handle();
    let bar = make_bar(info.total);
    let show_segments = opts.segments;

    let watcher = tokio::spawn({
        let bar = bar.clone();
        let mut rx = handle.progress.clone();
        async move {
            loop {
                let p: Progress = rx.borrow_and_update().clone();
                render(&bar, &p, show_segments);
                if p.done || rx.changed().await.is_err() {
                    break;
                }
            }
        }
    });

    // Ctrl-C leaves the partial file and its state behind so the transfer can be resumed.
    let interrupt = {
        let handle = DownloadHandleRef(handle);
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                handle.0.pause();
            }
        })
    };

    let result = dl.run().await;
    watcher.abort();
    interrupt.abort();

    match result {
        Ok(path) => {
            bar.finish_and_clear();
            println!("{}", path.display());
            Ok(())
        }
        Err(e) => {
            bar.abandon();
            Err(report(e))
        }
    }
}

/// Keeps a handle alive for the signal task without borrowing the download.
struct DownloadHandleRef(riptide_core::DownloadHandle);

fn report(e: riptide_core::Error) -> anyhow::Error {
    use riptide_core::Error as E;
    // Turn the conditions a user can act on into advice rather than a bare error.
    let hint = match &e {
        E::LinkExpired => Some(
            "the link has most likely expired; open the page again and copy a fresh URL \
             (and pass --referer and --cookies if the site needs a session)",
        ),
        E::ContentChanged { .. } => Some(
            "the file on the server is not the one partially downloaded; delete the .rtpart \
             and .rtstate files to start over",
        ),
        E::ResumeUnsupported => {
            Some("the server ignored our byte range; retry with -n 1 to download in one stream")
        }
        E::BadStatus { status: 403, .. } => Some(
            "the server refused the request; a Referer or Cookie header from the page is \
             usually what is missing",
        ),
        _ => None,
    };
    match hint {
        Some(h) => anyhow::anyhow!("{e}\n\nhint: {h}"),
        None => anyhow::anyhow!(e),
    }
}

fn make_bar(total: Option<u64>) -> ProgressBar {
    match total {
        Some(t) => {
            let bar = ProgressBar::new(t);
            bar.set_style(
                ProgressStyle::with_template("  {bar:34.cyan/blue} {percent:>3}%  {msg}")
                    .unwrap()
                    .progress_chars("=> "),
            );
            bar
        }
        None => {
            let bar = ProgressBar::new_spinner();
            bar.set_style(ProgressStyle::with_template("  {spinner} {msg}").unwrap());
            bar
        }
    }
}

fn render(bar: &ProgressBar, p: &Progress, show_segments: bool) {
    if p.total.is_some() {
        bar.set_position(p.downloaded);
    }
    let mut msg = format!(
        "{}/{}  {}/s",
        human_bytes(p.downloaded),
        p.total.map(human_bytes).unwrap_or_else(|| "?".into()),
        human_bytes(p.speed),
    );
    if p.connections > 0 {
        msg.push_str(&format!("  {} conn", p.connections));
    }
    if p.stalled {
        msg.push_str("  stalled, retrying");
    } else if let Some(eta) = p.eta_secs {
        msg.push_str(&format!("  eta {}", human_duration(eta)));
    }
    if show_segments {
        msg.push_str(&format!("  [{} seg]", p.segments));
    }
    bar.set_message(msg);
}

async fn resume(file: PathBuf, opts: DownloadOpts) -> Result<()> {
    // Accept whichever of the three related names the user had to hand.
    let base = {
        let s = file.to_string_lossy();
        let trimmed = s.trim_end_matches(".rtstate").trim_end_matches(".rtpart").to_string();
        PathBuf::from(trimmed)
    };
    let state_path = {
        let mut s = base.clone().into_os_string();
        s.push(".rtstate");
        PathBuf::from(s)
    };

    let state = riptide_core::ResumeState::load(&state_path)
        .await
        .with_context(|| format!("no resume state at {}", state_path.display()))?;

    eprintln!(
        "resuming {} ({} of {} already downloaded)",
        base.display(),
        human_bytes(state.map.downloaded()),
        state.total.map(human_bytes).unwrap_or_else(|| "?".into()),
    );

    let mut spec = opts.spec(&state.url)?;
    if spec.mirrors.is_empty() {
        spec.mirrors = state.mirrors.clone();
    }

    let mut opts = opts;
    opts.dir = base.parent().map(|p| p.to_path_buf());
    opts.output = base.file_name().map(|n| n.to_string_lossy().to_string());
    opts.categorize = false;
    run_one(&state.url.clone(), &opts).await
}

async fn fetch_text(url: &str, opts: &DownloadOpts) -> Result<String> {
    let config = opts.config();
    let client = riptide_core::probe::client(config.proxy.as_deref(), config.timeout)?;
    let spec = opts.spec(url)?;
    let resp = client.get(url).headers(spec.headers(None)).send().await?;
    if !resp.status().is_success() {
        bail!("{} returned {}", url, resp.status());
    }
    Ok(resp.text().await?)
}

async fn list_variants(url: &str, opts: DownloadOpts) -> Result<()> {
    let body = fetch_text(url, &opts).await?;
    match media::parse(&body, url)? {
        Manifest::HlsMaster { variants } => {
            println!("HLS master playlist, {} variants:", variants.len());
            for v in &variants {
                println!(
                    "  {:<24} {:<22} {}",
                    v.id,
                    v.label(),
                    if v.is_audio_only { "audio" } else { "video" }
                );
            }
        }
        Manifest::HlsMedia { plan } => {
            println!(
                "HLS media playlist: {} segments, {:.0}s{}",
                plan.total_segments(),
                plan.duration,
                if plan.live { ", live" } else { "" }
            );
            describe_encryption(plan.encryption.as_ref());
        }
        Manifest::Dash { streams } => {
            println!("DASH manifest, {} representations:", streams.len());
            for (v, plan) in &streams {
                println!(
                    "  {:<24} {:<22} {:>5} seg  {}",
                    v.id,
                    v.label(),
                    plan.total_segments(),
                    if v.is_audio_only { "audio" } else { "video" }
                );
            }
            if let Some((_, plan)) = streams.first() {
                describe_encryption(plan.encryption.as_ref());
            }
        }
    }
    Ok(())
}

fn describe_encryption(enc: Option<&media::Encryption>) {
    match enc {
        Some(media::Encryption::Aes128 { .. }) => {
            println!("  encryption: AES-128 with an openly served key (supported)");
        }
        Some(media::Encryption::Drm { system }) => {
            println!("  encryption: {system} DRM, which Riptide will not download");
        }
        _ => {}
    }
}

async fn download_stream(url: &str, opts: DownloadOpts) -> Result<()> {
    let spec = opts.spec(url)?;
    let config = opts.config();

    let (variant, plan) = riptide_core::stream::resolve_manifest(&spec, &config, &opts.variant)
        .await
        .map_err(report)?;
    eprintln!("variant: {} {}", variant.id, variant.label());

    plan.check_supported().map_err(report)?;
    if plan.live {
        bail!(
            "this is a live stream with no end, so it cannot be downloaded as a file;              Riptide only fetches finished (VOD) streams"
        );
    }

    let name =
        opts.output.clone().unwrap_or_else(|| riptide_core::stream::stream_filename(url, &plan));
    let dir = opts.dest_dir(&name);

    // Ctrl-C stops at the next segment boundary, leaving a resumable checkpoint.
    let cancel = Arc::new(AtomicBool::new(false));
    let signal = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("\nstopping after the current segment...");
                cancel.store(true, Ordering::Relaxed);
            }
        })
    };

    let result = riptide_core::stream::download_plan(
        &plan,
        &dir,
        &name,
        &spec,
        config,
        cancel,
        |done, total, bytes| {
            eprint!("\r  segment {done}/{total}  {}          ", human_bytes(bytes));
        },
    )
    .await;
    signal.abort();
    eprintln!();

    match result {
        Ok(path) => {
            println!("{}", path.display());
            Ok(())
        }
        Err(riptide_core::Error::Cancelled) => {
            eprintln!("stopped; run the same command again to carry on");
            Ok(())
        }
        Err(e) => Err(report(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse_with_and_without_suffixes() {
        assert_eq!(parse_rate("1024").unwrap(), 1024);
        assert_eq!(parse_rate("500k").unwrap(), 512_000);
        assert_eq!(parse_rate("2M").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_rate("1.5G").unwrap(), (1.5 * 1024.0 * 1024.0 * 1024.0) as u64);
        assert_eq!(parse_rate("64b").unwrap(), 64);
        assert!(parse_rate("fast").is_err());
        assert!(parse_rate("-5M").is_err());
        assert!(parse_rate("").is_err());
    }

    #[test]
    fn byte_counts_render_readably() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1_572_864), "1.5 MiB");
        assert_eq!(human_bytes(5_368_709_120), "5.0 GiB");
    }

    #[test]
    fn durations_render_readably() {
        assert_eq!(human_duration(45), "45s");
        assert_eq!(human_duration(90), "1m30s");
        assert_eq!(human_duration(3700), "1h01m");
    }

    #[test]
    fn headers_must_be_name_colon_value() {
        let mut opts =
            DownloadOpts { headers: vec!["X-Token: abc123".into()], ..Default::default() };
        let spec = opts.spec("https://e.com/f").unwrap();
        assert_eq!(spec.headers.get("X-Token").unwrap(), "abc123");

        opts.headers = vec!["malformed".into()];
        assert!(opts.spec("https://e.com/f").is_err());
    }

    #[test]
    fn the_cli_definition_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
