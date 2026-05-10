//! End-to-end integration test against an in-process tonic server.
//!
//! Spins up a fake `SmitedService.Trigger` endpoint on a random local port,
//! runs the built smited-watch binary against it with a config that
//! matches `error TS\d+`, and asserts the daemon received exactly the
//! expected `TriggerRequest`s.

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use assert_cmd::cargo::CommandCargoExt;
use tempfile::TempDir;
use tokio::sync::Mutex;
use tonic::transport::Server;
use tonic::{Request, Response, Status};

use smited_watch::proto::smited::v1::{
    smited_service_server::{SmitedService, SmitedServiceServer},
    BackendStatus, BackendSummary, DescribeBackendRequest, DescribeBackendResponse, HealthRequest,
    HealthResponse, ListBackendsRequest, ListBackendsResponse, ListSensationsRequest,
    ListSensationsResponse, RegisterSensationRequest, RegisterSensationResponse, StopRequest,
    StopResponse, SubscribeEventsRequest, TriggerRequest, TriggerResponse,
    UnregisterSensationRequest, UnregisterSensationResponse,
};

/// Capture every TriggerRequest received during the test.
type Capture = Arc<Mutex<Vec<TriggerRequest>>>;

struct FakeDaemon {
    captured: Capture,
}

#[tonic::async_trait]
impl SmitedService for FakeDaemon {
    async fn list_backends(
        &self,
        _req: Request<ListBackendsRequest>,
    ) -> Result<Response<ListBackendsResponse>, Status> {
        Ok(Response::new(ListBackendsResponse {
            backends: vec![BackendSummary {
                id: "mock-owo".into(),
                kind: "mock".into(),
                display_name: "Mock OWO".into(),
                status: BackendStatus::Ready as i32,
                capabilities: vec!["vibration".into()],
            }],
        }))
    }
    async fn describe_backend(
        &self,
        _req: Request<DescribeBackendRequest>,
    ) -> Result<Response<DescribeBackendResponse>, Status> {
        Err(Status::unimplemented("not used in tests"))
    }
    async fn trigger(
        &self,
        req: Request<TriggerRequest>,
    ) -> Result<Response<TriggerResponse>, Status> {
        let req = req.into_inner();
        let trace = req.client_trace_id.clone();
        self.captured.lock().await.push(req);
        Ok(Response::new(TriggerResponse {
            accepted: true,
            sensation_id: "sens-1".into(),
            client_trace_id: trace,
            error: None,
        }))
    }
    async fn stop(&self, _req: Request<StopRequest>) -> Result<Response<StopResponse>, Status> {
        Err(Status::unimplemented("not used in tests"))
    }
    async fn list_sensations(
        &self,
        _req: Request<ListSensationsRequest>,
    ) -> Result<Response<ListSensationsResponse>, Status> {
        Err(Status::unimplemented("not used in tests"))
    }
    async fn register_sensation(
        &self,
        _req: Request<RegisterSensationRequest>,
    ) -> Result<Response<RegisterSensationResponse>, Status> {
        Err(Status::unimplemented("not used in tests"))
    }
    async fn unregister_sensation(
        &self,
        _req: Request<UnregisterSensationRequest>,
    ) -> Result<Response<UnregisterSensationResponse>, Status> {
        Err(Status::unimplemented("not used in tests"))
    }
    async fn health(
        &self,
        _req: Request<HealthRequest>,
    ) -> Result<Response<HealthResponse>, Status> {
        Err(Status::unimplemented("not used in tests"))
    }
    type SubscribeEventsStream = std::pin::Pin<
        Box<
            dyn tokio_stream::Stream<Item = Result<smited_watch::proto::smited::v1::Event, Status>>
                + Send,
        >,
    >;
    async fn subscribe_events(
        &self,
        _req: Request<SubscribeEventsRequest>,
    ) -> Result<Response<Self::SubscribeEventsStream>, Status> {
        Err(Status::unimplemented("not used in tests"))
    }
}

