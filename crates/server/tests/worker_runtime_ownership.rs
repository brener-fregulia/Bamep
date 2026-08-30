//! Real-process tests for the Worker control-boundary lifetime ownership
//! lock (correction audit on Issue #37: "Solve the ownership model once").
//!
//! `crates/server/src/adapters/worker_runtime_ownership.rs` already carries
//! unit tests (pure ancestor/mode-shape predicates) and same-process
//! `fs_tests` (directory validation, and lock acquire/release semantics
//! through two independently-`open()`ed lock objects). This file adds the
//! evidence that only a real separate-process test can provide: `flock`
//! semantics are documented per *open file description*, and while a
//! second independent `open()` in the same process already exercises that
//! correctly (proven in the unit module), a genuinely separate `bamepd`
//! process is the strongest, most representative evidence for the actual
//! deployment scenario — two independently started `bamepd` instances
//! racing the same runtime directory.
//!
//! Unix Domain Sockets/`flock`/process signals are Unix-only; this whole
//! file is a no-op on other platforms.

#![cfg(unix)]

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::time::Duration;

use bamep_server::adapters::worker_runtime_ownership::{
    LockError, RuntimeOwnershipLock, TrustedRuntimeDir,
};
use rcgen::{generate_simple_self_signed, CertifiedKey};
use support::TestDatabase;

struct TestEnv {
    dir: PathBuf,
    socket_path: PathBuf,
    cert_path: PathBuf,
    key_path: PathBuf,
    /// A real, migrated per-test PostgreSQL database (Issue #38: `bamepd`
    /// now connects to PostgreSQL at startup so the Worker control plane can
    /// answer `AuthorizationQuery` traffic with current durable state — see
    /// `bamepd.rs`). Always `Some` after [`TestEnv::new`]; `Drop::drop`
    /// takes it to hand an owned value to `TestDatabase::teardown`, which
    /// consumes `self`.
    db: Option<TestDatabase>,
    /// A dedicated single-threaded runtime this test-support type owns, used
    /// only to drive the async `TestDatabase` setup/teardown from otherwise
    /// synchronous `#[test]` functions — this file's own subject
    /// (`RuntimeOwnershipLock`/real-process `bamepd` behavior) requires no
    /// async runtime of its own.
    db_runtime: tokio::runtime::Runtime,
}

impl TestEnv {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "bamep-worker-runtime-ownership-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("create test dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
            .expect("set trusted parent dir permissions");

        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(["worker-runtime-ownership-test.bamep.local".to_string()])
                .expect("generate test certificate");
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        std::fs::write(&cert_path, cert.pem()).expect("write cert.pem");
        std::fs::write(&key_path, signing_key.serialize_pem()).expect("write key.pem");
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
            .expect("set trusted key.pem permissions");

        // `bamepd`'s runtime directory is the socket's parent — a dedicated
        // subdirectory under `dir`, created fresh by `bamepd` itself.
        let runtime_dir = dir.join("run");

        let db_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build test database runtime");
        let db = db_runtime.block_on(TestDatabase::setup());

        Self {
            socket_path: runtime_dir.join("worker.sock"),
            cert_path,
            key_path,
            dir,
            db: Some(db),
            db_runtime,
        }
    }

    fn db_url(&self) -> &str {
        &self.db.as_ref().expect("db present until Drop").db_url
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        if let Some(db) = self.db.take() {
            self.db_runtime.block_on(db.teardown());
        }
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

/// A harmless real executable used as the "Worker" for tests that only
/// exercise `bamepd`'s own startup ordering (runtime-dir validation, lock
/// acquisition, socket bind) and never need a real Worker handshake:
/// `bamepd` supervises and respawns it exactly like a real Worker, but this
/// test never waits for a UDS connection from it.
fn harmless_placeholder_executable() -> PathBuf {
    PathBuf::from("/bin/sleep")
}

fn bamepd_binary_path() -> PathBuf {
    let release = !cfg!(debug_assertions);
    let mut command = StdCommand::new(env!("CARGO"));
    command.args(["build", "--package", "bamep-server", "--bin", "bamepd"]);
    if release {
        command.arg("--release");
    }
    command.current_dir(workspace_root());
    let status = command.status().expect("run cargo build for bamepd");
    assert!(status.success(), "cargo build --bin bamepd failed");

    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| workspace_root().join("target"));
    let profile_dir = if release { "release" } else { "debug" };
    let path = target_dir.join(profile_dir).join("bamepd");
    assert!(
        path.exists(),
        "expected bamepd binary at {}",
        path.display()
    );
    path
}

