//! `bamep-worker` binary entrypoint (Issue #37; Issue #39 Phases D1/D2/E1/
//! E2A). Startup order: load config -> load the Server TLS identity ->
//! initialize D1 storage -> construct the E1 control client -> bind and serve
//! the HTTPS `/api/data/v1/` data-plane listener, running the control driver
//! and the HTTPS server concurrently. `bamepd` owns this process's lifecycle
//! — start, liveness, restart, controlled termination
//! (`bamep_server::runtime::worker_supervisor`) — so this binary implements
//! no self-shutdown signal handling; ordinary process termination
//! (SIGTERM/SIGKILL) is sufficient. If either the control driver or the HTTPS
//! server ends unexpectedly the process exits non-zero so `bamepd` respawns
//! it — the Worker never runs indefinitely with only one half alive.
//! Phase E2B composes the data plane with D1 staging and D2 reconstruction
//! for chunk `PUT` and seal `POST`.

fn main() {
    let config = bamep_worker::config::WorkerConfig::from_env().unwrap_or_else(|err| {
        eprintln!("bamep-worker: configuration error: {err}");
        std::process::exit(1);
    });

    let identity =
        bamep_worker::tls::load_server_identity(&config.tls_cert_path, &config.tls_key_path)
            .unwrap_or_else(|err| {
                eprintln!("bamep-worker: failed to load Server TLS identity: {err}");
                std::process::exit(1);
            });

    // The exact `rustls::ServerConfig` the HTTPS data-plane listener will
    // serve — the same Server TLS identity the Agent already trusts (ADR-0018
    // "TLS identity"), TLS 1.3, `ring`, no client auth.
    let tls_server_config =
        bamep_worker::tls::build_server_config(&identity).unwrap_or_else(|err| {
            eprintln!("bamep-worker: Server TLS identity is not usable: {err}");
            std::process::exit(1);
        });

    // Validate/prepare the local chunk storage root and clear recognized
    // leftover staging files before Worker continues (Issue #39 Phase D1).
    // Fails closed: no usable storage root -> no Worker. Phase E2A does not
    // yet inject this into the data-plane handler (resume discovery reads no
    // storage), but Phase E2B will, so it stays initialized here.
    let chunk_store = bamep_worker::storage::FilesystemChunkStore::initialize(&config.storage_root)
        .unwrap_or_else(|err| {
            eprintln!("bamep-worker: chunk storage initialization failed: {err}");
            std::process::exit(1);
        });
    eprintln!(
        "bamep-worker: chunk storage root ready: {}",
        config.storage_root.display()
    );

    let worker_instance_id = uuid::Uuid::new_v4();
    eprintln!(
        "bamep-worker: starting; worker_instance_id={worker_instance_id}; uds_path={}; control_request_timeout={}ms; data_plane_bind_addr={}",
        config.uds_path.display(),
        config.control_request_timeout.as_millis(),
        config.data_plane_bind_addr,
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|err| {
            eprintln!("bamep-worker: failed to start async runtime: {err}");
            std::process::exit(1);
        });

    runtime.block_on(async move {
        // Held for the process lifetime; Phase E2B's chunk-PUT/seal-POST
        // handlers are the first consumers of the storage mechanism.
        let _chunk_store = chunk_store;

        let (control_handle, control_driver) = bamep_worker::ipc::worker_control(
            config.uds_path,
            config.reconnect_delay,
            config.control_request_timeout,
            worker_instance_id,
        );
        let data_plane = bamep_worker::data_plane::DataPlane::new(
            config.data_plane_bind_addr,
            tls_server_config,
            control_handle,
        );

        // One shutdown signal wired to both halves. `bamepd` supervision
        // (SIGTERM/SIGKILL) is what normally terminates this process; if
        // *either* half ends on its own that is a fault — signal the sibling
        // to wind down, then exit non-zero so `bamepd` respawns a clean
        // process (never run indefinitely with only one half alive).
        let (shutdown_tx, _) = tokio::sync::watch::channel(false);
        let control_shutdown = {
            let mut rx = shutdown_tx.subscribe();
            async move {
                let _ = rx.wait_for(|stop| *stop).await;
            }
        };
        let data_plane_shutdown = {
            let mut rx = shutdown_tx.subscribe();
            async move {
                let _ = rx.wait_for(|stop| *stop).await;
            }
        };

        let control_task = tokio::spawn(control_driver.run(control_shutdown));
        let data_plane_task = tokio::spawn(data_plane.run(data_plane_shutdown));

        tokio::select! {
            result = control_task => {
                eprintln!("bamep-worker: control driver ended unexpectedly ({result:?})");
            }
            result = data_plane_task => {
                eprintln!("bamep-worker: HTTPS data-plane server ended unexpectedly ({result:?})");
            }
        }
        let _ = shutdown_tx.send(true);
        std::process::exit(1);
    });
}
