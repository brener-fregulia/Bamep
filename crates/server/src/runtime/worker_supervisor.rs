//! Runtime Service supervising the Worker OS process (Issue #37 "Worker
//! supervision"; ADR-0018 "Supervision"): `bamepd` starts a genuinely
//! separate Worker child process, observes its exit, and respawns it after
//! a bounded delay — without requiring the control-plane process itself to
//! restart. In-process, never PostgreSQL-durable state, like
//! `worker_authority`/`presence`/`outbound_sessions`.
//!
//! Uses real `tokio::process::Command`/`Child` — a genuine OS process, not a
//! simulated Tokio task (Issue #37 "Worker supervision": "Do not simulate
//! Worker with an async task for acceptance").

use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::{mpsc, watch};

#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    pub worker_executable: PathBuf,
    /// Environment variables forwarded to the Worker child — UDS path and
    /// TLS identity paths only; never business state
    /// (`bamep_worker::config` "Worker process config").
    pub env: Vec<(String, String)>,
    /// Bounded delay before respawning after an exit (crash or normal),
    /// and before retrying after a spawn failure. Fixed, not exponential —
    /// Issue #37 "Restart policy" explicitly excludes an exponential-backoff
    /// framework, persistent restart counters, and HA failover.
    pub restart_delay: Duration,
}

/// The bounded capacity for the `mpsc::Sender<SupervisorEvent>` channel
/// (correction audit "Supervisor event backpressure"). These events are
/// diagnostic/logging only — never business authority the rest of `bamepd`
/// depends on — so a small bound plus drop-on-full is the right shape:
/// large enough that an ordinary momentary stall in the log consumer never
/// visibly drops events, small enough to bound worst-case memory
/// unconditionally rather than relying on the observed production rate
/// staying low.
pub const SUPERVISOR_EVENT_CHANNEL_CAPACITY: usize = 32;

/// Sent over a bounded `mpsc::Sender` in [`WorkerSupervisor::run`] via
/// `try_send` — never the blocking `send().await` (correction audit
/// "Supervisor event backpressure"). `try_send`'s `Err(Full)` is silently
/// dropped: losing a diagnostic event (for example, a duplicate
/// spawn-failure log during a slow-consumer burst) is acceptable, but
/// letting a stalled log consumer apply backpressure to Worker
/// supervision itself — which must keep spawning/reaping/respawning
/// regardless of whether anything is reading these events — is not.
#[derive(Debug)]
pub enum SupervisorEvent {
    WorkerStarted {
        pid: u32,
    },
    /// The child process exited (crash or clean exit) — distinct from
    /// [`SupervisorEvent::SpawnFailed`], which never produced a running
    /// child at all (Issue #37 "Restart policy": "Distinguish: spawn/
    /// configuration failure; child runtime crash").
    WorkerExited {
        pid: u32,
        status: std::io::Result<ExitStatus>,
    },
    /// The child process could not be started at all (for example, a
    /// misconfigured executable path) — `bamepd` still does not crash; it
    /// waits `restart_delay` and retries, since #37 does not attempt to
    /// classify a spawn failure as permanently unrecoverable.
    SpawnFailed(std::io::Error),
}

pub struct WorkerSupervisor {
    config: SupervisorConfig,
}

impl WorkerSupervisor {
    pub fn new(config: SupervisorConfig) -> Self {
        Self { config }
    }

