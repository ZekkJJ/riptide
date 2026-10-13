//! Daemon configuration and the shared secret the browser extension authenticates with.

use anyhow::{Context, Result};
use freedm_core::category::CategorySet;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Where finished files go.
    pub download_dir: PathBuf,
    /// File into per-category subfolders.
    pub categorize: bool,
    /// Connections per download.
    pub connections_per_download: usize,
    /// Downloads transferring at once; the rest wait in the queue.
    pub max_concurrent_downloads: usize,
    /// Ceiling across all downloads, in bytes per second. 0 for unlimited.
    pub global_speed_limit: u64,
    /// Attempts per segment, and for the initial probe, before giving up.
    pub max_retries: u32,
    pub proxy: Option<String>,
    /// Port the local API listens on, bound to the loopback interface only.
    pub port: u16,
    /// Shared secret the browser extension must present. Generated on first run.
    pub token: String,
    pub categories: CategorySet,
    /// Extensions the extension should offer to capture. Empty means use the category list.
    pub capture_extensions: Vec<String>,
    /// Hosts never captured, as glob patterns.
    pub capture_exclusions: Vec<String>,
    /// Smallest response worth taking over, in bytes.
    pub capture_min_size: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            download_dir: default_download_dir(),
            categorize: true,
            connections_per_download: 8,
            max_concurrent_downloads: 3,
            global_speed_limit: 0,
            max_retries: 5,
            proxy: None,
            port: 7739,
            token: generate_token(),
            categories: CategorySet::standard(),
            capture_extensions: Vec::new(),
            capture_exclusions: vec![
                // Browser and OS update traffic, which must never be diverted.
                "*.windowsupdate.com".into(),
                "*.mozilla.org".into(),
                "*.googleapis.com".into(),
                "clients*.google.com".into(),
                "*.gvt1.com".into(),
            ],
            // Below a few hundred kilobytes, taking the download over is slower than letting
            // the browser finish the request it already has open.
            capture_min_size: 256 * 1024,
        }
    }
}

impl AppConfig {
    pub fn path() -> PathBuf {
        config_dir().join("config.json")
    }

    /// Load the config, writing a default one (with a fresh token) if none exists.
    pub fn load_or_create() -> Result<Self> {
        let path = Self::path();
        if let Ok(bytes) = std::fs::read(&path) {
            match serde_json::from_slice::<Self>(&bytes) {
                Ok(mut cfg) => {
                    // An older config, or one hand-edited to remove the token, still needs one.
                    if cfg.token.len() < 16 {
                        cfg.token = generate_token();
                        cfg.save()?;
                    }
                    return Ok(cfg);
                }
                Err(e) => {
                    // Never start with a half-understood config: say so and keep the file.
                    anyhow::bail!("{} is not valid config: {e}", path.display());
                }
            }
        }
        let cfg = Self::default();
        cfg.save()?;
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let json = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension("json.new");
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Extensions to hand the browser extension.
    pub fn effective_capture_extensions(&self) -> Vec<String> {
        if self.capture_extensions.is_empty() {
            self.categories.capture_extensions()
        } else {
            self.capture_extensions.clone()
        }
    }

    pub fn engine_config(&self) -> freedm_core::DownloadConfig {
        freedm_core::DownloadConfig {
            max_connections: self.connections_per_download.clamp(1, 64),
            max_retries: self.max_retries,
            proxy: self.proxy.clone(),
            // The per-download limit is derived from the global one so that the configured
            // ceiling is respected no matter how many downloads are running.
            speed_limit: if self.global_speed_limit == 0 {
                0
            } else {
                (self.global_speed_limit / self.max_concurrent_downloads.max(1) as u64).max(1024)
            },
            ..Default::default()
        }
    }
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir().map(|d| d.join("freedm")).unwrap_or_else(|| PathBuf::from(".freedm"))
}

fn default_download_dir() -> PathBuf {
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .map(|d| d.join("FreeDM"))
        .unwrap_or_else(|| PathBuf::from("downloads"))
}

/// A 256-bit random token, hex encoded.
///
/// Taken from the OS random source via `Uuid::new_v4`, which uses `getrandom`; two UUIDs give
/// 256 bits, of which 122 bits each are random.
fn generate_token() -> String {
    format!("{:x}{:x}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_config_has_a_long_random_token() {
        let a = AppConfig::default();
        let b = AppConfig::default();
        assert_eq!(a.token.len(), 64);
        assert!(a.token.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a.token, b.token, "each install must get its own secret");
    }

    #[test]
    fn the_api_binds_a_non_privileged_port() {
        assert!(AppConfig::default().port > 1024);
    }

    #[test]
    fn capture_extensions_fall_back_to_the_category_list() {
        let mut cfg = AppConfig::default();
        assert!(cfg.effective_capture_extensions().contains(&"mkv".to_string()));
        cfg.capture_extensions = vec!["iso".into()];
        assert_eq!(cfg.effective_capture_extensions(), vec!["iso".to_string()]);
    }

    #[test]
    fn the_global_limit_is_shared_out_across_concurrent_downloads() {
        let mut cfg =
            AppConfig { global_speed_limit: 3_000_000, max_concurrent_downloads: 3, ..Default::default() };
        // Three downloads at a third each keeps the total at the configured ceiling.
        assert_eq!(cfg.engine_config().speed_limit, 1_000_000);

        cfg.global_speed_limit = 0;
        assert_eq!(cfg.engine_config().speed_limit, 0, "0 means unlimited");
    }

    #[test]
    fn connection_counts_are_clamped_to_something_sane() {
        let mut cfg = AppConfig { connections_per_download: 0, ..Default::default() };
        assert_eq!(cfg.engine_config().max_connections, 1);
        cfg.connections_per_download = 9999;
        assert_eq!(cfg.engine_config().max_connections, 64);
    }

    #[test]
    fn update_traffic_is_excluded_from_capture_by_default() {
        let cfg = AppConfig::default();
        assert!(cfg.capture_exclusions.iter().any(|p| p.contains("windowsupdate")));
    }

    #[test]
    fn config_round_trips_through_json() {
        let cfg = AppConfig::default();
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        let back: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.token, cfg.token);
        assert_eq!(back.port, cfg.port);
        assert_eq!(back.categories.categories.len(), cfg.categories.categories.len());
    }

    #[test]
    fn a_partial_config_file_fills_in_defaults() {
        // Users hand-edit this file, so unknown-but-absent fields must not be fatal.
        let back: AppConfig = serde_json::from_str(r#"{"connections_per_download": 16}"#).unwrap();
        assert_eq!(back.connections_per_download, 16);
        assert_eq!(back.port, 7739);
        assert!(!back.token.is_empty());
    }
}
