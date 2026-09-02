//! Headless operational-core scale and persistence validation.
//!
//! These tests keep correctness assertions separate from measurements. They
//! exercise 24 real durable Endpoint workflows against PostgreSQL and 24
//! concurrent Agent -> Worker chunked transfers across the existing real WSS,
//! TLS/HTTPS, AF_UNIX, Worker, and PostgreSQL integration harness.

#![cfg(unix)]

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bamep_agent_protocol::AgentProtocolMessage;
use bamep_domain::{
    Actor, BootNonce, ChunkSize, DigestAlgorithm, InventorySnapshot, JobId, JobStepId,
    SourceProvenance, TransferDirection, TransferId,
};
use bamep_server::adapters::postgres::{
    PostgresBootContextRepository, PostgresCredentialRedemptionRepository,
    PostgresEndpointRepository, PostgresInventoryRepository, PostgresJobRepository,
    PostgresTransferRepository,
};
use bamep_server::application::{
    BootOrchestrationService, EnrollmentService, JobSchedulingService, JobService, RedeemResult,
    TransferDispatchResult, TransferDispatchService, TransferService,
};
use bamep_server::ports::InventoryRepository;
use bamep_server::runtime::resource_arbiter::{
    ResourceClaim, ResourceKind, TechnicalResourceArbiter,
};
use bamep_simulator::{
    AgentTransferAuthorization, DataPlaneTransferDirection, InMemoryTransferSource,
    TransferActionResult, TransferRunOptions, TransferRunOutcome,
};
use chrono::{Duration as ChronoDuration, Utc};
use serde_json::{Map, Value};
use sqlx::PgPool;
use support::transfer_vertical::{run_transfer_streaming_progress, Vertical, SOURCE_LEN};
use support::TestDatabase;
use tokio::sync::Barrier;

const ENDPOINT_COUNT: usize = 24;
const RESOURCE_CAPACITY: u64 = 8;

struct Services {
    boot: BootOrchestrationService<PostgresBootContextRepository>,
    enrollment:
        EnrollmentService<PostgresEndpointRepository, PostgresCredentialRedemptionRepository>,
    jobs: JobService<PostgresJobRepository>,
    scheduling: JobSchedulingService<PostgresJobRepository>,
    transfers: TransferService<PostgresTransferRepository>,
    inventory: Arc<PostgresInventoryRepository>,
    job_repo: Arc<PostgresJobRepository>,
}

fn services(pool: PgPool) -> Services {
    let job_repo = Arc::new(PostgresJobRepository::new(pool.clone()));
    let inventory = Arc::new(PostgresInventoryRepository::new(pool.clone()));
    Services {
        boot: BootOrchestrationService::new(
            Arc::new(PostgresBootContextRepository::new(pool.clone())),
            ChronoDuration::minutes(5),
        ),
        enrollment: EnrollmentService::new(
            Arc::new(PostgresEndpointRepository::new(pool.clone())),
            Arc::new(PostgresCredentialRedemptionRepository::new(pool.clone())),
        ),
        jobs: JobService::new(Arc::clone(&job_repo)),
        scheduling: JobSchedulingService::new(Arc::clone(&job_repo)),
        transfers: TransferService::new(Arc::new(PostgresTransferRepository::new(pool))),
        inventory,
        job_repo,
    }
}

#[derive(Clone, Copy)]
struct ReadyTransfer {
    job_id: JobId,
    step_id: JobStepId,
    transfer_id: TransferId,
}

