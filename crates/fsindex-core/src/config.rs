//! Configuration: roots to index, exclusions and tuning knobs.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RootConfig {
    /// Absolute path (a leading `~` is expanded).
    pub path: String,
    /// Optional human label; defaults to the path.
    pub label: Option<String>,
    pub enabled: bool,
}

impl Default for RootConfig {
    fn default() -> Self {
        RootConfig {
            path: String::new(),
            label: None,
            enabled: true,
        }
    }
}

impl RootConfig {
    pub fn new(path: impl Into<String>) -> Self {
        RootConfig {
            path: path.into(),
            ..Default::default()
        }
    }

    pub fn resolved(&self) -> PathBuf {
        expand_tilde(&self.path)
    }

    pub fn display(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.path)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ExclusionsConfig {
    /// Apply the built-in sensible defaults in addition to `names`/`paths`/`patterns`.
    pub use_defaults: bool,
    pub names: Vec<String>,
    /// Absolute path prefixes; the path and everything below it is skipped.
    pub paths: Vec<String>,
    pub patterns: Vec<String>,
}

impl Default for ExclusionsConfig {
    fn default() -> Self {
        ExclusionsConfig {
            use_defaults: true,
            names: Vec::new(),
            paths: Vec::new(),
            patterns: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub roots: Vec<RootConfig>,
    pub exclusions: ExclusionsConfig,
    /// Default page size when a query does not specify a limit.
    pub max_results: u64,
    /// Periodic full rescan per volume (correctness floor).
    pub reconcile_interval_secs: u64,
    /// Coalesce window for filesystem events.
    pub debounce_ms: u64,
    /// A directory with more children than this is not individually watched.
    pub max_dirs_per_volume: usize,
    /// Pending dirs above this triggers a full volume rescan instead.
    pub max_pending_dirs: usize,
    /// Compaction cadence for tombstoned nodes.
    pub compact_interval_secs: u64,
    /// Compact once dead tombstones exceed this fraction (percent) of live.
    pub compact_dead_percent: u64,
    /// Log filter, e.g. `info` or `fsindex_core=debug`.
    pub log: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            roots: vec![RootConfig::new("~/")],
            exclusions: ExclusionsConfig::default(),
            max_results: 1000,
            reconcile_interval_secs: 3600,
            debounce_ms: 250,
            max_dirs_per_volume: 250_000,
            max_pending_dirs: 50_000,
            compact_interval_secs: 900,
            compact_dead_percent: 50,
            log: None,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        let text = std::fs::read_to_string(path)?;
        let cfg: Config = toml::from_str(&text)?;
        Ok(cfg)
    }

    pub fn load_or_default(path: &Path) -> Config {
        Config::load(path).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }

    /// Enabled roots whose path is non-empty.
    pub fn enabled_roots(&self) -> Vec<&RootConfig> {
        self.roots
            .iter()
            .filter(|r| r.enabled && !r.path.trim().is_empty())
            .collect()
    }

    pub fn exclusions(&self) -> anyhow::Result<crate::paths::Exclusions> {
        crate::paths::Exclusions::build(
            &self.exclusions.names,
            &self.exclusions.paths,
            &self.exclusions.patterns,
            self.exclusions.use_defaults,
        )
    }
}

/// ` $XDG_CONFIG_HOME/fsindex-mk2/fsindex.toml` or `~/.config/fsindex/fsindex.toml`.
pub fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| expand_tilde("~/.config"))
        .join("fsindex-mk2")
        .join("fsindex.toml")
}

/// `$XDG_STATE_HOME/fsindex` or `~/.local/state/fsindex`.
pub fn state_dir() -> PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| expand_tilde("~/.local/state"))
        .join("fsindex-mk2")
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if p == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
    }
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(p)
}

/// Write an annotated default config to `path` if it does not exist.
pub fn write_default_config(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        return Ok(());
    }
    let cfg = Config::default();
    cfg.save(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_home() {
        let p = expand_tilde("~/foo");
        assert!(p.to_string_lossy().ends_with("/foo"));
        assert!(!p.to_string_lossy().starts_with('~'));
    }

    #[test]
    fn default_config_roundtrips() {
        let cfg = Config::default();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.max_results, cfg.max_results);
    }
}