/// Spin up the fake daemon on `127.0.0.1:0` and return `(addr, capture, shutdown_tx)`.
async fn spawn_fake_daemon() -> (
    std::net::SocketAddr,
    Capture,
    tokio::sync::oneshot::Sender<()>,
) {
    let captured: Capture = Arc::new(Mutex::new(Vec::new()));
    let svc = FakeDaemon {
        captured: captured.clone(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        let _ = Server::builder()
            .add_service(SmitedServiceServer::new(svc))
            .serve_with_incoming_shutdown(incoming, async {
                let _ = rx.await;
            })
            .await;
    });
    // Tiny grace period so the server is ready before we start firing.
    tokio::time::sleep(Duration::from_millis(50)).await;
    (addr, captured, tx)
}

fn write_match_config(host: &str) -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("watch.toml");
    std::fs::write(
        &path,
        format!(
            r#"
[smited]
host = "{host}"
backend_id = "mock-owo"

[smited.connection]
timeout_ms = 1500
strategy = "per_trigger"

[[patterns]]
name = "ts"
regex = 'error TS\d+'
sensation = "compile_error_mild"
debounce_ms = 0

[on_exit]
success_sensation = "deploy_success"
failure_sensation = ""
success_min_duration_ms = 1
failure_dedupe_window_ms = 2000
"#
        ),
    )
    .unwrap();
    (dir, path)
}

/// Variant with patterns active but BOTH on-exit sensations disabled.
/// Lets a test isolate "did a pattern fire?" from "did on-exit fire?"
/// without having to filter out the latter from the captured set.
fn write_pattern_only_config(host: &str) -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("watch.toml");
    std::fs::write(
        &path,
        format!(
            r#"
[smited]
host = "{host}"
backend_id = "mock-owo"

[smited.connection]
timeout_ms = 1500
strategy = "per_trigger"

[[patterns]]
name = "ts"
regex = 'error TS\d+'
sensation = "compile_error_mild"
debounce_ms = 0

[on_exit]
success_sensation = ""
failure_sensation = ""
success_min_duration_ms = 30000
failure_dedupe_window_ms = 2000
"#
        ),
    )
    .unwrap();
    (dir, path)
}

/// Variant with a `failure_sensation` set so we can verify the on-exit
/// failure path (and its suppression branches) at the wire level.
fn write_failure_config(host: &str) -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("watch.toml");
    std::fs::write(
        &path,
        format!(
            r#"
[smited]
host = "{host}"
backend_id = "mock-owo"

[smited.connection]
timeout_ms = 1500
strategy = "per_trigger"

[on_exit]
success_sensation = ""
failure_sensation = "build_failed"
success_min_duration_ms = 30000
failure_dedupe_window_ms = 0
"#
        ),
    )
    .unwrap();
    (dir, path)
}