async fn prepare_endpoint(services: &Services, index: usize) -> ReadyTransfer {
    let now = Utc::now();
    let signal = format!("m1-scale-scheduler-{index:02}");
    let credential = services
        .boot
        .issue_enrollment_credential(&signal, BootNonce::generate().unwrap(), now)
        .await
        .unwrap();
    let RedeemResult::Established { endpoint_id, .. } = services
        .enrollment
        .redeem(&credential.to_wire_value())
        .await
        .unwrap()
    else {
        panic!("endpoint {index} did not establish");
    };
    services
        .enrollment
        .approve_enrollment(
            endpoint_id,
            Actor::Operator {
                label: "m1-scale-validation".into(),
            },
            now,
        )
        .await
        .unwrap();

    let mut inventory = Map::new();
    inventory.insert("simulator_index".into(), Value::from(index as u64));
    services
        .inventory
        .record_inventory(endpoint_id, InventorySnapshot(inventory), now)
        .await
        .unwrap()
        .expect("first inventory must create a durable revision");

    let job = services.jobs.create_workflow(endpoint_id, 1).await.unwrap();
    let step_id = job.steps[0].id;
    services.scheduling.admit(job.id).await.unwrap();
    services
        .scheduling
        .satisfy_current_step_preconditions(job.id, step_id)
        .await
        .unwrap();
    let transfer = services
        .transfers
        .create_transfer_context(
            endpoint_id,
            job.id,
            step_id,
            TransferDirection::AgentToServer,
            DigestAlgorithm::Sha256,
            ChunkSize::new(4096).unwrap(),
            SourceProvenance::new(format!("simulated-disk-{index:02}")),
        )
        .await
        .unwrap();

    ReadyTransfer {
        job_id: job.id,
        step_id,
        transfer_id: transfer.transfer.id,
    }
}

fn percentile(samples: &mut [Duration], numerator: usize, denominator: usize) -> Duration {
    samples.sort_unstable();
    let index = ((samples.len() - 1) * numerator).div_ceil(denominator);
    samples[index]
}

async fn durable_counts(pool: &PgPool) -> Vec<(&'static str, i64)> {
    let mut counts = Vec::new();
    for table in [
        "endpoints",
        "inventory_revisions",
        "audit_records",
        "domain_events",
        "jobs",
        "job_steps",
        "attempts",
        "artifacts",
        "transfers",
        "chunk_manifests",
        "chunk_identities",
    ] {
        let query = format!("SELECT COUNT(*) FROM {table}");
        let count = sqlx::query_scalar(sqlx::AssertSqlSafe(query))
            .fetch_one(pool)
            .await
            .unwrap();
        counts.push((table, count));
    }
    counts
}

