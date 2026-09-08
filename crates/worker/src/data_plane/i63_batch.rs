//! Removable Issue #63 batch_8 clean-fast-path experiment.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::storage::{finalize_i63_batch, I63BatchTiming, Sha256Digest, StagingChunk};

type Member = (StagingChunk, Sha256Digest, u64);

pub(super) struct Coordinator {
    state: Mutex<State>,
    changed: Condvar,
    timeout: Duration,
    sink: Option<PathBuf>,
    valid_config: bool,
}

#[derive(Default)]
struct State {
    groups: HashMap<(Uuid, u64), Group>,
    failed: HashSet<Uuid>,
    requests: HashSet<(Uuid, u64)>,
}

struct Group {
    started: Instant,
    members: Vec<Member>,
    finalizing: bool,
    result: Option<bool>,
}

impl Coordinator {
    pub(super) fn from_env() -> Option<Arc<Self>> {
        let value = std::env::var_os("BAMEP_I63_WORKER_BATCH_FINALIZE")?;
        let mut coordinator = Self::new(Duration::from_secs(60));
        coordinator.valid_config = value == "8";
        coordinator.sink = std::env::var_os("BAMEP_I63_WORKER_BATCH_TIMING").map(PathBuf::from);
        if !coordinator.valid_config {
            eprintln!(
                "Issue #63 batch_8: BAMEP_I63_WORKER_BATCH_FINALIZE must equal 8; failing closed"
            );
        }
        Some(Arc::new(coordinator))
    }

    pub(super) fn new(timeout: Duration) -> Self {
        Self {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            timeout,
            sink: None,
            valid_config: true,
        }
    }

    pub(super) fn request(self: &Arc<Self>, transfer: Uuid, index: u64) -> RequestGuard {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !self.valid_config || index >= 32 || !state.requests.insert((transfer, index)) {
            state.failed.insert(transfer);
            self.changed.notify_all();
            eprintln!("Issue #63 batch_8: invalid geometry/configuration or repeated PUT; case contaminated");
        }
        RequestGuard {
            coordinator: self.clone(),
            transfer,
            committed: false,
        }
    }

