//! Real separate-process integration test for Issue #37 ("Testing — Process
//! Supervision"): proves `WorkerSupervisor` starts a genuinely separate
//! Worker OS process (not a simulated Tokio task), that it completes a real
//! UDS handshake against the real `WorkerControlPlane`, that `bamepd`
//! survives a Worker crash and respawns a new Worker with a new PID and a
//! new `worker_instance_id`, and that controlled shutdown terminates/reaps
//! the child and cleans up the socket.
//!
//! Builds and spawns the actual `bamep-worker` binary via `cargo build`, so
//! this test requires network-free local compilation to already have the
//! crate's dependencies available (same as any other `cargo test` in this
//! workspace) and takes longer than the narrower protocol-level tests in
//! `worker_control_plane.rs`.
//!
//! Unix Domain Sockets/process-signal semantics are Unix-only; this whole
//! file is a no-op on other platforms
//! (`docs/development/testing.md` "Development environments").

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bamep_server::adapters::worker_control_plane::WorkerControlPlane;
use bamep_server::application::TransferAuthorizationService;
use bamep_server::ports::{
    AuthorizationDurableState, RepositoryError, TransferAuthorizationRepository,
};
use bamep_server::runtime::bamepd_config::{
    ENV_RECONNECT_DELAY_MS, ENV_TLS_CERT_PATH, ENV_TLS_KEY_PATH, ENV_UDS_PATH,
};
use bamep_server::runtime::capability_store::CapabilityStore;
use bamep_server::runtime::replay_cache::ReplayCache;
use bamep_server::runtime::worker_authority::{WorkerAuthorityRegistry, WorkerControlState};
use bamep_server::runtime::worker_supervisor::{
    SupervisorConfig, SupervisorEvent, WorkerSupervisor, SUPERVISOR_EVENT_CHANNEL_CAPACITY,
};
use rcgen::{generate_simple_self_signed, CertifiedKey};
use tokio::sync::{mpsc, watch};
use tokio::time::timeout;

const TEST_TIMEOUT: Duration = Duration::from_secs(20);

/// This file exercises process-supervision/handshake semantics only — the
/// spawned real `bamep-worker` process never sends `AuthorizationQuery` —
/// so a minimal always-unknown fake is sufficient to construct the real
/// `TransferAuthorizationService` `WorkerControlPlane::run` now requires.
struct AlwaysUnknownTransferAuthorizationRepository;

#[async_trait]
impl TransferAuthorizationRepository for AlwaysUnknownTransferAuthorizationRepository {
    async fn load_authorization_state(
        &self,
        _transfer_id: bamep_domain::TransferId,
    ) -> Result<Option<AuthorizationDurableState>, RepositoryError> {
        Ok(None)
    }
}

fn fake_transfer_authorization_service() -> Arc<TransferAuthorizationService> {
    Arc::new(TransferAuthorizationService::new(
        Arc::new(AlwaysUnknownTransferAuthorizationRepository),
        Arc::new(CapabilityStore::new()),
        Arc::new(ReplayCache::new()),
        "https://server.example:8443",
    ))
}

struct TestEnv {
    dir: PathBuf,
    socket_path: PathBuf,
    cert_path: PathBuf,
    key_path: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "bamep-worker-process-supervision-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("create test dir");
        // `WorkerControlPlane::bind` requires an already-existing parent
        // directory to be owner-only (correction audit "Trusted UDS parent
        // directory"); the default umask would otherwise leave this
        // group/other-readable.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("set trusted test dir permissions");

        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(
                ["worker-process-supervision-test.bamep.local".to_string()],
            )
            .expect("generate test certificate");
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        std::fs::write(&cert_path, cert.pem()).expect("write cert.pem");
        std::fs::write(&key_path, signing_key.serialize_pem()).expect("write key.pem");
        // The spawned `bamep-worker` process now enforces least-privilege
        // Unix permissions on the private key path before reading it
        // (correction audit "TLS private-key file security"); the default
        // umask would otherwise leave this group/other-readable.
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
            .expect("set trusted key.pem permissions");

        Self {
            socket_path: dir.join("worker.sock"),
            cert_path,
            key_path,
            dir,
        }
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

/// Ensures `bamep-worker` is freshly built, then returns the path to its
/// executable. Uses the same `cargo`/profile the current test binary was
/// built with, so a plain `cargo test` and a `cargo test --release` each
/// locate the matching worker binary.
fn worker_binary_path() -> PathBuf {
    let release = !cfg!(debug_assertions);

    let mut command = StdCommand::new(env!("CARGO"));
    command.args([
        "build",
        "--package",
        "bamep-worker",
        "--bin",
        "bamep-worker",
    ]);
    if release {
        command.arg("--release");
    }
    command.current_dir(workspace_root());
    let status = command.status().expect("run cargo build for bamep-worker");
    assert!(
        status.success(),
        "cargo build --package bamep-worker failed"
    );

    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| workspace_root().join("target"));
    let profile_dir = if release { "release" } else { "debug" };
    let path = target_dir.join(profile_dir).join("bamep-worker");
    assert!(
        path.exists(),
        "expected worker binary at {}",
        path.display()
    );
    path
}

