//! Round-trip and validation tests for the TOML configuration schema.

use std::path::PathBuf;
use tempfile::TempDir;

use smited_watch::config::{self, ConnectionStrategy};

const FULL_CONFIG: &str = r#"
[smited]
host = "windows-rig.local:7777"
backend_id = "mock-owo"

[smited.connection]
timeout_ms = 750
strategy = "per_trigger"

[[patterns]]
name = "ts_error"
regex = 'error TS\d+'
sensation = "compile_error_mild"
debounce_ms = 500
intensity_scale = 75
priority = 25

[[patterns]]
name = "vite_error"
regex = '\[vite\].*error'
sensation = "compile_error_severe"
backend_id = "owo-primary"

[on_exit]
success_sensation = "deploy_success"
failure_sensation = "compile_error_severe"
success_min_duration_ms = 12000
failure_dedupe_window_ms = 1500
"#;

fn write_temp(contents: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("watch.toml");
    std::fs::write(&path, contents).unwrap();
    (dir, path)
}

#[test]
fn parses_fully_populated_config() {
    let (_dir, path) = write_temp(FULL_CONFIG);
    let cfg = config::load(&path).expect("config should load");

    assert_eq!(cfg.smited.host.as_deref(), Some("windows-rig.local:7777"));
    assert_eq!(cfg.smited.backend_id.as_deref(), Some("mock-owo"));
    assert_eq!(cfg.smited.connection.timeout_ms, 750);
    assert!(matches!(
        cfg.smited.connection.strategy,
        ConnectionStrategy::PerTrigger
    ));

    assert_eq!(cfg.patterns.len(), 2);

    let p0 = &cfg.patterns[0];
    assert_eq!(p0.name, "ts_error");
    assert_eq!(p0.regex, r"error TS\d+");
    assert_eq!(p0.sensation, "compile_error_mild");
    assert_eq!(p0.debounce_ms, 500);
    assert_eq!(p0.intensity_scale, Some(75));
    assert_eq!(p0.priority, Some(25));
    assert!(
        p0.compiled.is_some(),
        "regex should be compiled at load time"
    );
    assert!(p0.compiled.as_ref().unwrap().is_match("error TS1234"));

    let p1 = &cfg.patterns[1];
    assert_eq!(p1.name, "vite_error");
    assert_eq!(p1.backend_id.as_deref(), Some("owo-primary"));
    assert_eq!(p1.debounce_ms, 500, "debounce_ms default = 500");
    assert_eq!(p1.intensity_scale, None);

    assert_eq!(cfg.on_exit.success_sensation, "deploy_success");
    assert_eq!(cfg.on_exit.failure_sensation, "compile_error_severe");
    assert_eq!(cfg.on_exit.success_min_duration_ms, 12000);
    assert_eq!(cfg.on_exit.failure_dedupe_window_ms, 1500);
}

#[test]
fn applies_defaults_when_optional_fields_omitted() {
    // Minimal config — every optional field omitted. Defaults must apply.
    let (_dir, path) = write_temp(
        r#"
[[patterns]]
name = "x"
regex = "x"
sensation = "x"
"#,
    );
    let cfg = config::load(&path).expect("config should load");

    assert_eq!(cfg.smited.host, None);
    assert_eq!(cfg.smited.backend_id, None);
    assert_eq!(cfg.smited.connection.timeout_ms, 500);
    assert!(matches!(
        cfg.smited.connection.strategy,
        ConnectionStrategy::Persistent
    ));

    assert_eq!(cfg.patterns[0].debounce_ms, 500);
    assert!(cfg.patterns[0].backend_id.is_none());
    assert!(cfg.patterns[0].intensity_scale.is_none());
    assert!(cfg.patterns[0].priority.is_none());

    assert_eq!(cfg.on_exit.success_sensation, "");
    assert_eq!(cfg.on_exit.failure_sensation, "");
    assert_eq!(cfg.on_exit.success_min_duration_ms, 30_000);
    assert_eq!(cfg.on_exit.failure_dedupe_window_ms, 2_000);
}

#[test]
fn rejects_malformed_toml() {
    let (_dir, path) = write_temp("this is not toml = = =");
    let err = config::load(&path).expect_err("malformed TOML should error");
    let msg = format!("{err:#}");
    assert!(
        msg.to_lowercase().contains("toml") || msg.to_lowercase().contains("parse"),
        "error should mention TOML/parse, got: {msg}"
    );
}

#[test]
fn rejects_unparseable_pattern_regex_with_pattern_name_in_error() {
    let (_dir, path) = write_temp(
        r#"
[[patterns]]
name = "broken_pattern"
regex = "(unclosed"
sensation = "x"
"#,
    );
    let err = config::load(&path).expect_err("bad regex should error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("broken_pattern"),
        "error should mention pattern name 'broken_pattern', got: {msg}"
    );
}

#[test]
fn empty_on_exit_sensations_are_allowed() {
    let (_dir, path) = write_temp(
        r#"
[on_exit]
success_sensation = ""
failure_sensation = ""
"#,
    );
    let cfg = config::load(&path).expect("empty sensations are valid");
    assert_eq!(cfg.on_exit.success_sensation, "");
    assert_eq!(cfg.on_exit.failure_sensation, "");
}

#[test]
fn explicit_config_path_must_exist() {
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("nope.toml");
    let err = config::load(&missing).expect_err("missing config file should error");
    let msg = format!("{err:#}");
    assert!(
        msg.to_lowercase().contains("nope.toml")
            || msg.to_lowercase().contains("not found")
            || msg.to_lowercase().contains("no such"),
        "error should describe missing file, got: {msg}"
    );
}