    pub(super) fn failed(&self, transfer: Uuid) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failed
            .contains(&transfer)
    }

    fn fail(&self, transfer: Uuid) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.failed.insert(transfer);
        for ((id, _), group) in &mut state.groups {
            if *id == transfer && !group.finalizing {
                group.members.clear();
                group.result = Some(false);
            }
        }
        self.changed.notify_all();
    }

    pub(super) fn submit(&self, staged: StagingChunk) -> bool {
        let transfer = staged.transfer_id();
        let batch = staged.chunk_index() / 8;
        let key = (transfer, batch);
        let digest = staged.digest();
        let size = staged.staged_len();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.failed.contains(&transfer) {
            return false;
        }
        let group = state.groups.entry(key).or_insert_with(|| Group {
            started: Instant::now(),
            members: Vec::new(),
            finalizing: false,
            result: None,
        });
        if group.finalizing
            || group.result.is_some()
            || group
                .members
                .iter()
                .any(|(s, _, _)| s.chunk_index() == staged.chunk_index())
        {
            drop(state);
            self.fail(transfer);
            return false;
        }
        group.members.push((staged, digest, size));
        if group.members.len() == 8 {
            group.finalizing = true;
            let wait_ns = group.started.elapsed().as_nanos();
            let members = std::mem::take(&mut group.members);
            drop(state);
            let mut timing = I63BatchTiming::default();
            // No extra tasks: the eighth existing staging worker executes all
            // syncs serially. Catch unwind so every peer is always released.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                finalize_i63_batch(members, &mut timing)
            }));
            let ok = matches!(result, Ok(Ok(())));
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let ok = ok && !state.failed.contains(&transfer);
            let group = state.groups.get_mut(&key).expect("registered batch");
            group.finalizing = false;
            group.result = Some(ok);
            if !ok {
                state.failed.insert(transfer);
            }
            self.changed.notify_all();
            drop(state);
            self.record(
                transfer,
                batch,
                8,
                wait_ns,
                &timing,
                if ok { "durable" } else { "failed" },
            );
            return ok;
        }
        loop {
            if state.failed.contains(&transfer) {
                return false;
            }
            let group = state.groups.get(&key).expect("registered batch");
            if let Some(ok) = group.result {
                return ok;
            }
            if !group.finalizing && group.started.elapsed() >= self.timeout {
                let count = group.members.len();
                let waited = group.started.elapsed().as_nanos();
                drop(state);
                self.fail(transfer);
                self.record(
                    transfer,
                    batch,
                    count,
                    waited,
                    &I63BatchTiming::default(),
                    "missing_member_timeout",
                );
                eprintln!("Issue #63 batch_8: missing member timeout; case contaminated");
                return false;
            }
            let wait = if group.finalizing {
                self.timeout
            } else {
                self.timeout.saturating_sub(group.started.elapsed())
            };
            state = self
                .changed
                .wait_timeout(state, wait)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    fn record(
        &self,
        transfer: Uuid,
        batch: u64,
        count: usize,
        wait: u128,
        t: &I63BatchTiming,
        outcome: &str,
    ) {
        let Some(path) = &self.sink else {
            return;
        };
        let record = serde_json::json!({
            "transfer_id": transfer.to_string(), "batch_number": batch,
            "first_chunk_index": batch * 8, "last_chunk_index": batch * 8 + 7,
            "member_count": count, "wait_until_full_ns": wait,
            "file_sync_sum_ns": t.file_sync_sum_ns, "placement_sum_ns": t.placement_sum_ns,
            "dir_fsync_ns": t.dir_fsync_ns, "batch_finalize_total_ns": t.batch_finalize_total_ns,
            "outcome": outcome,
        });
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "{record}");
        }
    }
}

pub(super) struct RequestGuard {
    coordinator: Arc<Coordinator>,
    transfer: Uuid,
    committed: bool,
}

