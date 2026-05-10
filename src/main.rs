use clap::Parser;
use smited_watch::cli::{tracing_filter, Cli, DEFAULT_BACKEND_ID};

fn main() -> ! {
    let cli = Cli::parse();

    // Tracing first, so config-resolution errors render through tracing.
    init_tracing(&cli);

    if cli.command.is_empty() {
        // Spec: "With no command, print help and exit non-zero."
        let mut cmd = <Cli as clap::CommandFactory>::command();
        let _ = cmd.print_help();
        eprintln!();
        std::process::exit(2);
    }

    // SMITED_WATCH_DISABLE=1 → pure passthrough. The spec says: "spawns
    // the command, pipes stdio, exits with the inner code, fires no
    // triggers." We bypass `wrap::run` *entirely* — no PTY allocation,
    // no scanner, no tee, no tokio runtime. The child inherits our
    // stdin/stdout/stderr unmodified, so its TTY-detection logic sees
    // exactly what it would have seen without the wrapper.
    if std::env::var("SMITED_WATCH_DISABLE").as_deref() == Ok("1") {
        std::process::exit(run_disabled_passthrough(cli.command));
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[smited-watch] failed to start tokio runtime: {e}");
            std::process::exit(1);
        }
    };

    let exit_code = runtime.block_on(async move { run_with_config(cli).await });

    // We deliberately use `std::process::exit(i32)` rather than returning
    // `std::process::ExitCode::from(u8)` so the wrapped command's full
    // exit code propagates on Windows. Windows process exit codes are
    // 32-bit (e.g. MSI installers commonly use 3010 to mean "reboot
    // required"); truncating to a u8 would silently break the spec's
    // "exit with the same exit code as the inner command" guarantee.
    // On Unix the kernel masks to the low 8 bits regardless, so this
    // is a no-op there.
    match exit_code {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("[smited-watch] {e:#}");
            std::process::exit(1);
        }
    }
}

fn init_tracing(cli: &Cli) {
    use tracing_subscriber::{fmt, EnvFilter};
    // Allow `RUST_LOG` to override our derived filter for power users.
    let filter =
        std::env::var("RUST_LOG").unwrap_or_else(|_| tracing_filter(cli.verbose, cli.quiet));
    let subscriber = fmt()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_env_filter(EnvFilter::new(filter))
        .with_ansi(false);
    let _ = subscriber.try_init();
}

/// Build orchestration objects from a fully-resolved [`Cli`] + config and
/// hand off to [`smited_watch::wrap::run`].
async fn run_with_config(cli: Cli) -> anyhow::Result<i32> {
    let (config_path, was_auto_created) =
        smited_watch::config::resolve_or_create(cli.config.as_deref())?;
    let mut config = smited_watch::config::load(&config_path)?;

    // CLI overrides on top of config.
    if let Some(h) = cli.host.clone() {
        config.smited.host = Some(h);
    }
    if let Some(b) = cli.backend_id.clone() {
        config.smited.backend_id = Some(b);
    }

    let host_for_client = if cli.dry_run {
        None
    } else {
        config.smited.host.clone()
    };
    let dry_run = host_for_client.is_none();

    let default_backend_id = config
        .smited
        .backend_id
        .clone()
        .unwrap_or_else(|| DEFAULT_BACKEND_ID.to_string());

    // --quiet suppresses every smited-watch-originated stderr line, including
    // the banner and the auto-created-config notice. The wrapped command's
    // own output is unaffected.
    if !cli.quiet && !cli.no_banner {
        let banner = banner_line(&cli.command, dry_run);
        eprintln!("{banner}");
    }
    if !cli.quiet && was_auto_created {
        eprintln!(
            "[smited-watch] wrote default config template to {} — edit it to add patterns",
            config_path.display()
        );
    }

    let opts = build_wrap_options(cli, config, host_for_client, default_backend_id);
    let status = smited_watch::wrap::run(opts).await?;
    Ok(propagate_exit_code(status))
}

/// `SMITED_WATCH_DISABLE=1` passthrough: synchronously spawn the wrapped
/// command with all three stdio handles inherited from the parent, wait
/// for it to exit, propagate the exit code. No tokio, no PTY, no
/// scanning, no triggers.
///
/// The child inherits our pgrp by default (we do *not* call
/// `process_group(0)`), so a Ctrl-C in the parent terminal reaches both
/// us and the child via the kernel's foreground-pgrp delivery — no
/// signal-handler plumbing needed.
fn run_disabled_passthrough(cmd: Vec<std::ffi::OsString>) -> i32 {
    use std::process::{Command, Stdio};
    if cmd.is_empty() {
        eprintln!("[smited-watch] disable mode: no command given");
        return 1;
    }
    let mut child = match Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[smited-watch] failed to spawn {:?}: {e}", cmd[0]);
            return 1;
        }
    };
    match child.wait() {
        Ok(status) => propagate_exit_code(status),
        Err(e) => {
            eprintln!("[smited-watch] failed to wait on child: {e}");
            1
        }
    }
}

fn build_wrap_options(
    cli: Cli,
    config: smited_watch::config::Config,
    host_for_client: Option<String>,
    default_backend_id: String,
) -> smited_watch::wrap::WrapOptions {
    use std::sync::Arc;
    use std::time::Duration;

    let timeout = Duration::from_millis(config.smited.connection.timeout_ms);
    let strategy = config.smited.connection.strategy;
    let trigger_client = Arc::new(smited_watch::client::TriggerClient::new(
        host_for_client,
        strategy,
        timeout,
    ));

    let debouncers: Vec<smited_watch::debounce::Debouncer> = config
        .patterns
        .iter()
        .map(|p| smited_watch::debounce::Debouncer::new(Duration::from_millis(p.debounce_ms)))
        .collect();

    smited_watch::wrap::WrapOptions {
        cmd: cli.command,
        patterns: Arc::new(config.patterns),
        debouncers: Arc::new(debouncers),
        trigger_client,
        default_backend_id,
        on_exit: config.on_exit,
        force_pipes: false,
    }
}

/// Translate a child's [`std::process::ExitStatus`] into an exit code we
/// can hand to `ExitCode::from(...)`. Mirrors the standard Unix shell
/// convention so that `cargo build && smited-watch -- ...` and CI
/// pipelines treat signal-killed children the same way the shell would
/// have if the user had run the command directly.
///
/// * Normal exit → propagate the exit code unchanged
/// * Signal-killed (Unix) → `128 + signum` (e.g. SIGINT = 130, SIGTERM = 143, SIGKILL = 137)
/// * Anything else (Windows non-zero with no code, etc.) → 1
fn propagate_exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    1
}

fn banner_line(cmd: &[std::ffi::OsString], dry_run: bool) -> String {
    let joined = cmd
        .iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    if dry_run {
        format!("[smited-watch] wrapping {joined} (dry-run)")
    } else {
        format!("[smited-watch] wrapping {joined}")
    }
}
