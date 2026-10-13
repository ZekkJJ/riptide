// SPDX-FileCopyrightText: Riptide contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! The Riptide local service: download queue, HTTP API and web UI.

mod api;
mod config;
mod manager;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::AppConfig;
use manager::Manager;
use std::net::{Ipv4Addr, SocketAddr};

#[derive(Parser)]
#[command(
    name = "riptided",
    version,
    about = "Riptide background service, web UI and browser-integration endpoint"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Override the configured port.
    #[arg(long)]
    port: Option<u16>,

    /// Open the web UI in a browser once the service is listening.
    #[arg(long)]
    open: bool,

    #[arg(short, long)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Run the service (the default).
    Serve,
    /// Print the connection token for the browser extension.
    Token,
    /// Print where the config file lives.
    ConfigPath,
    /// Queue a URL on an already-running service.
    Add {
        url: String,
        #[arg(short, long)]
        filename: Option<String>,
        #[arg(long)]
        referer: Option<String>,
    },
    /// Show what the running service is doing.
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(
            |_| {
                if cli.verbose {
                    "riptide_core=debug,riptide_daemon=debug".into()
                } else {
                    "info".into()
                }
            },
        ))
        .with_target(false)
        .init();

    let mut cfg = AppConfig::load_or_create()
        .with_context(|| format!("loading {}", AppConfig::path().display()))?;
    if let Some(port) = cli.port {
        cfg.port = port;
    }

    match cli.command {
        Some(Command::Token) => {
            println!("{}", cfg.token);
            Ok(())
        }
        Some(Command::ConfigPath) => {
            println!("{}", AppConfig::path().display());
            Ok(())
        }
        Some(Command::Add { url, filename, referer }) => {
            let body = serde_json::json!({
                "url": url, "filename": filename, "referer": referer, "source": "cli"
            });
            let item: serde_json::Value = request(&cfg, "POST", "/downloads", Some(body)).await?;
            println!("queued {}", item["filename"].as_str().unwrap_or(&url));
            Ok(())
        }
        Some(Command::Status) => {
            let items: Vec<serde_json::Value> = request(&cfg, "GET", "/downloads", None).await?;
            if items.is_empty() {
                println!("no downloads");
                return Ok(());
            }
            for it in items {
                let p = &it["progress"];
                let done = p["downloaded"].as_u64().unwrap_or(0);
                let total = p["total"].as_u64();
                let pct = total
                    .filter(|t| *t > 0)
                    .map(|t| format!("{:>5.1}%", done as f64 / t as f64 * 100.0))
                    .unwrap_or_else(|| "    ?".into());
                println!(
                    "{pct}  {:<10} {}",
                    it["status"].as_str().unwrap_or("?"),
                    it["filename"].as_str().unwrap_or("?"),
                );
            }
            Ok(())
        }
        Some(Command::Serve) | None => serve(cfg, cli.open).await,
    }
}

async fn serve(cfg: AppConfig, open: bool) -> Result<()> {
    std::fs::create_dir_all(&cfg.download_dir)
        .with_context(|| format!("creating {}", cfg.download_dir.display()))?;

    let port = cfg.port;
    let token = cfg.token.clone();
    let download_dir = cfg.download_dir.clone();
    let manager = Manager::new(cfg);
    let app = api::router(manager, token.clone());

    // Loopback only: nothing here should ever be reachable from the network.
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| {
        format!("binding {addr} (is another Riptide already running? try --port)")
    })?;

    let url = format!("http://127.0.0.1:{port}/");
    tracing::info!("Riptide listening on {url}");
    tracing::info!("saving to {}", download_dir.display());
    eprintln!();
    eprintln!("  Riptide is running.");
    eprintln!("  Web UI:  {url}");
    eprintln!("  Folder:  {}", download_dir.display());
    eprintln!("  Token:   {token}");
    eprintln!();
    eprintln!("  Paste the token into the browser extension's options to capture downloads.");
    eprintln!("  Press Ctrl-C to stop.");
    eprintln!();

    if open {
        let _ = open_in_browser(&url);
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down; partial downloads can be resumed next time");
        })
        .await
        .context("serving the api")?;
    Ok(())
}

/// Call the running service's own API, for the client subcommands.
async fn request<T: serde::de::DeserializeOwned>(
    cfg: &AppConfig,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<T> {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{}/api{path}", cfg.port);
    let mut req = match method {
        "POST" => client.post(&url),
        "DELETE" => client.delete(&url),
        _ => client.get(&url),
    }
    .header("X-Riptide-Token", &cfg.token);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req.send().await.with_context(|| {
        format!("cannot reach the Riptide service on port {}; start it with `riptided`", cfg.port)
    })?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("the service replied {status}: {text}");
    }
    serde_json::from_str(&text).with_context(|| format!("unexpected reply: {text}"))
}

fn open_in_browser(url: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn().map(|_| ())
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn().map(|_| ())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(url).spawn().map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cli_definition_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
