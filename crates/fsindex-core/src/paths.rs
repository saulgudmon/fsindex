//! Path helpers, extension normalisation and the exclusion rules that keep the
//! index small and relevant.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Lowercased extension without the leading dot. `None`/hidden dotfiles and
/// names without a dot yield an empty string.
pub fn normalize_ext(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => ext.to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// Name-only exclusion check used by the recursive walker's `filter_entry`.
pub fn excluded_name(names: &[String], name: &str) -> bool {
    names.iter().any(|n| n == name)
}

/// Rules for skipping directories/files during scans and directory resyncs.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Exclusions {
    /// Directory names skipped anywhere in the tree.
    pub names: Vec<String>,
    /// Absolute path prefixes; the path itself and everything below it is
    /// skipped. Useful for excluding a specific subtree of an indexed root.
    pub paths: Vec<String>,
    /// Regular expressions matched against the absolute path; a match excludes.
    pub patterns: Vec<String>,
    #[serde(skip)]
    compiled: Vec<Regex>,
    /// Canonicalised `paths`, used for fast prefix matching during walks.
    #[serde(skip)]
    path_prefixes: Vec<PathBuf>,
}

impl Default for Exclusions {
    fn default() -> Self {
        let mut e = Exclusions {
            names: Vec::new(),
            paths: Vec::new(),
            patterns: Vec::new(),
            compiled: Vec::new(),
            path_prefixes: Vec::new(),
        };
        e.names = builtin_names();
        e.compiled = builtin_patterns();
        e
    }
}

impl Exclusions {
    /// Build from config: start from user values; if `use_defaults`, append the
    /// built-ins. User patterns are compiled up front so an invalid regex is
    /// reported before anything is applied.
    pub fn build(
        names: &[String],
        paths: &[String],
        patterns: &[String],
        use_defaults: bool,
    ) -> anyhow::Result<Self> {
        let mut out = Exclusions {
            names: Vec::new(),
            paths: Vec::new(),
            patterns: Vec::new(),
            compiled: Vec::new(),
            path_prefixes: Vec::new(),
        };
        if use_defaults {
            out.names = builtin_names();
            out.compiled = builtin_patterns();
        }
        for n in names {
            if !out.names.contains(n) {
                out.names.push(n.clone());
            }
        }
        for p in paths {
            out.path_prefixes.push(normalize_path(p));
            out.paths.push(p.clone());
        }
        for p in patterns {
            let re =
                Regex::new(p).map_err(|e| anyhow::anyhow!("invalid exclude pattern {p:?}: {e}"))?;
            out.patterns.push(p.clone());
            out.compiled.push(re);
        }
        Ok(out)
    }

    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// Should this entry be excluded? `path` is absolute. Walkers always pass a
    /// canonical path (built from a canonicalised root), so `paths` can be
    /// prefix-matched without re-stating every entry.
    pub fn excludes(&self, path: &Path, name: &str, _is_dir: bool) -> bool {
        if self.names.iter().any(|n| n == name) {
            return true;
        }
        if self
            .path_prefixes
            .iter()
            .any(|prefix| path == prefix || path.starts_with(prefix))
        {
            return true;
        }
        if self.compiled.is_empty() {
            return false;
        }
        match path.to_str() {
            Some(p) => self.compiled.iter().any(|re| re.is_match(p)),
            None => false,
        }
    }
}

/// Expand a leading `~` and canonicalise a user-entered exclusion path. When the
/// path does not exist (offline volume, typo), the expanded form is kept so the
/// rule still applies once it appears.
fn normalize_path(p: &str) -> PathBuf {
    let expanded = expand_tilde(p);
    std::fs::canonicalize(&expanded).unwrap_or(expanded)
}

/// Minimal `~`/`~/` expansion (kept local so `paths` has no config dependency).
fn expand_tilde(p: &str) -> PathBuf {
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

/// The built-in exclusion names, exposed so clients (CLI/GUI) can show the
/// user exactly what `use_defaults = true` skips.
pub fn default_exclusion_names() -> Vec<String> {
    builtin_names()
}

/// The built-in exclusion patterns (regex sources), exposed for display.
pub fn default_exclusion_patterns() -> Vec<String> {
    BUILTIN_PATTERNS.iter().map(|s| s.to_string()).collect()
}

fn builtin_names() -> Vec<String> {
    [
        // VCS internals (objects are the bulk of a repo).
        ".git",
        // JS
        "node_modules",
        ".pnpm-store",
        ".yarn",
        ".next",
        ".nuxt",
        ".parcel-cache",
        ".turbo",
        ".svelte-kit",
        // Rust / C / generic build output
        "target",
        "build",
        // Python
        "__pycache__",
        ".venv",
        "venv",
        ".mypy_cache",
        ".pytest_cache",
        ".tox",
        ".ruff_cache",
        // JVM / other caches
        ".gradle",
        ".m2",
        ".cargo",
        ".rustup",
        ".npm",
        ".bundle",
        // System / desktop noise
        ".cache",
        ".thumbnails",
        "lost+found",
        ".local/share/Trash",
        ".Trash-1000",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn builtin_patterns() -> Vec<Regex> {
    // `.git` as a name already excludes the whole directory, but the pattern is
    // retained so users who re-enable `.git` still skip its object store.
    BUILTIN_PATTERNS
        .iter()
        .filter_map(|p| Regex::new(p).ok())
        .collect()
}

const BUILTIN_PATTERNS: [&str; 1] = [r"(^|/)\.git/(objects|modules)(/|$)"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_normalisation() {
        assert_eq!(normalize_ext("foo.RS"), "rs");
        assert_eq!(normalize_ext("archive.tar.GZ"), "gz");
        assert_eq!(normalize_ext(".bashrc"), "");
        assert_eq!(normalize_ext("noext"), "");
    }
}