async fn observe_peak_checked_out(pool: PgPool, running: Arc<AtomicBool>) -> u32 {
    let mut peak = 0;
    while running.load(Ordering::Relaxed) {
        peak = peak.max(pool.size().saturating_sub(pool.num_idle() as u32));
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    peak.max(pool.size().saturating_sub(pool.num_idle() as u32))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn twenty_four_endpoints_create_durable_load_and_contend_for_scheduler_resources() {
    let db = TestDatabase::setup().await;
    let services = Arc::new(services(db.pool.clone()));
    let setup_barrier = Arc::new(Barrier::new(ENDPOINT_COUNT));
    let observing = Arc::new(AtomicBool::new(true));
    let observer = tokio::spawn(observe_peak_checked_out(
        db.pool.clone(),
        Arc::clone(&observing),
    ));
    let workload_start = Instant::now();
    let mut setup_tasks = Vec::new();

    for index in 0..ENDPOINT_COUNT {
        let services = Arc::clone(&services);
        let barrier = Arc::clone(&setup_barrier);
        setup_tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            let started = Instant::now();
            let transfer = prepare_endpoint(&services, index).await;
            (transfer, started.elapsed())
        }));
    }

    let mut ready = Vec::new();
    let mut setup_latencies = Vec::new();
    for task in setup_tasks {
        let (transfer, latency) = task.await.unwrap();
        ready.push(transfer);
        setup_latencies.push(latency);
    }
    assert_eq!(ready.len(), ENDPOINT_COUNT);

    let arbiter = Arc::new(TechnicalResourceArbiter::new([(
        ResourceKind::new("network"),
        RESOURCE_CAPACITY,
    )]));
    let dispatch_barrier = Arc::new(Barrier::new(ENDPOINT_COUNT));
    let mut dispatch_tasks = Vec::new();
    for transfer in ready {
        let barrier = Arc::clone(&dispatch_barrier);
        let service =
            TransferDispatchService::new(Arc::clone(&services.job_repo), Arc::clone(&arbiter));
        dispatch_tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            let started = Instant::now();
            let outcome = service
                .commit_transfer_dispatch(
                    transfer.job_id,
                    transfer.step_id,
                    transfer.transfer_id,
                    vec![ResourceClaim::new(ResourceKind::new("network"), 1)],
                )
                .await
                .unwrap();
            (transfer, outcome, started.elapsed())
        }));
    }

    let mut committed = Vec::new();
    let mut unavailable = 0;
    let mut dispatch_latencies = Vec::new();
    for task in dispatch_tasks {
        let (transfer, outcome, latency) = task.await.unwrap();
        dispatch_latencies.push(latency);
        match outcome {
            TransferDispatchResult::Committed { reservation, .. } => {
                committed.push((transfer, reservation));
            }
            TransferDispatchResult::ResourceUnavailable => unavailable += 1,
            TransferDispatchResult::Rejected(reason) => {
                panic!("unexpected dispatch rejection: {reason:?}")
            }
        }
    }

    assert_eq!(committed.len(), RESOURCE_CAPACITY as usize);
    assert_eq!(unavailable, ENDPOINT_COUNT - RESOURCE_CAPACITY as usize);
    for (transfer, reservation) in &committed {
        let state: String = sqlx::query_scalar("SELECT state::text FROM job_steps WHERE id = $1")
            .bind(transfer.step_id.0)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(state, "Dispatching");
        arbiter.release(*reservation);
    }
    let full_capacity = arbiter
        .acquire(vec![ResourceClaim::new(
            ResourceKind::new("network"),
            RESOURCE_CAPACITY,
        )])
        .expect("all committed reservations must release cleanly");
    arbiter.release(full_capacity);

    let counts = durable_counts(&db.pool).await;
    observing.store(false, Ordering::Relaxed);
    let peak_checked_out = observer.await.unwrap();
    let total_rows: i64 = counts.iter().map(|(_, count)| count).sum();
    assert_eq!(
        counts
            .iter()
            .find(|(name, _)| *name == "endpoints")
            .unwrap()
            .1,
        ENDPOINT_COUNT as i64
    );
    assert_eq!(
        counts
            .iter()
            .find(|(name, _)| *name == "inventory_revisions")
            .unwrap()
            .1,
        ENDPOINT_COUNT as i64
    );

    let setup_p95 = percentile(&mut setup_latencies, 95, 100);
    let setup_max = *setup_latencies.iter().max().unwrap();
    let dispatch_p95 = percentile(&mut dispatch_latencies, 95, 100);
    let dispatch_max = *dispatch_latencies.iter().max().unwrap();
    eprintln!(
        "M1_SCALE scheduler endpoints={ENDPOINT_COUNT} capacity={RESOURCE_CAPACITY} committed={} resource_backpressured={unavailable} pg_pool_max=10 pg_peak_checked_out={peak_checked_out} durable_rows={total_rows} rows={counts:?} setup_p95_ms={} setup_max_ms={} dispatch_p95_ms={} dispatch_max_ms={} elapsed_ms={}",
        committed.len(),
        setup_p95.as_millis(),
        setup_max.as_millis(),
        dispatch_p95.as_millis(),
        dispatch_max.as_millis(),
        workload_start.elapsed().as_millis(),
    );

    db.teardown().await;
}

