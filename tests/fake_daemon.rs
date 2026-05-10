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