fn run_binary_with(env_disable: bool, dry_run: bool, cfg: &std::path::Path, cmd: &str) -> i32 {
    let mut c = Command::cargo_bin("smited-watch").expect("binary built");
    c.arg("--no-banner").arg("--quiet").arg("--config").arg(cfg);
    if env_disable {
        c.env("SMITED_WATCH_DISABLE", "1");
    }
    if dry_run {
        c.arg("--dry-run");
    }
    c.arg("--").arg("bash").arg("-c").arg(cmd);
    c.stdout(Stdio::null()).stderr(Stdio::null());
    let status = c.status().expect("run smited-watch");
    status.code().unwrap_or(-1)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn matched_pattern_fires_one_trigger_with_expected_sensation() {
    let (addr, captured, shutdown) = spawn_fake_daemon().await;
    let (_dir, cfg) = write_match_config(&addr.to_string());

    // Run the binary in a blocking thread so we can keep the tonic server
    // running on the current runtime.
    let cfg_owned = cfg.clone();
    let exit = tokio::task::spawn_blocking(move || {
        run_binary_with(false, false, &cfg_owned, "echo 'error TS1234'; exit 1")
    })
    .await
    .unwrap();

    // Allow the fire-and-forget task time to land.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let received = captured.lock().await;
    assert_eq!(exit, 1, "child exit 1 should propagate");
    assert_eq!(
        received.len(),
        1,
        "exactly one trigger expected (failure_sensation is disabled, success path won't fire)"
    );
    assert_eq!(
        received[0].backend_id, "mock-owo",
        "backend_id should fall back to config default"
    );
    let sensation_name = match &received[0].sensation {
        Some(smited_watch::proto::smited::v1::trigger_request::Sensation::SensationName(n)) => {
            n.clone()
        }
        other => panic!("expected SensationName, got {other:?}"),
    };
    assert_eq!(sensation_name, "compile_error_mild");
    assert!(
        received[0].client_trace_id.starts_with("watch-ts-"),
        "trace id format: {}",
        received[0].client_trace_id
    );

    let _ = shutdown.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dry_run_fires_no_triggers() {
    let (addr, captured, shutdown) = spawn_fake_daemon().await;
    let (_dir, cfg) = write_match_config(&addr.to_string());

    let cfg_owned = cfg.clone();
    let _exit = tokio::task::spawn_blocking(move || {
        run_binary_with(false, true, &cfg_owned, "echo 'error TS1234'; exit 1")
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(500)).await;

    let received = captured.lock().await;
    assert!(
        received.is_empty(),
        "dry-run must not fire any triggers; got {} reqs",
        received.len()
    );

    let _ = shutdown.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn smited_watch_disable_fires_no_triggers() {
    let (addr, captured, shutdown) = spawn_fake_daemon().await;
    let (_dir, cfg) = write_match_config(&addr.to_string());

    let cfg_owned = cfg.clone();
    let _exit = tokio::task::spawn_blocking(move || {
        run_binary_with(true, false, &cfg_owned, "echo 'error TS1234'; exit 0")
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(500)).await;

    let received = captured.lock().await;
    assert!(
        received.is_empty(),
        "SMITED_WATCH_DISABLE=1 must skip the trigger pipeline"
    );

    let _ = shutdown.send(());
}

/// Wire-level proof of fix #1: child killed by SIGINT (the PTY-raw-mode
/// scenario where the wrapper never observes the signal itself) must NOT
/// fire `failure_sensation`. We send SIGINT to the *child PID* so the
/// wrapper's signal handler is bypassed, mirroring the PTY case where
/// Ctrl-C bytes go through stdin into the PTY rather than to the wrapper.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_killed_by_sigint_does_not_fire_failure_sensation() {
    let (addr, captured, shutdown) = spawn_fake_daemon().await;
    let (dir, cfg) = write_failure_config(&addr.to_string());
    let pid_file = dir.path().join("bash.pid");
    let pid_file_str = pid_file.display().to_string();

    let cfg_owned = cfg.clone();
    let pid_file_clone = pid_file.clone();
    let runner = tokio::task::spawn_blocking(move || {
        // `exec sleep` so bash's PID *becomes* sleep — no descendants
        // hanging on to the wrapper's stdio pipes after the signal
        // (which would block the wrapper's tee tasks until sleep
        // finishes naturally and turn this from a 1-second test into a
        // 60-second one).
        let script = format!("echo $$ > {pid_file_str}; exec sleep 60");
        let mut c = Command::cargo_bin("smited-watch").expect("binary built");
        c.arg("--no-banner")
            .arg("--quiet")
            .arg("--config")
            .arg(&cfg_owned)
            .arg("--")
            .arg("bash")
            .arg("-c")
            .arg(&script)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        c.status().expect("run smited-watch").code().unwrap_or(-1)
    });

    // Give bash time to write its PID, then SIGINT the bash child directly.
    let bash_pid: i32 = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(s) = std::fs::read_to_string(&pid_file_clone) {
                if let Ok(n) = s.trim().parse::<i32>() {
                    break n;
                }
            }
            if tokio::time::Instant::now() > deadline {
                let _ = shutdown.send(());
                panic!("bash never wrote its PID");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    unsafe {
        libc::kill(bash_pid as libc::pid_t, libc::SIGINT);
    }
    let _ = runner.await.unwrap();

    // Allow drain.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let received = captured.lock().await;
    assert!(
        received.is_empty(),
        "child killed by user-initiated SIGINT must NOT fire failure_sensation \
         (got {} requests: {:?})",
        received.len(),
        received
            .iter()
            .filter_map(|r| match &r.sensation {
                Some(
                    smited_watch::proto::smited::v1::trigger_request::Sensation::SensationName(n),
                ) => Some(n.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    );

    let _ = shutdown.send(());
}

/// Wire-level proof of the second half of fix-#1: when a backgrounded
/// descendant emits a pattern-matching line *after* the immediate child
/// has already exited, no trigger should fire — the wrapper has aborted
/// its scanner and tee tasks within the grace window. Pre-fix the tee
/// tasks (and scanner consumer) were detached on timeout rather than
/// aborted, so they kept processing and the late pattern match would
/// still reach the daemon.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_triggers_fire_after_grace_window_for_backgrounded_descendants() {
    let (addr, captured, shutdown) = spawn_fake_daemon().await;
    // Pattern-only config: the on-exit branches are disabled so the
    // assertion "no triggers fired" only catches the late-pattern case
    // we're actually testing.
    let (dir, cfg) = write_pattern_only_config(&addr.to_string());

    // Write the backgrounded subshell's PID to a file so cleanup can
    // target that specific PID instead of using `pkill -f` (which
    // would also match unrelated `sleep 0.5` processes from concurrent
    // tests or from a developer's other terminals).
    let pid_file = dir.path().join("subshell.pid");
    let script = format!(
        r#"(sleep 0.5; echo "error TS9999") & echo $! > {}; exit 0"#,
        pid_file.display()
    );

    let cfg_owned = cfg.clone();
    let exit = tokio::task::spawn_blocking(move || {
        // Bash exits immediately (code 0). The descendant, with bash's
        // stdout/stderr inherited, sleeps half a second and *then* emits
        // a TS-error line. The wrapper has long since aborted its
        // scanner and tee tasks — the late line should be swallowed by
        // the kernel pipe buffer with no live reader to scan it.
        run_binary_with(false, false, &cfg_owned, &script)
    })
    .await
    .unwrap();

    // Wait long enough that *if* the bug were back, the late echo would
    // have happened (500ms) and the trigger would have arrived (a few
    // hundred ms more). 1.5s is generous.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let received = captured.lock().await;
    assert_eq!(exit, 0, "bash exited 0; wrapper should propagate that");
    assert!(
        received.is_empty(),
        "no triggers should fire after the wrapper exited; got {} request(s): {:?}",
        received.len(),
        received
            .iter()
            .filter_map(|r| match &r.sensation {
                Some(
                    smited_watch::proto::smited::v1::trigger_request::Sensation::SensationName(n),
                ) => Some(n.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    );

    // Cleanup: kill the specific subshell PID we spawned (which in turn
    // takes its `sleep 0.5` child with it via the same pgrp). Best-effort
    // — by the time the assertion ran the descendant has likely already
    // finished its 500ms sleep and exited on its own.
    if let Ok(s) = std::fs::read_to_string(&pid_file) {
        if let Ok(pid) = s.trim().parse::<i32>() {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }

    let _ = shutdown.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn on_exit_success_sensation_fires_when_duration_threshold_met() {
    // Use a config where the failure_dedupe matters: success path with
    // success_min_duration_ms = 1 ensures any non-instant success fires.
    let (addr, captured, shutdown) = spawn_fake_daemon().await;
    let (_dir, cfg) = write_match_config(&addr.to_string());

    let cfg_owned = cfg.clone();
    let exit = tokio::task::spawn_blocking(move || {
        // Sleep 50ms so duration > success_min_duration_ms (1ms).
        run_binary_with(false, false, &cfg_owned, "sleep 0.05; echo nothing-special")
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(500)).await;

    let received = captured.lock().await;
    assert_eq!(exit, 0);
    assert_eq!(
        received.len(),
        1,
        "exactly one trigger expected: success on-exit"
    );
    let sensation_name = match &received[0].sensation {
        Some(smited_watch::proto::smited::v1::trigger_request::Sensation::SensationName(n)) => {
            n.clone()
        }
        other => panic!("expected SensationName, got {other:?}"),
    };
    assert_eq!(sensation_name, "deploy_success");
    assert!(received[0].client_trace_id.starts_with("watch-on-exit-"));

    let _ = shutdown.send(());
}
