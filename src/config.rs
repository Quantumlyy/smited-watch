//! Configuration file (TOML) parsing for smited-watch.
//!
//! Loads `watch.toml` from the user's config dir (or an explicit `--config`
//! path), applies defaults for omitted fields, and compiles each pattern's
//! regex up front so a bad regex fails fast with the offending pattern's
//! name in the error.
//!
//! The default config path follows the spec:
//!   * Linux/macOS: `$XDG_CONFIG_HOME/smited/watch.toml`,
//!     falling back to `~/.config/smited/watch.toml` when XDG is unset
//!   * Windows: `%APPDATA%\smited\watch.toml`
//!
//! When the user did not pass `--config` and the default path does not yet
//! exist, [`resolve_or_create`] writes a fully-commented template (the
//! `examples/wrap.toml` shipped with the binary) so the user has something
//! to edit instead of a blank slate. An explicit `--config` path that
//! doesn't exist is *not* auto-created — that's a user error.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use regex::Regex;
use serde::Deserialize;

/// Bundled annotated example config — also used as the auto-created template.
const TEMPLATE: &str = include_str!("../examples/wrap.toml");

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub smited: SmitedSection,
    #[serde(default)]
    pub patterns: Vec<Pattern>,
    #[serde(default)]
    pub on_exit: OnExit,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SmitedSection {
    pub host: Option<String>,
    pub backend_id: Option<String>,
    #[serde(default)]
    pub connection: Connection,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub strategy: ConnectionStrategy,
}

impl Default for Connection {
    fn default() -> Self {
        Self {
            timeout_ms: default_timeout_ms(),
            strategy: ConnectionStrategy::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStrategy {
    #[default]
    Persistent,
    PerTrigger,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pattern {
    pub name: String,
    pub regex: String,
    pub sensation: String,
    #[serde(default)]
    pub backend_id: Option<String>,
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    #[serde(default)]
    pub intensity_scale: Option<u32>,
    #[serde(default)]
    pub priority: Option<i32>,

    /// Populated by [`load`] after TOML parsing — never read from the file.
    #[serde(skip)]
    pub compiled: Option<Regex>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OnExit {
    #[serde(default)]
    pub success_sensation: String,
    #[serde(default)]
    pub failure_sensation: String,
    #[serde(default = "default_success_min_duration_ms")]
    pub success_min_duration_ms: u64,
    #[serde(default = "default_failure_dedupe_window_ms")]
    pub failure_dedupe_window_ms: u64,
}

impl Default for OnExit {
    fn default() -> Self {
        Self {
            success_sensation: String::new(),
            failure_sensation: String::new(),
            success_min_duration_ms: default_success_min_duration_ms(),
            failure_dedupe_window_ms: default_failure_dedupe_window_ms(),
        }
    }
}

fn default_timeout_ms() -> u64 {
    500
}
fn default_debounce_ms() -> u64 {
    500
}
fn default_success_min_duration_ms() -> u64 {
    30_000
}
fn default_failure_dedupe_window_ms() -> u64 {
    2_000
}

/// Read, parse, and validate a config file at the given path.
///
/// Compiles each pattern's regex; on failure, the returned error mentions
/// the pattern's `name` so users can find the offender in their config.
pub fn load(path: &Path) -> Result<Config> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("read config file at {}", path.display()))?;
    let mut cfg: Config = toml::from_str(&body)
        .with_context(|| format!("parse TOML config at {}", path.display()))?;

    for p in &mut cfg.patterns {
        let compiled = Regex::new(&p.regex)
            .map_err(|e| anyhow!("pattern {:?}: invalid regex {:?}: {e}", p.name, p.regex))?;
        if let Some(scale) = p.intensity_scale {
            if scale > 100 {
                return Err(anyhow!(
                    "pattern {:?}: intensity_scale {scale} out of range 0..=100",
                    p.name
                ));
            }
        }
        if let Some(prio) = p.priority {
            if !(-1000..=1000).contains(&prio) {
                return Err(anyhow!(
                    "pattern {:?}: priority {prio} out of range -1000..=1000",
                    p.name
                ));
            }
        }
        p.compiled = Some(compiled);
    }

    Ok(cfg)
}

/// Resolve the config path, auto-creating a template at the default location.
///
/// Returns `(path, was_auto_created)`. The boolean is true only when this
/// call wrote a fresh template at the default location — the caller can use
/// it to print a one-line "wrote default config to …" notice.
///
/// * `Some(path)` → returned as-is. Auto-creation is **not** attempted at
///   user-supplied paths; if the file doesn't exist [`load`] will error.
/// * `None` → resolves to [`default_config_path`]. If that file doesn't
///   exist yet, parent dirs are created and the bundled template is
///   written there.
pub fn resolve_or_create(explicit: Option<&Path>) -> Result<(PathBuf, bool)> {
    if let Some(p) = explicit {
        return Ok((p.to_path_buf(), false));
    }
    let path = default_config_path()?;
    if path.exists() {
        return Ok((path, false));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create config directory {}", parent.display()))?;
    }
    write_template(&path)?;
    Ok((path, true))
}

/// Write the bundled annotated template to `path`.
///
/// Overwrites if the file already exists. Callers that want a "don't
/// clobber" semantics should check `path.exists()` first; [`resolve_or_create`]
/// already does this.
pub fn write_template(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create config directory {}", parent.display()))?;
    }
    std::fs::write(path, TEMPLATE)
        .with_context(|| format!("write config template to {}", path.display()))?;
    Ok(())
}

/// Compute the default config path per the spec.
///
/// * Linux/macOS: `$XDG_CONFIG_HOME/smited/watch.toml` if set, else
///   `$HOME/.config/smited/watch.toml`.
/// * Windows: `%APPDATA%\smited\watch.toml`.
pub fn default_config_path() -> Result<PathBuf> {
    #[cfg(unix)]
    {
        let base = if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            PathBuf::from(xdg)
        } else {
            let home = std::env::var_os("HOME")
                .ok_or_else(|| anyhow!("$HOME is unset; cannot compute default config path"))?;
            PathBuf::from(home).join(".config")
        };
        Ok(base.join("smited").join("watch.toml"))
    }
    #[cfg(windows)]
    {
        let appdata = std::env::var_os("APPDATA")
            .ok_or_else(|| anyhow!("%APPDATA% is unset; cannot compute default config path"))?;
        Ok(PathBuf::from(appdata).join("smited").join("watch.toml"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(anyhow!("unsupported platform for default config path"))
    }
}