#[test]
fn write_template_produces_a_valid_loadable_config() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("watch.toml");
    config::write_template(&path).expect("template should write");

    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("[smited]"), "template missing [smited]");
    assert!(
        body.contains("[[patterns]]"),
        "template missing [[patterns]]"
    );
    assert!(body.contains("[on_exit]"), "template missing [on_exit]");

    // Roundtrip — the template must itself be valid.
    let cfg = config::load(&path).expect("template should load cleanly");
    assert!(
        !cfg.patterns.is_empty(),
        "template should ship example patterns"
    );
}

#[test]
fn auto_created_template_is_dry_run_by_default() {
    // First-run UX contract: writing the template and immediately loading
    // it must produce a config with NO `host` set, so the watcher runs in
    // dry-run mode rather than trying to contact the example daemon
    // (`windows-rig.local:7777`) that the user almost certainly does not
    // have on their network. The example value lives in the file as a
    // commented-out hint instead.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("watch.toml");
    config::write_template(&path).expect("template should write");
    let cfg = config::load(&path).expect("template should load");
    assert!(
        cfg.smited.host.is_none(),
        "auto-created template must leave [smited].host unset so first run is dry-run; \
         got host={:?}",
        cfg.smited.host
    );
}

#[test]
fn known_strategy_strings_parse() {
    for (s, expect_per_trigger) in [("persistent", false), ("per_trigger", true)] {
        let (_dir, path) = write_temp(&format!(
            r#"
[smited.connection]
strategy = "{s}"
"#
        ));
        let cfg = config::load(&path).unwrap();
        assert_eq!(
            matches!(
                cfg.smited.connection.strategy,
                ConnectionStrategy::PerTrigger
            ),
            expect_per_trigger,
            "strategy={s}"
        );
    }
}

#[test]
fn unknown_strategy_string_errors() {
    let (_dir, path) = write_temp(
        r#"
[smited.connection]
strategy = "carrier-pigeon"
"#,
    );
    let err = config::load(&path).expect_err("unknown strategy should error");
    let msg = format!("{err:#}").to_lowercase();
    assert!(
        msg.contains("strategy") || msg.contains("carrier-pigeon"),
        "error should mention the bad strategy, got: {msg}"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// XDG_CONFIG_HOME resolution
//
// These tests mutate process env vars, so they must run serialised.
// Cargo's test runner parallelises by default; without a lock, a sibling
// test reading XDG_CONFIG_HOME would see whatever the previous test left
// behind. Tests live in this single test binary, sharing a process —
// the lock here is sufficient.
//
// `unwrap_or_else(|e| e.into_inner())` lets us recover from a panicking
// earlier test poisoning the mutex; otherwise one bad test would block
// every other XDG-related test in the same `cargo test` invocation.
// ─────────────────────────────────────────────────────────────────────────

#[cfg(unix)]
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Snapshot/restore guard for the small set of env vars these tests
/// touch — so even if a test panics mid-way, the next one starts from
/// the inherited environment.
#[cfg(unix)]
struct EnvRestore {
    xdg: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
}

#[cfg(unix)]
impl EnvRestore {
    fn capture() -> Self {
        Self {
            xdg: std::env::var_os("XDG_CONFIG_HOME"),
            home: std::env::var_os("HOME"),
        }
    }
}

#[cfg(unix)]
impl Drop for EnvRestore {
    fn drop(&mut self) {
        // SAFETY: env mutation is unsafe in 2024-edition std; here we're
        // explicitly serialising via env_lock() so concurrent reads
        // can't observe partially-updated state.
        unsafe {
            match &self.xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            match &self.home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn xdg_config_home_set_and_non_empty_is_used_directly() {
    let _g = env_lock();
    let _r = EnvRestore::capture();
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", "/custom/xdg");
    }
    let path = config::default_config_path().expect("default path");
    assert_eq!(path, PathBuf::from("/custom/xdg/smited/watch.toml"));
}

#[cfg(unix)]
#[test]
fn xdg_config_home_unset_falls_back_to_home_config() {
    let _g = env_lock();
    let _r = EnvRestore::capture();
    unsafe {
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::set_var("HOME", "/home/test");
    }
    let path = config::default_config_path().expect("default path");
    assert_eq!(path, PathBuf::from("/home/test/.config/smited/watch.toml"));
}

/// The bug fixed in this round: `XDG_CONFIG_HOME=""` (set but empty)
/// was being passed straight to `PathBuf::from`, producing the relative
/// path `smited/watch.toml` and resolving against the current working
/// directory — so smited-watch would read/write a different config
/// depending on which directory it was launched from. The XDG spec
/// requires empty-equals-unset for exactly this reason.
#[cfg(unix)]
#[test]
fn xdg_config_home_empty_falls_back_to_home_config() {
    let _g = env_lock();
    let _r = EnvRestore::capture();
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", "");
        std::env::set_var("HOME", "/home/test");
    }
    let path = config::default_config_path().expect("default path");
    assert_eq!(
        path,
        PathBuf::from("/home/test/.config/smited/watch.toml"),
        "empty XDG_CONFIG_HOME must fall back to $HOME/.config per XDG spec, \
         not produce a relative path"
    );
    // Belt-and-braces: the path is absolute (no relative-path leak).
    assert!(
        path.is_absolute(),
        "default config path must be absolute; got {}",
        path.display()
    );
}