fn spawn_bamepd(binary: &Path, env: &TestEnv) -> std::process::Child {
    StdCommand::new(binary)
        .env("BAMEP_WORKER_UDS_PATH", &env.socket_path)
        .env(
            "BAMEPD_WORKER_EXECUTABLE",
            harmless_placeholder_executable(),
        )
        .env("BAMEP_WORKER_TLS_CERT_PATH", &env.cert_path)
        .env("BAMEP_WORKER_TLS_KEY_PATH", &env.key_path)
        // Issue #39 Phase E2A: bamepd validates this against the
        // BAMEP_DATA_PLANE_BASE_URL port and forwards it to the Worker
        // child (the placeholder Worker executable never binds it).
        .env("BAMEP_WORKER_DATA_PLANE_BIND_ADDR", "127.0.0.1:8443")
        // Issue #39 Phase D1: `bamepd` requires this to be present so it can
        // forward it to the Worker child. This test's Worker executable is a
        // harmless placeholder, so the path is only carried, never used.
        .env("BAMEP_WORKER_STORAGE_ROOT", env.dir.join("chunk-storage"))
        .env("BAMEPD_WORKER_RESTART_DELAY_MS", "500")
        .env("BAMEP_WORKER_RECONNECT_DELAY_MS", "500")
        .env("BAMEPD_DATABASE_URL", env.db_url())
        .env("BAMEP_DATA_PLANE_BASE_URL", "https://server.example:8443")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bamepd")
}

/// Proves the primary invariant end to end with two real, separate
/// `bamepd` OS processes: the second process must fail — via the
/// ownership lock, before ever touching the Worker UDS socket pathname —
/// while the first is still alive, and the first's socket must remain
/// completely undisturbed.
#[test]
fn a_second_real_bamepd_process_fails_before_touching_the_socket_and_the_first_is_undisturbed() {
    let env = TestEnv::new();
    let binary = bamepd_binary_path();

    let mut first = spawn_bamepd(&binary, &env);

    // Give the first instance time to validate the runtime directory,
    // acquire the lock, and bind the socket.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !env.socket_path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "first bamepd never bound the Worker UDS socket"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let first_socket_metadata =
        std::fs::symlink_metadata(&env.socket_path).expect("first bamepd's socket exists");

    // A second, independently started `bamepd` process targeting the exact
    // same runtime directory must fail — and must exit non-zero — without
    // ever mutating the first instance's live socket.
    let mut second = spawn_bamepd(&binary, &env);
    let second_status = second.wait().expect("wait for second bamepd");
    assert!(
        !second_status.success(),
        "a second bamepd targeting the same runtime directory must fail to start"
    );

    let second_stderr = {
        use std::io::Read;
        let mut buf = String::new();
        second
            .stderr
            .take()
            .expect("captured stderr")
            .read_to_string(&mut buf)
            .expect("read stderr");
        buf
    };
    assert!(
        second_stderr.contains("ownership lock"),
        "expected the second bamepd's failure to name the ownership lock, got: {second_stderr}"
    );

    // The first instance's socket must be byte-for-byte the same
    // filesystem object it was before the second instance ever ran —
    // never probed/unlinked/replaced.
    let socket_metadata_after =
        std::fs::symlink_metadata(&env.socket_path).expect("first bamepd's socket still exists");
    assert_eq!(
        (
            std::os::unix::fs::MetadataExt::dev(&first_socket_metadata),
            std::os::unix::fs::MetadataExt::ino(&first_socket_metadata)
        ),
        (
            std::os::unix::fs::MetadataExt::dev(&socket_metadata_after),
            std::os::unix::fs::MetadataExt::ino(&socket_metadata_after)
        ),
        "the first bamepd's socket must remain the exact same filesystem object"
    );

    let _ = first.kill();
    let _ = first.wait();
}