async fn wait_for_availability(registry: &WorkerAuthorityRegistry, available: bool) {
    timeout(TEST_TIMEOUT, async {
        loop {
            if registry.current().is_available() == available {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("authority availability={available} not reached before timeout"));
}

#[tokio::test]
async fn supervisor_manages_a_genuinely_separate_worker_process_through_handshake_crash_respawn_and_shutdown(
) {
    let env = TestEnv::new();
    let worker_executable = worker_binary_path();

    // Startup ordering: bind the UDS listener before starting the
    // supervisor (Issue #37 "Startup ordering").
    let plane = WorkerControlPlane::bind(&env.socket_path).expect("bind UDS listener");
    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let transfer_authorization = fake_transfer_authorization_service();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let control_plane_task = tokio::spawn(plane.run(
        Arc::clone(&registry),
        Arc::clone(&transfer_authorization),
        shutdown_rx.clone(),
    ));

    let supervisor_env = vec![
        (
            ENV_UDS_PATH.to_string(),
            env.socket_path.display().to_string(),
        ),
        (
            ENV_TLS_CERT_PATH.to_string(),
            env.cert_path.display().to_string(),
        ),
        (
            ENV_TLS_KEY_PATH.to_string(),
            env.key_path.display().to_string(),
        ),
        (ENV_RECONNECT_DELAY_MS.to_string(), "100".to_string()),
    ];
    let supervisor = WorkerSupervisor::new(SupervisorConfig {
        worker_executable,
        env: supervisor_env,
        restart_delay: Duration::from_millis(100),
    });
    let (events_tx, mut events_rx) = mpsc::channel(SUPERVISOR_EVENT_CHANNEL_CAPACITY);
    let supervisor_task = tokio::spawn(async move { supervisor.run(shutdown_rx, events_tx).await });

    // 1+2: a real, genuinely separate OS process.
    let pid1 = match timeout(TEST_TIMEOUT, events_rx.recv())
        .await
        .expect("no timeout")
    {
        Some(SupervisorEvent::WorkerStarted { pid }) => pid,
        other => panic!("expected WorkerStarted, got {other:?}"),
    };
    assert_ne!(
        pid1,
        std::process::id(),
        "Worker must be a genuinely separate OS process, not a task in this test process"
    );

    // 3: the real spawned process completes a real UDS handshake.
    wait_for_availability(&registry, true).await;
    let instance_id_1 = match registry.current() {
        WorkerControlState::Active {
            worker_instance_id, ..
        } => worker_instance_id,
        WorkerControlState::NoConnection => panic!("expected an active connection"),
    };

    // 4: terminate/crash the real Worker process — a genuine external
    // SIGKILL, not a self-directed exit.
    let kill_status = StdCommand::new("kill")
        .args(["-9", &pid1.to_string()])
        .status()
        .expect("run kill");
    assert!(kill_status.success(), "failed to signal the worker process");

    // 5: bamepd (the supervisor task) remains alive and reports the exit.
    let exited_pid = match timeout(TEST_TIMEOUT, events_rx.recv())
        .await
        .expect("no timeout")
    {
        Some(SupervisorEvent::WorkerExited { pid, .. }) => pid,
        other => panic!("expected WorkerExited, got {other:?}"),
    };
    assert_eq!(exited_pid, pid1);
    assert!(
        !supervisor_task.is_finished(),
        "bamepd must remain alive after the Worker crashes"
    );

    // Authority must become unavailable promptly after the crash — no
    // stale local assumption survives the disconnect.
    wait_for_availability(&registry, false).await;

    // 6+7+8: a new Worker PID is launched and completes a fresh handshake
    // with a new worker_instance_id.
    let pid2 = match timeout(TEST_TIMEOUT, events_rx.recv())
        .await
        .expect("no timeout")
    {
        Some(SupervisorEvent::WorkerStarted { pid }) => pid,
        other => panic!("expected WorkerStarted (respawn), got {other:?}"),
    };
    assert_ne!(pid2, pid1, "respawn must launch a new PID");

    wait_for_availability(&registry, true).await;
    let instance_id_2 = match registry.current() {
        WorkerControlState::Active {
            worker_instance_id, ..
        } => worker_instance_id,
        WorkerControlState::NoConnection => panic!("expected an active connection"),
    };
    assert_ne!(
        instance_id_2, instance_id_1,
        "a new Worker process must report a new worker_instance_id"
    );

    // 9: controlled shutdown terminates/reaps the child, ends the control
    // plane cleanly, and cleans up the socket file.
    shutdown_tx.send(true).expect("send shutdown");
    timeout(TEST_TIMEOUT, supervisor_task)
        .await
        .expect("no timeout")
        .expect("supervisor task");
    let control_plane_result = timeout(TEST_TIMEOUT, control_plane_task)
        .await
        .expect("no timeout")
        .expect("control plane task");
    assert!(
        control_plane_result.is_ok(),
        "controlled shutdown must return Ok(()): {control_plane_result:?}"
    );
    assert!(
        !env.socket_path.exists(),
        "socket file must be cleaned up on controlled shutdown"
    );
}
