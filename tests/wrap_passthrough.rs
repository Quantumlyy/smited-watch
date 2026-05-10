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

#[cfg(unix)]
#[test]
fn sigint_is_forwarded_as_sigint_not_sigkill() {
    // Send SIGINT to smited-watch and verify the wrapped command exits
    // with signal=SIGINT (signum 2), proving the wrapper forwarded the
    // exact signal it received rather than calling kill()/start_kill()
    // which would send SIGKILL (signum 9).
    //
    // We don't try to verify a SIGINT trap ran inside the child — bash
    // defers traps while waiting on `sleep` in the foreground, which
    // makes that test brittle. The signum check directly verifies the
    // bug fix without depending on shell-specific trap timing.
    use std::os::unix::process::ExitStatusExt;

    let (_dir, cfg) = empty_config();
    let mut child = binary(&cfg)
        .arg("--")
        .arg("sleep")
        .arg("30")
        .spawn()
        .expect("spawn smited-watch");
    let pid = child.id();
    // Give the wrapper time to initialize its tokio runtime, spawn the
    // child sleep, and install signal handlers. Generous because cargo
    // test runs tests in parallel and the system can be loaded.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    // SAFETY: we just spawned `child`; its PID is valid for our lifetime.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGINT);
    }
    let status = child.wait().expect("wait on smited-watch");
    // smited-watch propagates child-killed-by-signal as 128 + signum.
    // SIGINT = 2 ⇒ exit code 130. If we'd sent SIGKILL (signum 9) instead,
    // we'd see 137. Anything else proves the signal forwarding bug.
    assert_eq!(
        status.code(),
        Some(128 + libc::SIGINT),
        "smited-watch should propagate child-killed-by-SIGINT as 130 \
         (128+SIGINT), proving the wrapper sent SIGINT — not SIGKILL — to \
         the child; got status={status:?} (code={:?}, signal={:?})",
        status.code(),
        status.signal()
    );
}

#[test]
fn final_line_without_trailing_newline_still_passes_through() {
    // `printf` (unlike `echo`) does NOT append a trailing newline, so
    // this exercises the scanner-flush-after-drain ordering: the trailing
    // line must still reach the parent's stdout.
    let (_dir, cfg) = empty_config();
    let out = binary(&cfg)
        .arg("--")
        .arg("printf")
        .arg("trailing-no-newline")
        .output()
        .expect("run smited-watch");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "trailing-no-newline",
        "trailing line without \\n must still pass through byte-perfect"
    );
}