async fn run_transfer(vertical: Vertical, barrier: Arc<Barrier>) -> (Vertical, Duration) {
    let mut session = vertical.connect_agent().await;
    vertical.dispatch_transfer(&session).await;
    let dispatch = session.expect_dispatch().await;
    let agent = vertical.agent();
    let response = agent.accept(&dispatch);
    let accepted = response
        .accepted
        .expect("transfer dispatch must be accepted");
    session
        .send(AgentProtocolMessage::ActionAck(response.ack))
        .await;
    let (proof_key, grant) = session
        .obtain_grant(vertical.fixture.action_id, vertical.fixture.transfer.id.0)
        .await;
    let authorization = AgentTransferAuthorization::new(
        proof_key,
        grant.body.token,
        vertical.fixture.transfer.id.0,
        vertical.fixture.transfer.artifact_id.0,
        DataPlaneTransferDirection::AgentToServer,
        grant.body.data_plane_base_url,
    );
    let source = InMemoryTransferSource::pattern(SOURCE_LEN, 21);

    barrier.wait().await;
    let started = Instant::now();
    let run = run_transfer_streaming_progress(
        &agent,
        &accepted,
        &authorization,
        &source,
        &TransferRunOptions::default(),
        &mut session,
        vertical.fixture.action_id,
    )
    .await;
    let elapsed = started.elapsed();
    let TransferRunOutcome::Completed(TransferActionResult::Verified { artifact_id }) = run.outcome
    else {
        panic!("concurrent transfer did not verify: {:?}", run.outcome);
    };
    assert_eq!(artifact_id, vertical.fixture.transfer.artifact_id.0);
    assert_eq!(run.progress_observed, vec![0, 4096, 8192, 10_000]);
    session
        .send(AgentProtocolMessage::ActionResult(
            TransferActionResult::Verified { artifact_id }
                .into_action_result(vertical.fixture.action_id),
        ))
        .await;
    session.close_and_join().await;
    assert_eq!(vertical.artifact_state().await, "Verified");
    assert_eq!(vertical.attempt_state().await, "Succeeded");
    assert_eq!(vertical.job_state().await, "Succeeded");
    (vertical, elapsed)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn twenty_four_endpoints_transfer_chunks_concurrently_to_verified_artifacts() {
    let db = TestDatabase::setup().await;
    let setup_started = Instant::now();
    let mut verticals = Vec::new();
    for index in 0..ENDPOINT_COUNT {
        verticals.push(Vertical::start(&db, &format!("m1-scale-transfer-{index:02}")).await);
    }
    let setup_elapsed = setup_started.elapsed();
    let barrier = Arc::new(Barrier::new(ENDPOINT_COUNT));
    let workload_started = Instant::now();
    let mut tasks = Vec::new();
    for vertical in verticals {
        tasks.push(tokio::spawn(run_transfer(vertical, Arc::clone(&barrier))));
    }

    let mut completed = Vec::new();
    let mut latencies = Vec::new();
    for task in tasks {
        let (vertical, latency) = task.await.unwrap();
        completed.push(vertical);
        latencies.push(latency);
    }
    assert_eq!(completed.len(), ENDPOINT_COUNT);
    let verified: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM artifacts WHERE state = 'Verified'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    let held_chunks: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM chunk_identities WHERE held = TRUE")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(verified, ENDPOINT_COUNT as i64);
    assert_eq!(held_chunks, (ENDPOINT_COUNT * 3) as i64);

    let counts = durable_counts(&db.pool).await;
    let p95 = percentile(&mut latencies, 95, 100);
    let max = *latencies.iter().max().unwrap();
    let bytes = ENDPOINT_COUNT * SOURCE_LEN;
    eprintln!(
        "M1_SCALE data_plane endpoints={ENDPOINT_COUNT} concurrent_transfers={} bytes={bytes} verified_artifacts={verified} held_chunks={held_chunks} rows={counts:?} setup_ms={} transfer_p95_ms={} transfer_max_ms={} concurrent_elapsed_ms={}",
        completed.len(),
        setup_elapsed.as_millis(),
        p95.as_millis(),
        max.as_millis(),
        workload_started.elapsed().as_millis(),
    );

    drop(completed);
    db.teardown().await;
}