    /// Runs the supervise loop forever until `shutdown` fires, spawning,
    /// observing, and respawning the Worker child with `restart_delay`
    /// between attempts. Never busy-spins — every iteration blocks on
    /// either the child's exit or the restart delay.
    ///
    /// `shutdown` is single-shot for this method's purposes: the only
    /// value this caller ever sends is `true`, exactly once, to request
    /// controlled shutdown. On shutdown, kills and reaps any currently
    /// running child (`Command::kill_on_drop(true)` is also set as a
    /// defense-in-depth backstop against an unexpected supervisor-task
    /// drop) before returning — never leaving a known child intentionally
    /// orphaned.
    pub async fn run(
        &self,
        mut shutdown: watch::Receiver<bool>,
        events: mpsc::Sender<SupervisorEvent>,
    ) {
        loop {
            if *shutdown.borrow() {
                return;
            }

            let mut command = Command::new(&self.config.worker_executable);
            command.envs(
                self.config
                    .env
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str())),
            );
            command.kill_on_drop(true);

            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(err) => {
                    let _ = events.try_send(SupervisorEvent::SpawnFailed(err));
                    if Self::sleep_or_shutdown(&mut shutdown, self.config.restart_delay).await {
                        return;
                    }
                    continue;
                }
            };

            let pid = child.id().expect("a freshly spawned child has a pid");
            let _ = events.try_send(SupervisorEvent::WorkerStarted { pid });

            tokio::select! {
                status = child.wait() => {
                    let _ = events.try_send(SupervisorEvent::WorkerExited { pid, status });
                }
                _ = shutdown.changed() => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return;
                }
            }

            if Self::sleep_or_shutdown(&mut shutdown, self.config.restart_delay).await {
                return;
            }
        }
    }

    /// Sleeps `delay`, returning `true` early if shutdown fires during the
    /// wait (so the caller can stop looping instead of respawning).
    async fn sleep_or_shutdown(shutdown: &mut watch::Receiver<bool>, delay: Duration) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(delay) => false,
            _ = shutdown.changed() => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Configuration-failure proof without spawning a real process: an
    /// executable path that cannot exist keeps `bamepd` alive, reports
    /// `SpawnFailed`, and keeps retrying rather than crashing the
    /// supervisor task.
    #[tokio::test]
    async fn spawn_failure_is_reported_and_does_not_stop_supervision() {
        let config = SupervisorConfig {
            worker_executable: PathBuf::from(if cfg!(windows) {
                "Z:\\definitely\\does\\not\\exist\\bamep-worker.exe"
            } else {
                "/definitely/does/not/exist/bamep-worker"
            }),
            env: vec![],
            restart_delay: Duration::from_millis(20),
        };
        let supervisor = WorkerSupervisor::new(config);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (events_tx, mut events_rx) = mpsc::channel(SUPERVISOR_EVENT_CHANNEL_CAPACITY);

        let handle = tokio::spawn(async move { supervisor.run(shutdown_rx, events_tx).await });

        // Two consecutive spawn failures prove the loop keeps retrying
        // rather than giving up after the first.
        assert!(matches!(
            events_rx.recv().await,
            Some(SupervisorEvent::SpawnFailed(_))
        ));
        assert!(matches!(
            events_rx.recv().await,
            Some(SupervisorEvent::SpawnFailed(_))
        ));

        shutdown_tx.send(true).expect("send shutdown");
        handle.await.expect("supervisor task");
    }

    /// Correction audit "Supervisor event backpressure": a consumer that
    /// never drains the channel must never block Worker supervision itself.
    /// With a full bounded channel and no reader, the supervisor must still
    /// keep spawning/reporting/retrying via `try_send`'s drop-on-full
    /// behavior rather than stalling on a blocking send.
    #[tokio::test]
    async fn a_full_event_channel_never_blocks_supervision() {
        let config = SupervisorConfig {
            worker_executable: PathBuf::from(if cfg!(windows) {
                "Z:\\definitely\\does\\not\\exist\\bamep-worker.exe"
            } else {
                "/definitely/does/not/exist/bamep-worker"
            }),
            env: vec![],
            restart_delay: Duration::from_millis(5),
        };
        let supervisor = WorkerSupervisor::new(config);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        // Capacity 1, and nothing ever reads from `_events_rx`: every event
        // after the first must be dropped via `try_send`, never block.
        let (events_tx, _events_rx) = mpsc::channel(1);

        let handle = tokio::spawn(async move { supervisor.run(shutdown_rx, events_tx).await });

        // If `try_send` were a blocking `send().await` instead, this would
        // hang forever once the channel filled up rather than completing
        // promptly.
        tokio::time::sleep(Duration::from_millis(100)).await;
        shutdown_tx.send(true).expect("send shutdown");
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("supervisor must remain responsive to shutdown even with a full, unread event channel")
            .expect("supervisor task");
    }
}
