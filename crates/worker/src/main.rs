//! `bamep-worker` binary entrypoint (Issue #37; Issue #39 Phases D1/D2/E1).
//! Loads configuration, the Server TLS identity, and the local chunk storage
//! root, then runs the concurrent Worker Protocol v1 control client forever.
//! `bamepd` owns this process's lifecycle — start, liveness, restart, and
//! controlled termination (`bamep_server::runtime::worker_supervisor`) — so
//! this binary itself implements no self-shutdown signal handling: ordinary
//! process termination (SIGTERM/SIGKILL from the supervisor) is sufficient.
//! Phase E1 stands up the control client and retains its handle; Phase E2
//! will compose that handle with D1 storage and D2 reconstruction behind the
//! HTTPS data-plane routes.

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

    // Proves the loaded cert/key pair is actually rustls-usable before
    // Worker claims readiness (ADR-0018 "TLS identity"). #37 binds no
    // production listener with it — see `bamep_worker::tls` docs.
    if let Err(err) = bamep_worker::tls::build_server_config(&identity) {
        eprintln!("bamep-worker: Server TLS identity is not usable: {err}");
        std::process::exit(1);
    }

    // Validate/prepare the local chunk storage root and clear recognized
    // leftover staging files before Worker continues (Issue #39 Phase D1).
    // Fails closed: no usable storage root -> no Worker (Phase E will not
    // add an HTTPS listener that could accept chunk uploads without one).
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
        "bamep-worker: starting; worker_instance_id={worker_instance_id}; uds_path={}; control_request_timeout={}ms",
        config.uds_path.display(),
        config.control_request_timeout.as_millis(),
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|err| {
            eprintln!("bamep-worker: failed to start async runtime: {err}");
            std::process::exit(1);
        });

    runtime.block_on(async move {
        // Held for the process lifetime; Phase E2's HTTPS request handlers are
        // the first consumers of the storage mechanism and the control
        // handle. E1 wires nothing to them beyond standing the control client
        // up and keeping the reconnect/dispatch driver running.
        let _chunk_store = chunk_store;

        let (control_handle, control_driver) = bamep_worker::ipc::worker_control(
            config.uds_path,
            config.reconnect_delay,
            config.control_request_timeout,
            worker_instance_id,
        );
        let _control_handle = control_handle;

        // `bamepd` supervision (SIGTERM/SIGKILL) terminates this process; the
        // driver runs until then. `std::future::pending()` is the "no
        // in-process shutdown" signal — the driver still fails closed and
        // reconnects on every UDS loss.
        control_driver.run(std::future::pending::<()>()).await;
    });
}