#[test]
fn quiet_flag_suppresses_banner_and_auto_create_notice() {
    // The empty_config helper passes --quiet for noise reasons; build a
    // raw command without the helper so we can prove --quiet works.
    let (_dir, cfg) = empty_config();
    let mut c = Command::cargo_bin("smited-watch").expect("binary built by cargo");
    let out = c
        // No --no-banner: --quiet alone should suppress everything we emit.
        .arg("--quiet")
        .arg("--config")
        .arg(&cfg)
        .arg("--")
        .arg("echo")
        .arg("hi")
        .output()
        .expect("run smited-watch");
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
    assert_eq!(
        out.stderr.len(),
        0,
        "--quiet must suppress banner + auto-create notice + tracing output; got stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(unix)]
#[test]
fn sigterm_to_wrapper_kills_descendants_via_process_group() {
    // Wrap a bash script that backgrounds a `sleep` and writes its PID
    // to a file. After we send SIGTERM to the wrapper, the wrapper
    // forwards SIGTERM to the child *process group*, so the descendant
    // sleep must also die. Pre-fix, signalling only the bash PID would
    // leave sleep orphaned.
    //
    // Why SIGTERM (not SIGINT) for this test: in non-interactive mode
    // bash explicitly *ignores* SIGINT in backgrounded jobs (Bash man:
    // "When job control is not in effect, asynchronous commands ignore
    // SIGINT and SIGQUIT"). SIGTERM has no such carve-out, so it
    // exercises pgrp forwarding cleanly without bash semantics in the way.
    let (dir, cfg) = empty_config();
    let pid_file = dir.path().join("sleep.pid");
    let pid_file_str = pid_file.display().to_string();
    // Use a long sleep so a regression — where SIGINT only reaches the
    // immediate child and `sleep` runs to natural completion — would be
    // observable as the test timing out, not as the test silently passing.
    let script = format!("sleep 60 & echo $! > {pid_file_str}; wait");
    let mut child = binary(&cfg)
        .arg("--")
        .arg("bash")
        .arg("-c")
        .arg(&script)
        .spawn()
        .expect("spawn smited-watch");
    let wrapper_pid = child.id();

    // Wait for bash to write the descendant PID.
    let descendant_pid: i32 = {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Ok(s) = std::fs::read_to_string(&pid_file) {
                if let Ok(n) = s.trim().parse::<i32>() {
                    break n;
                }
            }
            if std::time::Instant::now() > deadline {
                let _ = unsafe { libc::kill(wrapper_pid as libc::pid_t, libc::SIGKILL) };
                let _ = child.wait();
                panic!("sleep PID file never appeared; bash didn't start");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    };

    let signaled_at = std::time::Instant::now();
    unsafe {
        libc::kill(wrapper_pid as libc::pid_t, libc::SIGTERM);
    }
    let _ = child.wait().expect("wait on smited-watch");
    let elapsed = signaled_at.elapsed();

    // Hard ceiling: if pgrp forwarding works, the wrapper should exit
    // within a few seconds (signal delivery + 1s trigger drain). If we
    // had signalled only the bash PID, bash's `wait` would block on the
    // backgrounded sleep until sleep finishes naturally at 60s.
    // 10 seconds gives ample headroom for a slow CI without letting a
    // 60s-sleep regression slip through.
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "wrapper took {elapsed:?} to exit after SIGTERM — pgrp forwarding probably broken \
         (descendant sleep would have run to natural completion at 60s)"
    );

    // Give the kernel a moment to reap the descendant.
    std::thread::sleep(std::time::Duration::from_millis(200));
    let alive = unsafe { libc::kill(descendant_pid as libc::pid_t, 0) } == 0;
    if alive {
        // Don't leak the descendant if the fix is wrong.
        unsafe {
            libc::kill(descendant_pid as libc::pid_t, libc::SIGKILL);
        }
        panic!(
            "descendant sleep (pid {descendant_pid}) survived after wrapper exit — \
             SIGINT was not forwarded to the child process group"
        );
    }
}

#[cfg(unix)]
#[test]
fn child_reading_stdin_does_not_get_sigttin_when_stdin_is_a_tty() {
    // The reviewer's #2 scenario: `smited-watch -- bash -c 'read -t 1 x' >out`
    // hangs pre-fix because the child gets `process_group(0)` and is
    // therefore in a *background* pgrp relative to the parent's
    // controlling TTY; reading stdin then triggers SIGTTIN and stops it.
    //
    // Reproducing that bug requires stdin to actually be a TTY, which a
    // captured cargo-test child doesn't have. We open /dev/tty directly
    // as the wrapper's stdin so the same kernel rules kick in. If
    // /dev/tty isn't available (CI without a controlling terminal), the
    // test skips rather than failing — there's no portable way to fake
    // a controlling TTY.
    let tty = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
    {
        Ok(t) => t,
        Err(_) => {
            eprintln!("SKIP: /dev/tty not available in this environment");
            return;
        }
    };

    let (_dir, cfg) = empty_config();
    let started = std::time::Instant::now();
    // `read -t 1 x` returns nonzero on timeout but does NOT hang. The
    // bug presents as the wrapper waiting much longer than 1 second
    // because bash is suspended on SIGTTIN and never gets to time out.
    let out = binary(&cfg)
        .stdin(tty)
        .arg("--")
        .arg("bash")
        .arg("-c")
        .arg("read -t 1 x; echo exit=$?")
        .output()
        .expect("run smited-watch");
    let elapsed = started.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "wrapper took {elapsed:?} — bash's `read -t 1` should time out in 1s; \
         a long wait means the child was suspended on SIGTTIN"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("exit="),
        "bash should have run to completion and printed its exit line; \
         got stdout={stdout:?}, status={:?}",
        out.status
    );
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
