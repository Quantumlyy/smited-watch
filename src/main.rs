use std::process::ExitCode;

use clap::Parser;
use smited_watch::cli::{tracing_filter, Cli, DEFAULT_BACKEND_ID};

fn main() -> ExitCode {
    let cli = Cli::parse();

    // Tracing first, so config-resolution errors render through tracing.
    init_tracing(&cli);

    if cli.command.is_empty() {
        // Spec: "With no command, print help and exit non-zero."
        let mut cmd = <Cli as clap::CommandFactory>::command();
        let _ = cmd.print_help();
        eprintln!();
        return ExitCode::from(2);
    }

    // SMITED_WATCH_DISABLE=1 → pure passthrough.
    let disabled = std::env::var("SMITED_WATCH_DISABLE")
        .map(|v| v == "1")
        .unwrap_or(false);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[smited-watch] failed to start tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };

    let exit_code = runtime.block_on(async move {
        if disabled {
            return run_passthrough(cli).await;
        }
        run_with_config(cli).await
    });

    match exit_code {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("[smited-watch] {e:#}");
            ExitCode::from(1)
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

    if !cli.no_banner {
        let banner = banner_line(&cli.command, dry_run);
        eprintln!("{banner}");
    }
    if was_auto_created {
        eprintln!(
            "[smited-watch] wrote default config template to {} — edit it to add patterns",
            config_path.display()
        );
    }

    let opts = build_wrap_options(cli, config, host_for_client, default_backend_id);
    let status = smited_watch::wrap::run(opts).await?;
    Ok(propagate_exit_code(status))
}

/// SMITED_WATCH_DISABLE=1 path: spawn the command, pipe stdio, exit with
/// its code. No config load, no scanning, no triggers.
async fn run_passthrough(cli: Cli) -> anyhow::Result<i32> {
    let opts = smited_watch::wrap::WrapOptions {
        cmd: cli.command,
        patterns: std::sync::Arc::new(Vec::new()),
        debouncers: std::sync::Arc::new(Vec::new()),
        trigger_client: std::sync::Arc::new(smited_watch::client::TriggerClient::new(
            None,
            smited_watch::config::ConnectionStrategy::Persistent,
            std::time::Duration::from_millis(500),
        )),
        default_backend_id: DEFAULT_BACKEND_ID.to_string(),
        on_exit: smited_watch::config::OnExit::default(),
        force_pipes: false,
    };
    let status = smited_watch::wrap::run(opts).await?;
    Ok(propagate_exit_code(status))
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
