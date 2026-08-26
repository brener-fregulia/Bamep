//! `bamepd` binary entrypoint (Issue #37): the minimal Server daemon
//! composition root proving real Worker supervision. Owns only:
//!
//! - the Worker UDS control-plane listener;
//! - the Worker process supervisor;
//! - Worker executable location/config and TLS identity path forwarding;
//! - shutdown coordination.
//!
//! Deliberately does NOT wire PostgreSQL startup, the Administrative API,
//! the Agent WSS listener, Web, scheduler workflows, transfer
//! authorization, Worker HTTPS, or storage — those existing/future
//! responsibilities remain tested through their own Application/Adapter
//! boundaries until their own composition-root work requires integration.
//! `bamepd` is currently only a *partial* composition root.

use std::sync::Arc;

use bamep_server::adapters::worker_control_plane::WorkerControlPlane;
use bamep_server::runtime::bamepd_config::BamepdConfig;
use bamep_server::runtime::worker_authority::WorkerAuthorityRegistry;
use bamep_server::runtime::worker_supervisor::{
    SupervisorConfig, SupervisorEvent, WorkerSupervisor,
};
use tokio::sync::{mpsc, watch};

fn main() {
    let config = BamepdConfig::from_env().unwrap_or_else(|err| {
        eprintln!("bamepd: configuration error: {err}");
        std::process::exit(1);
    });

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|err| {
            eprintln!("bamepd: failed to start async runtime: {err}");
            std::process::exit(1);
        });

    runtime.block_on(run(config));
}

/// Startup ordering (Issue #37 "Startup ordering"): the UDS listener is
/// bound *before* the Worker supervisor starts, so Worker never races
/// against a listener that was never successfully created.
async fn run(config: BamepdConfig) {
    let control_plane = WorkerControlPlane::bind(&config.uds_path).unwrap_or_else(|err| {
        eprintln!("bamepd: failed to bind Worker UDS listener: {err}");
        std::process::exit(1);
    });

    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();

    let supervisor = WorkerSupervisor::new(SupervisorConfig {
        worker_executable: config.worker_executable.clone(),
        env: config.worker_env(),
        restart_delay: config.worker_restart_delay,
    });
    let supervisor_task = {
        let shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move { supervisor.run(shutdown_rx, events_tx).await })
    };

    let event_log_task = tokio::spawn(async move {
        while let Some(event) = events_rx.recv().await {
            match event {
                SupervisorEvent::WorkerStarted { pid } => {
                    eprintln!("bamepd: Worker started, pid={pid}");
                }
                SupervisorEvent::WorkerExited { pid, status } => {
                    eprintln!("bamepd: Worker exited, pid={pid}, status={status:?}");
                }
                SupervisorEvent::SpawnFailed(err) => {
                    eprintln!("bamepd: failed to spawn Worker: {err}");
                }
            }
        }
    });

    // `control_plane.run` accepts Worker's connection, performs the
    // handshake, and registers the resulting connection generation with
    // `registry` — the narrow readiness/control-connection seam #38/#39
    // consume through `registry.current()`.
    let control_plane_task = tokio::spawn(control_plane.run(Arc::clone(&registry), shutdown_rx));

    let _ = tokio::signal::ctrl_c().await;
    eprintln!("bamepd: shutdown requested");

    let _ = shutdown_tx.send(true);
    let _ = supervisor_task.await;
    let _ = control_plane_task.await;
    event_log_task.abort();
}
