//! `bamepd` binary entrypoint (Issue #37, extended by Issue #38): the
//! minimal Server daemon composition root proving real Worker supervision
//! and sender-constrained transfer authorization. Owns only:
//!
//! - the Worker UDS control-plane listener;
//! - the Worker process supervisor;
//! - Worker executable location/config and TLS identity path forwarding;
//! - the minimum PostgreSQL connection needed to answer Worker
//!   `AuthorizationQuery` traffic with current durable state (Issue #38 —
//!   the Worker control plane cannot decide authorization without it);
//! - the process-lifetime capability store and replay cache;
//! - shutdown coordination.
//!
//! Still deliberately does NOT wire the Administrative API, the Agent WSS
//! listener (`TransferAuthorizationService::issue` is exercised directly by
//! its own Application/Adapter-level tests, not through a real WSS listener
//! here), Web, scheduler workflows, Worker HTTPS, or storage — those
//! existing/future responsibilities remain tested through their own
//! Application/Adapter boundaries until their own composition-root work
//! requires integration. `bamepd` is currently only a *partial* composition
//! root.

use std::sync::Arc;

use bamep_server::adapters::postgres::{
    PostgresTransferAuthorizationRepository, PostgresTransferRepository,
};
use bamep_server::adapters::worker_control_plane::WorkerControlPlane;
use bamep_server::adapters::worker_runtime_ownership::{RuntimeOwnershipLock, TrustedRuntimeDir};
use bamep_server::application::{ChunkAcceptanceService, TransferAuthorizationService};
use bamep_server::runtime::bamepd_config::BamepdConfig;
use bamep_server::runtime::capability_store::CapabilityStore;
use bamep_server::runtime::replay_cache::ReplayCache;
use bamep_server::runtime::worker_authority::WorkerAuthorityRegistry;
use bamep_server::runtime::worker_supervisor::{
    SupervisorConfig, SupervisorEvent, WorkerSupervisor, SUPERVISOR_EVENT_CHANNEL_CAPACITY,
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

/// Startup ordering (Issue #37 "Startup ordering"; correction audit "Solve
/// the ownership model once"):
///
/// 1. validate/create the trusted runtime directory;
/// 2. acquire the exclusive, non-blocking, lifetime-held ownership lock —
///    a competing live `bamepd` targeting the same runtime directory fails
///    here, before ever touching the Worker UDS socket pathname;
/// 3. only then bind the UDS listener;
/// 4. only then start the Worker supervisor, so Worker never races against
///    a listener that was never successfully created.
async fn run(config: BamepdConfig) {
    let runtime_dir_path = config.uds_path.parent().unwrap_or_else(|| {
        eprintln!("bamepd: configured Worker UDS path has no parent directory");
        std::process::exit(1);
    });
    let runtime_dir =
        TrustedRuntimeDir::validate_or_create(runtime_dir_path).unwrap_or_else(|err| {
            eprintln!("bamepd: untrusted Worker control-boundary runtime directory: {err}");
            std::process::exit(1);
        });
    let ownership_lock = RuntimeOwnershipLock::acquire(&runtime_dir).unwrap_or_else(|err| {
        eprintln!("bamepd: failed to acquire the Worker control-boundary ownership lock: {err}");
        std::process::exit(1);
    });

    let control_plane = WorkerControlPlane::bind(&config.uds_path).unwrap_or_else(|err| {
        eprintln!("bamepd: failed to bind Worker UDS listener: {err}");
        std::process::exit(1);
    });

    // Issue #38: the Worker control plane cannot answer `AuthorizationQuery`
    // without current durable state, so `bamepd` must connect to PostgreSQL
    // before it can honestly claim the control plane is ready to serve.
    let pool = bamep_server::adapters::postgres::connect(&config.database_url)
        .await
        .unwrap_or_else(|err| {
            eprintln!("bamepd: failed to connect to PostgreSQL: {err}");
            std::process::exit(1);
        });
    let authorization_repo = Arc::new(PostgresTransferAuthorizationRepository::new(pool.clone()));
    let capability_store = Arc::new(CapabilityStore::new());
    let replay_cache = Arc::new(ReplayCache::new());
    let transfer_authorization = Arc::new(TransferAuthorizationService::new(
        authorization_repo,
        Arc::clone(&capability_store),
        replay_cache,
        config.data_plane_base_url.clone(),
    ));
    // Issue #39 Phase C1: `ChunkAcceptanceRequest` durable coordination.
    // `resume_discovery` reuses `transfer_authorization` directly.
    let chunk_acceptance = Arc::new(ChunkAcceptanceService::new(Arc::new(
        PostgresTransferRepository::new(pool),
    )));

    let registry = Arc::new(WorkerAuthorityRegistry::new());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (events_tx, mut events_rx) = mpsc::channel(SUPERVISOR_EVENT_CHANNEL_CAPACITY);

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
    // handshake, registers the resulting connection generation with
    // `registry`, and answers `AuthorizationQuery` traffic through
    // `transfer_authorization` — the narrow readiness/control-connection
    // seam #39 consumes through `registry.current()`.
    let mut control_plane_task = tokio::spawn(control_plane.run(
        Arc::clone(&registry),
        Arc::clone(&transfer_authorization),
        Arc::clone(&chunk_acceptance),
        shutdown_rx,
    ));

    // Wait for either a controlled shutdown request or the Worker
    // control-plane task terminating on its own — a persistent `accept()`
    // failure, or an unexpected panic — since `bamepd` must never remain
    // apparently healthy with no authoritative Worker UDS boundary
    // (correction audit "bamepd response to control-plane failure").
    // `&mut control_plane_task` is polled here without consuming it, so it
    // can still be awaited again below only if this branch never fired.
    let control_plane_failure = tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            eprintln!("bamepd: shutdown requested");
            None
        }
        joined = &mut control_plane_task => {
            match &joined {
                Ok(Ok(())) => {
                    eprintln!("bamepd: Worker control plane stopped unexpectedly with no shutdown requested");
                }
                Ok(Err(err)) => {
                    eprintln!("bamepd: Worker control plane failed: {err}");
                }
                Err(join_err) => {
                    eprintln!("bamepd: Worker control plane task panicked: {join_err}");
                }
            }
            Some(joined)
        }
    };

    // Fail-closed shutdown: stop/kill/reap the supervised Worker regardless
    // of which branch above fired — a normal Worker child crash is already
    // the supervisor's own concern and must not itself reach this path.
    let _ = shutdown_tx.send(true);
    let _ = supervisor_task.await;
    if control_plane_failure.is_none() {
        let _ = control_plane_task.await;
    }
    event_log_task.abort();

    // `WorkerControlPlane::run` already removed its own socket file before
    // returning (awaited above in both branches), so the ownership lock is
    // the last thing released — correction audit "Solve the ownership model
    // once": stop handlers, stop Worker, clean up the Worker socket, release
    // the lifetime ownership lock LAST.
    ownership_lock.release();

    if control_plane_failure.is_some() {
        eprintln!("bamepd: exiting after fatal Worker control-plane failure");
        std::process::exit(1);
    }
}
