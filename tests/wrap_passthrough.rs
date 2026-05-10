//! End-to-end passthrough tests: run the built `smited-watch` binary and
//! verify stdio is forwarded byte-perfect and the exit code propagates.

use std::process::Command;

use assert_cmd::cargo::CommandCargoExt;
use tempfile::TempDir;

/// Build a minimal config that:
/// * doesn't auto-create anywhere by being explicit (`--config <path>`)
/// * has no host (dry-run mode)
/// * has no patterns and no on-exit sensations (no scanner noise)
fn empty_config() -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("watch.toml");
    std::fs::write(
        &path,
        r#"
[smited]
[on_exit]
success_sensation = ""
failure_sensation = ""
"#,
    )
    .unwrap();
    (dir, path)
}

fn binary(config_path: &std::path::Path) -> Command {
    let mut c = Command::cargo_bin("smited-watch").expect("binary built by cargo");
    c.arg("--no-banner")
        .arg("--quiet")
        .arg("--config")
        .arg(config_path);
    // Belt-and-braces: also point the env-var equivalent, in case clap prefers it.
    c.env("SMITED_WATCH_CONFIG", config_path);
    c
}

#[test]
fn stdout_is_forwarded_verbatim() {
    let (_dir, cfg) = empty_config();
    let out = binary(&cfg)
        .arg("--")
        .arg("echo")
        .arg("hello")
        .output()
        .expect("run smited-watch");
    assert!(out.status.success(), "exit code = {:?}", out.status.code());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello\n");
}

#[test]
fn stderr_is_forwarded_verbatim() {
    let (_dir, cfg) = empty_config();
    let out = binary(&cfg)
        .arg("--")
        .arg("bash")
        .arg("-c")
        .arg("echo to-stderr 1>&2")
        .output()
        .expect("run smited-watch");
    assert!(out.status.success(), "exit code = {:?}", out.status.code());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "to-stderr\n",
        "stderr should contain ONLY the child's stderr (we passed --quiet)"
    );
}

#[test]
fn exit_code_propagates_nonzero() {
    let (_dir, cfg) = empty_config();
    let out = binary(&cfg)
        .arg("--")
        .arg("bash")
        .arg("-c")
        .arg("exit 7")
        .output()
        .expect("run smited-watch");
    assert_eq!(out.status.code(), Some(7));
}

#[test]
fn one_megabyte_stdout_does_not_deadlock_or_drop_bytes() {
    // 1 MB of deterministic, predictable bytes. We compare lengths rather
    // than full payloads to keep the assert message readable on failure.
    let (_dir, cfg) = empty_config();
    let out = binary(&cfg)
        .arg("--")
        .arg("bash")
        .arg("-c")
        .arg("head -c 1000000 /dev/zero")
        .output()
        .expect("run smited-watch");
    assert!(out.status.success(), "exit code = {:?}", out.status.code());
    assert_eq!(
        out.stdout.len(),
        1_000_000,
        "expected 1MB of zero-bytes through stdout, got {} bytes",
        out.stdout.len()
    );
    assert!(
        out.stdout.iter().all(|&b| b == 0),
        "every byte should be 0 (the input was /dev/zero)"
    );
}

#[test]
fn smited_watch_disable_envvar_makes_it_pure_passthrough() {
    let (_dir, cfg) = empty_config();
    let out = binary(&cfg)
        .env("SMITED_WATCH_DISABLE", "1")
        .arg("--")
        .arg("echo")
        .arg("hi")
        .output()
        .expect("run smited-watch");
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
}

#[test]
fn no_command_prints_help_and_exits_nonzero() {
    let (_dir, cfg) = empty_config();
    let out = binary(&cfg).output().expect("run smited-watch");
    assert_ne!(out.status.code(), Some(0), "exit code should be non-zero");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        stderr.as_ref()
    );
    assert!(
        combined.contains("Usage:") || combined.contains("usage:"),
        "no-command run should print help; got:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        stderr,
    );
}
