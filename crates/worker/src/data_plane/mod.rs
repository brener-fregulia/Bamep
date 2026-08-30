//! Worker-owned HTTPS `/api/data/v1/` data-plane listener (Issue #39 Phase
//! E2A; `docs/specifications/m0-data-plane-and-storage-contracts.md` "HTTPS
//! data-plane v1 contract"; ADR-0018).
//!
//! This module owns the HTTP serving runtime and the exact route/common-
//! header parsing the contract fixes, and it fully implements exactly one
//! operation — the `GET .../chunks` resume discovery — by handing a parsed
//! request to the Phase E1 control client
//! ([`WorkerControlHandle::discover_resume`](crate::ipc::WorkerControlHandle::discover_resume))
//! and rendering the authoritative aggregate.
//!
//! # Framework (HOW NOW, not a new architectural rule)
//!
//! Axum 0.8 + `axum-server` (rustls, `tls-rustls-no-provider`). ADR-0017
//! already accepts Axum 0.8 for `bamepd`'s Administrative HTTP surface; the
//! Worker reuses that stack rather than maintaining a second unrelated Rust
//! HTTP framework, and stays independently deployable (no `bamep-server`
//! dependency). `axum-server` receives the Worker's already-loaded
//! `ring`-built `rustls::ServerConfig` verbatim (the same Server TLS
//! identity the Agent already trusts, ADR-0018) — no bundled crypto backend,
//! no second certificate, no client certificates, no second trust anchor.
//!
//! # Authority boundary
//!
//! This module performs **structural** HTTP parsing only. It never verifies a
//! capability, proof signature, freshness, replay, Endpoint/Attempt/credential
//! state, or held-chunk truth: a structurally representable request always
//! goes to `bamepd` through E1, and a structurally *un*representable one is
//! rejected locally with the contract's fixed `400 MALFORMED_REQUEST`. Every
//! authorization failure, and every internal/control-transport failure, is
//! the contract's single fixed generic `401 AUTHORIZATION_DENIED`
//! (`m1-worker-data-plane-control-contract.md` "Failure semantics": a
//! distinguishable "try again later" is deliberately not defined). The
//! Worker emits no `5xx` on `/api/data/v1/` and issues no redirects.
//!
//! Not in Phase E2A: `PUT .../chunks/{n}` chunk transport, D1 staging from
//! HTTP, `ChunkAcceptanceRequest` orchestration, `POST .../seal`, D2
//! reconstruction from HTTP, `ArtifactVerificationReport` — all Phase E2B.

mod http;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::ipc::WorkerControlHandle;

/// How long in-flight requests may drain after a shutdown signal before the
/// listener is force-closed.
const GRACEFUL_SHUTDOWN: Duration = Duration::from_secs(3);

#[derive(Debug, thiserror::Error)]
pub enum DataPlaneError {
    /// The HTTPS listener could not bind / serve (address in use, socket
    /// error). The composition root treats this as fatal — the Worker must
    /// not run with only UDS control.
    #[error("data-plane HTTPS listener failed: {0}")]
    Serve(#[source] std::io::Error),
}

/// `axum-server`'s connection handle for a TCP-bound server.
pub type ServerHandle = axum_server::Handle<SocketAddr>;

/// The Worker HTTPS data-plane runtime. Built once at startup with the bind
/// address, the already-loaded Server TLS config, and the E1 control handle;
/// [`run`](Self::run) drives it until `shutdown` resolves.
pub struct DataPlane {
    bind_addr: SocketAddr,
    handle: ServerHandle,
    rustls_config: axum_server::tls_rustls::RustlsConfig,
    router: axum::Router,
}

impl DataPlane {
    /// `tls` is the Worker's existing `rustls::ServerConfig`
    /// (`crate::tls::build_server_config`) — used verbatim except that
    /// `http/1.1` is set as the sole ALPN protocol (the M1 data plane is
    /// HTTP/1.1). `control` is cloned into the request handlers.
    pub fn new(
        bind_addr: SocketAddr,
        tls: Arc<rustls::ServerConfig>,
        control: WorkerControlHandle,
    ) -> Self {
        let mut server_config = (*tls).clone();
        server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let rustls_config =
            axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(server_config));

        DataPlane {
            bind_addr,
            handle: ServerHandle::new(),
            rustls_config,
            router: http::router(control),
        }
    }

    /// A clone of the server handle — tests use `handle.listening().await` to
    /// discover the bound port when binding to `:0`.
    pub fn handle(&self) -> ServerHandle {
        self.handle.clone()
    }

    /// Serves HTTPS until `shutdown` resolves (then drains in-flight requests
    /// for a bounded grace period) or the listener fails. Returns `Ok(())` on
    /// a clean shutdown, `Err` on a bind/serve failure.
    pub async fn run(
        self,
        shutdown: impl std::future::Future<Output = ()>,
    ) -> Result<(), DataPlaneError> {
        let shutdown_handle = self.handle.clone();
        let server = axum_server::tls_rustls::bind_rustls(self.bind_addr, self.rustls_config)
            .handle(self.handle)
            .serve(self.router.into_make_service());

        tokio::pin!(server);
        tokio::pin!(shutdown);

        tokio::select! {
            result = &mut server => return result.map_err(DataPlaneError::Serve),
            _ = &mut shutdown => {}
        }

        shutdown_handle.graceful_shutdown(Some(GRACEFUL_SHUTDOWN));
        server.await.map_err(DataPlaneError::Serve)
    }
}