impl RequestGuard {
    pub(super) fn committed(&mut self) {
        self.committed = true;
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        if !self.committed {
            self.coordinator.fail(self.transfer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{ChunkStore, FilesystemChunkStore};
    use std::sync::{mpsc, Arc};
    use uuid::Uuid;

    #[test]
    fn installed_names_do_not_release_waiters_before_directory_fsync() {
        let root = std::env::temp_dir().join(format!("bamep-i63-dir-gate-{}", Uuid::new_v4()));
        let store = FilesystemChunkStore::initialize(&root).unwrap();
        let transfer = Uuid::new_v4();
        let c = Arc::new(Coordinator::new(Duration::from_secs(3)));
        let (tx, rx) = mpsc::channel();
        let mut handles = Vec::new();
        for index in 0..7 {
            let mut s = store.begin_stage(transfer, index, 4).unwrap();
            s.write(&[index as u8; 4]).unwrap();
            let c = c.clone();
            let tx = tx.clone();
            handles.push(std::thread::spawn(move || tx.send(c.submit(s)).unwrap()));
        }
        // Wait for the seven actual registrations, not a scheduling delay.
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let full = c
                .state
                .lock()
                .unwrap()
                .groups
                .get(&(transfer, 0))
                .is_some_and(|g| g.members.len() == 7);
            if full {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let (at_gate_tx, at_gate_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut s = store.begin_stage(transfer, 7, 4).unwrap();
        s.write(&[7; 4]).unwrap();
        handles.push(std::thread::spawn(move || {
            crate::storage::i63_test_directory_gate(move || {
                at_gate_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            });
            tx.send(c.submit(s)).unwrap();
        }));
        at_gate_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        for index in 0..8 {
            assert!(root
                .join(format!("transfers/{transfer}/chunks/{index}.chunk"))
                .is_file());
        }
        assert!(
            rx.try_recv().is_err(),
            "placement alone cannot release any waiter"
        );
        release_tx.send(()).unwrap();
        for _ in 0..8 {
            assert!(rx.recv_timeout(Duration::from_secs(3)).unwrap());
        }
        for handle in handles {
            handle.join().unwrap();
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_member_timeout_fails_all_waiters() {
        failed_group(false);
    }

    #[test]
    fn placement_failure_fails_all_waiters_and_preserves_residue() {
        failed_group(true);
    }

    fn failed_group(conflict: bool) {
        let root = std::env::temp_dir().join(format!("bamep-i63-fail-{}", Uuid::new_v4()));
        let store = FilesystemChunkStore::initialize(&root).unwrap();
        let transfer = Uuid::new_v4();
        if conflict {
            let mut existing = store.begin_stage(transfer, 4, 4).unwrap();
            existing.write(&[99; 4]).unwrap();
            existing.finalize().unwrap();
        }
        let coordinator = Arc::new(Coordinator::new(Duration::from_millis(100)));
        let (tx, rx) = mpsc::channel();
        let mut handles = Vec::new();
        let count = if conflict { 8 } else { 7 };
        for index in 0..count {
            let mut staged = store.begin_stage(transfer, index, 4).unwrap();
            staged.write(&[index as u8; 4]).unwrap();
            let c = coordinator.clone();
            let tx = tx.clone();
            handles.push(std::thread::spawn(move || {
                tx.send(c.submit(staged)).unwrap()
            }));
        }
        for _ in 0..count {
            assert!(!rx
                .recv_timeout(Duration::from_secs(3))
                .expect("every waiter exits"));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        if conflict {
            assert_eq!(
                std::fs::read(root.join(format!("transfers/{transfer}/chunks/4.chunk"))).unwrap(),
                [99; 4]
            );
            assert_eq!(
                std::fs::read(root.join(format!("transfers/{transfer}/chunks/0.chunk"))).unwrap(),
                [0; 4]
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancellation_guard_fails_peers_and_duplicate_request_contaminates() {
        let coordinator = Arc::new(Coordinator::new(Duration::from_secs(60)));
        let transfer = Uuid::new_v4();
        let guard = coordinator.request(transfer, 0);
        assert!(!coordinator.failed(transfer));
        drop(guard);
        assert!(coordinator.failed(transfer));
        let transfer = Uuid::new_v4();
        let mut first = coordinator.request(transfer, 0);
        first.committed();
        let _duplicate = coordinator.request(transfer, 0);
        assert!(coordinator.failed(transfer));
    }

    #[test]
    fn seven_members_cannot_cross_the_durability_barrier() {
        let root = std::env::temp_dir().join(format!("bamep-i63-batch-{}", Uuid::new_v4()));
        let store = FilesystemChunkStore::initialize(&root).unwrap();
        let transfer = Uuid::new_v4();
        let coordinator = Arc::new(Coordinator::new(Duration::from_secs(1)));
        let (tx, rx) = mpsc::channel();
        let mut handles = Vec::new();
        for index in 0..7 {
            let mut staged = store.begin_stage(transfer, index, 4).unwrap();
            staged.write(&[index as u8; 4]).unwrap();
            let c = coordinator.clone();
            let tx = tx.clone();
            handles.push(std::thread::spawn(move || {
                tx.send(c.submit(staged)).unwrap()
            }));
        }
        let premature = rx.recv_timeout(Duration::from_millis(100));
        let mut eighth = store.begin_stage(transfer, 7, 4).unwrap();
        eighth.write(&[7; 4]).unwrap();
        let eighth_result = coordinator.submit(eighth);
        for handle in handles {
            handle.join().unwrap();
        }
        std::fs::remove_dir_all(root).unwrap();
        assert!(premature.is_err(), "seven members must still be waiting");
        assert!(eighth_result);
        for _ in 0..7 {
            assert!(rx.recv().unwrap());
        }
    }
}
