//! Client configuration file handling.

use std::path::PathBuf;

use anyhow::{Context, Result};
use rnb_shared::AppConfig;

/// `$RUSTBOND_CONFIG`, else `~/Library/Application Support/rustbond/config.toml`
/// on macOS / `~/.config/rustbond/config.toml` elsewhere.
pub fn path() -> PathBuf {
    if let Ok(p) = std::env::var("RUSTBOND_CONFIG") {
        return PathBuf::from(p);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rustbond")
        .join("config.toml")
}

pub fn load() -> AppConfig {
    let p = path();
    match std::fs::read_to_string(&p) {
        Ok(txt) => toml::from_str(&txt).unwrap_or_else(|e| {
            eprintln!("invalid config {}: {e}", p.display());
            AppConfig::example()
        }),
        Err(_) => {
            let example = AppConfig::example();
            if let Err(e) = save(&example) {
                eprintln!("cannot write example config {}: {e:#}", p.display());
            } else {
                eprintln!("example config written to {}", p.display());
            }
            example
        }
    }
}

pub fn save(cfg: &AppConfig) -> Result<PathBuf> {
    let p = path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let txt = toml::to_string_pretty(cfg)?;
    std::fs::write(&p, txt).with_context(|| format!("writing {}", p.display()))?;
    Ok(p)
}
