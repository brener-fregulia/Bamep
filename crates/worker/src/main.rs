//! `bamep-worker` binary entrypoint (Issue #37). Loads configuration and the
//! Server TLS identity, then runs the reconnecting UDS control-plane client
//! forever. `bamepd` owns this process's lifecycle — start, liveness,
//! restart, and controlled termination
//! (`bamep_server::runtime::worker_supervisor`) — so this binary itself
//! implements no self-shutdown signal handling: ordinary process
//! termination (SIGTERM/SIGKILL from the supervisor) is sufficient, since
//! Worker holds no socket or other resource of its own that requires
//! cleanup on exit (it is the UDS *client*, not the listener).

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
        "bamep-worker: starting; worker_instance_id={worker_instance_id}; uds_path={}",
        config.uds_path.display()
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|err| {
            eprintln!("bamep-worker: failed to start async runtime: {err}");
            std::process::exit(1);
        });

    runtime.block_on(async move {
        // Held for the process lifetime; Phase E's HTTPS request handler is
        // the first consumer of the storage mechanism. D1 wires nothing to
        // it beyond startup initialization.
        let _chunk_store = chunk_store;

        let (tracker, _authority_rx) = bamep_worker::ipc::AuthorityTracker::new();
        let (publisher, _authorization_client) = bamep_worker::ipc::authorization_channel();
        let _: std::convert::Infallible = bamep_worker::ipc::run_client_loop(
            config.uds_path,
            config.reconnect_delay,
            worker_instance_id,
            tracker,
            publisher,
        )
        .await;
    });
}