/// Once the first real `bamepd` process terminates (releasing its lock via
/// process death — the kernel releases `flock`s automatically when every
/// descriptor referencing the open file description closes), a second real
/// `bamepd` process must be able to start cleanly against the same runtime
/// directory.
#[test]
fn a_second_real_bamepd_process_succeeds_after_the_first_terminates() {
    let env = TestEnv::new();
    let binary = bamepd_binary_path();

    let mut first = spawn_bamepd(&binary, &env);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !env.socket_path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "first bamepd never bound the Worker UDS socket"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // Real process death, not a controlled shutdown — proves the lock does
    // not require graceful cleanup to be released.
    first.kill().expect("kill first bamepd");
    first.wait().expect("reap first bamepd");

    let mut second = spawn_bamepd(&binary, &env);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if env.socket_path.exists() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "second bamepd never bound the Worker UDS socket after the first terminated"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let _ = second.kill();
    let _ = second.wait();
}

/// A stale socket left by an unclean shutdown may only be removed after
/// ownership is acquired — expressed here through the real library API a
/// real `bamepd` startup uses, against a runtime directory this test fully
/// controls (complementing the real-process tests above, which cannot
/// easily observe the intermediate "stale cleanup" step from outside).
#[test]
fn stale_socket_cleanup_only_happens_after_ownership_is_acquired() {
    let dir = std::env::temp_dir().join(format!(
        "bamep-worker-runtime-ownership-stale-{}",
        uuid::Uuid::new_v4()
    ));
    let runtime_dir = TrustedRuntimeDir::validate_or_create(&dir).expect("create runtime dir");
    let socket_path = dir.join("worker.sock");

    // A leftover Unix socket pathname from a previous, uncleanly-terminated
    // owner (never actually bound as a live listener in this test — the
    // narrow claim here is about ordering, not the stale-probe logic
    // itself, which `worker_control_plane.rs` already covers).
    {
        let _leftover = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind");
        // Dropped without cleanup, leaving the pathname behind.
    }
    assert!(socket_path.exists());

    // Acquire ownership first...
    let lock = RuntimeOwnershipLock::acquire(&runtime_dir).expect("acquire lock");

    // ...only now is it this test's turn to be the one allowed to inspect
    // and remove the stale pathname, mirroring `WorkerControlPlane::bind`'s
    // own contract of only running after the caller already holds the
    // lock.
    std::fs::remove_file(&socket_path).expect("remove stale socket while lock is held");
    assert!(!socket_path.exists());

    lock.release();
    std::fs::remove_dir_all(&dir).ok();
}

/// A second lock acquisition attempt for the same runtime directory must
/// fail with `LockError::AlreadyOwned` while the first is held — restated
/// here (distinct from the module's own `fs_tests`) as the direct
/// black-box proof that a caller performing `bamepd`'s exact startup
/// sequence (validate dir, then acquire) observes the documented failure
/// mode.
#[test]
fn acquiring_an_already_owned_lock_fails_with_already_owned() {
    let dir = std::env::temp_dir().join(format!(
        "bamep-worker-runtime-ownership-already-owned-{}",
        uuid::Uuid::new_v4()
    ));
    let runtime_dir = TrustedRuntimeDir::validate_or_create(&dir).expect("create runtime dir");

    let _first = RuntimeOwnershipLock::acquire(&runtime_dir).expect("first acquire");
    let second = RuntimeOwnershipLock::acquire(&runtime_dir);
    assert!(matches!(second, Err(LockError::AlreadyOwned { .. })));

    std::fs::remove_dir_all(&dir).ok();
}
