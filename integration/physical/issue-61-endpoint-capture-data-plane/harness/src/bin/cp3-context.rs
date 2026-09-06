//! Issue #61 CP3 — establish one exact selected-source M1-shaped transfer
//! context for the REAL physical WinPE Endpoint, through existing
//! Domain/Application authority only.
//!
//! THROWAWAY Spike scaffolding. NOT production architecture, NOT `crates/agent`,
//! NOT the `bamepd` composition root, NOT the Administrative API.
//!
//! What this binary does, against the EXISTING physical-integration database
//! (`bamep_physint_spike` by default — it is NOT created or dropped here):
//!
//!   1. read the physical Endpoint's current authoritative `InventoryRevisionId`
//!      and its reported `capture_source_observation_id` / `capturable_sources`;
//!   2. select exactly ONE opaque `agent_source_id` -> the exact
//!      `SourceReference { inventory_revision_id, source_observation_id,
//!      agent_source_id }` tuple;
//!   3. create, through real Application services only:
//!        JobService::create_workflow            (2 steps: [0] real, [1] pressure)
//!        JobSchedulingService::admit / satisfy_current_step_preconditions
//!        TransferService::create_transfer_context   (descriptive SourceProvenance)
//!        TransferDispatchService::commit_transfer_dispatch  -> Attempt{Dispatched}
//!   4. PRESSURE CHECK: create a second transfer context whose descriptive
//!      SourceProvenance carries a structurally-valid but STALE tuple (the
//!      Endpoint's PREVIOUS inventory revision / source-observation epoch), and
//!      report what the current code actually does with it.
//!
//! The action is `bamep.m1.data-plane-transfer`. `SourceProvenance` is
//! descriptive-only immutable text (`m0-data-plane-and-storage-contracts.md`
//! "M1 scope of SourceProvenance"). This binary makes NO claim that
//! `bamep.m2.endpoint-capture-transfer`, Server-side `SourceReference`
//! freshness validation, or RF-6 atomic target creation exist. It reads ZERO
//! source bytes and never contacts the MiniPC.

use std::sync::Arc;

use bamep_domain::{
    Actor, ChunkSize, DigestAlgorithm, EndpointId, SourceProvenance, TransferDirection,
};
use bamep_server::adapters::postgres::{
    PostgresCredentialRedemptionRepository, PostgresEndpointRepository, PostgresJobRepository,
    PostgresTransferRepository,
};
use bamep_server::application::{
    EnrollmentService, JobSchedulingService, JobService, TransferDispatchResult,
    TransferDispatchService, TransferService,
};
use bamep_server::runtime::resource_arbiter::{ResourceClaim, ResourceKind, TechnicalResourceArbiter};
use serde_json::Value;
use uuid::Uuid;

const DEFAULT_DB_NAME: &str = "bamep_physint_spike";
const CHUNK_SIZE: u32 = 8 * 1024 * 1024;

macro_rules! ev {
    ($($arg:tt)*) => { println!("CP3 | {}", format!($($arg)*)) };
}

fn die(msg: impl AsRef<str>) -> ! {
    eprintln!("CP3 | RESULT: FAIL — {}", msg.as_ref());
    std::process::exit(1);
}

/// scheme://<redacted>@host/db — never surfaces userinfo or query string.
fn redact_dsn(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("postgresql", url));
    let rest = rest.split('?').next().unwrap_or(rest);
    let (authority, db) = rest.split_once('/').unwrap_or((rest, ""));
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    format!("{scheme}://<redacted>@{host}/{db}")
}

fn default_db_url() -> String {
    let nonempty = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let user = nonempty("USER")
        .or_else(|| nonempty("LOGNAME"))
        .unwrap_or_else(|| die("cannot derive the OS user for a peer-auth DSN; pass --db-url"));
    let socket = ["/run/postgresql", "/var/run/postgresql"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
        .unwrap_or("/tmp")
        .replace('/', "%2F");
    format!("postgresql://{user}@{socket}/{DEFAULT_DB_NAME}")
}

struct Args {
    db_url: String,
    endpoint_id: Uuid,
    select: String,
    stale_agent_source_id: Option<String>,
    approve_endpoint: bool,
    /// Re-read and print the final durable state of an already-created CP3
    /// job (no new mutation). Used to regenerate evidence without re-running
    /// the one-active-job-per-Endpoint creation path.
    dump_only: Option<Uuid>,
}

fn parse_args() -> Args {
    let mut db_url = None;
    let mut endpoint_id = None;
    let mut select = None;
    let mut stale = None;
    let mut approve_endpoint = false;
    let mut dump_only = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--db-url" => db_url = it.next(),
            "--endpoint-id" => endpoint_id = it.next(),
            "--select" => select = it.next(),
            "--stale-agent-source-id" => stale = it.next(),
            "--approve-endpoint" => approve_endpoint = true,
            "--dump-only" => {
                dump_only = Some(
                    it.next()
                        .unwrap_or_else(|| die("--dump-only needs a job_id"))
                        .parse()
                        .unwrap_or_else(|_| die("--dump-only job_id is not a valid UUID")),
                )
            }
            "-h" | "--help" => {
                eprintln!(
                    "usage: cp3-context --endpoint-id <uuid> --select <agent_source_id> \
                     [--approve-endpoint] [--stale-agent-source-id <id>] [--db-url <dsn>]\n\
                     --approve-endpoint: advance a PendingEnrollment physical Endpoint to \
                     Enrolled via the real EnrollmentService::approve_enrollment operator path \
                     (required because CP1 left trusted-bootstrap out of scope)."
                );
                std::process::exit(2);
            }
            other => die(format!("unknown argument: {other}")),
        }
    }
    let require = |o: Option<String>, name: &str| -> String {
        if dump_only.is_some() {
            o.unwrap_or_default()
        } else {
            o.unwrap_or_else(|| die(format!("{name} is required")))
        }
    };
    let endpoint_raw = require(endpoint_id, "--endpoint-id <uuid>");
    Args {
        db_url: db_url.unwrap_or_else(default_db_url),
        endpoint_id: if dump_only.is_some() && endpoint_raw.is_empty() {
            Uuid::nil()
        } else {
            endpoint_raw
                .parse()
                .unwrap_or_else(|_| die("--endpoint-id is not a valid UUID"))
        },
        select: require(select, "--select <agent_source_id>"),
        stale_agent_source_id: stale,
        approve_endpoint,
        dump_only,
    }
}

/// The #59 SourceReference tuple, exactly.
#[derive(Clone, Debug)]
struct SourceReference {
    inventory_revision_id: Uuid,
    source_observation_id: String,
    agent_source_id: String,
}

impl SourceReference {
    /// Descriptive-only provenance text stored verbatim in
    /// `transfers.source_provenance`. Explicitly labelled so nothing downstream
    /// can mistake it for a validated authority.
    fn descriptive_provenance(&self, note: &str) -> String {
        serde_json::json!({
            "_schema": "issue-61-cp3.descriptive-source-provenance",
            "descriptive_only": true,
            "not_a_validated_source_reference": true,
            "server_side_freshness_validation_exists": false,
            "note": note,
            "source_reference": {
                "inventory_revision_id": self.inventory_revision_id.to_string(),
                "source_observation_id": self.source_observation_id,
                "agent_source_id": self.agent_source_id,
            }
        })
        .to_string()
    }
}

struct Revision {
    revision_id: Uuid,
    recorded_at: String,
    observation_id: String,
    agent_source_ids: Vec<String>,
}

async fn load_revisions(pool: &sqlx::PgPool, endpoint_id: Uuid) -> Vec<Revision> {
    let rows: Vec<(Uuid, chrono::DateTime<chrono::Utc>, String)> = sqlx::query_as(
        "SELECT revision_id, recorded_at, inventory::text \
         FROM inventory_revisions WHERE endpoint_id = $1 ORDER BY recorded_at",
    )
    .bind(endpoint_id)
    .fetch_all(pool)
    .await
    .unwrap_or_else(|e| die(format!("query inventory_revisions: {e}")));

    rows.into_iter()
        .map(|(revision_id, recorded_at, inv_text)| {
            let inv: Value = serde_json::from_str(&inv_text)
                .unwrap_or_else(|e| die(format!("inventory json parse ({revision_id}): {e}")));
            let observation_id = inv
                .get("capture_source_observation_id")
                .and_then(Value::as_str)
                .unwrap_or_else(|| die(format!("revision {revision_id}: no capture_source_observation_id")))
                .to_string();
            let agent_source_ids = inv
                .get("capturable_sources")
                .and_then(Value::as_array)
                .unwrap_or_else(|| die(format!("revision {revision_id}: no capturable_sources array")))
                .iter()
                .map(|s| {
                    s.get("agent_source_id")
                        .and_then(Value::as_str)
                        .unwrap_or_else(|| die(format!("revision {revision_id}: capturable_sources entry has no agent_source_id")))
                        .to_string()
                })
                .collect();
            Revision {
                revision_id,
                recorded_at: recorded_at.to_rfc3339(),
                observation_id,
                agent_source_ids,
            }
        })
        .collect()
}

async fn dump_context(pool: &sqlx::PgPool, job_id: Uuid) {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT 'job'::text, id::text, state::text FROM jobs WHERE id = $1 \
         UNION ALL SELECT 'step', id::text, state::text FROM job_steps WHERE job_id = $1 \
         UNION ALL SELECT 'attempt', a.id::text, a.state::text FROM attempts a \
           JOIN job_steps s ON s.id = a.job_step_id WHERE s.job_id = $1 \
         ORDER BY 1,2",
    )
    .bind(job_id)
    .fetch_all(pool)
    .await
    .unwrap();
    for (k, id, state) in &rows {
        ev!("durable | {k:<8} {id} state={state}");
    }
    let tr: Vec<(Uuid, Uuid, String, String, i32, Option<Uuid>, String)> = sqlx::query_as(
        "SELECT t.id, t.artifact_id, t.direction::text, t.digest_algorithm::text, t.chunk_size, \
                t.attempt_id, t.source_provenance \
         FROM transfers t JOIN job_steps s ON s.id = t.job_step_id WHERE s.job_id = $1 \
         ORDER BY s.step_order",
    )
    .bind(job_id)
    .fetch_all(pool)
    .await
    .unwrap();
    for (tid, aid, dir, alg, cs, att, prov) in &tr {
        let art: (String, String) =
            sqlx::query_as("SELECT state::text, capture_consistency::text FROM artifacts WHERE id=$1")
                .bind(aid)
                .fetch_one(pool)
                .await
                .unwrap();
        let man: (bool, Option<i32>, i64) = sqlx::query_as(
            "SELECT cm.sealed, cm.chunk_count, \
                    (SELECT count(*) FROM chunk_identities WHERE artifact_id = cm.artifact_id) \
             FROM chunk_manifests cm WHERE cm.artifact_id = $1",
        )
        .bind(aid)
        .fetch_one(pool)
        .await
        .unwrap();
        ev!("durable | transfer {tid}");
        ev!("durable |   artifact_id={aid} direction={dir} digest_algorithm={alg} chunk_size={cs}");
        ev!("durable |   attempt_bound={} artifact_state={} capture_consistency={}", att.is_some(), art.0, art.1);
        ev!(
            "durable |   manifest sealed={} chunk_count={} chunk_identities={} (unsealed empty manifest expected at CP3)",
            man.0,
            man.1.map_or("null".to_string(), |c| c.to_string()),
            man.2
        );
        ev!("durable |   source_provenance={prov}");
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let args = parse_args();
    ev!("Issue #61 CP3 — physical selected-source M1-shaped context (bamep.m1.data-plane-transfer)");
    ev!("db={}  (NOT created, NOT dropped by CP3)", redact_dsn(&args.db_url));
    ev!("target physical endpoint_id={}", args.endpoint_id);

    // `connect` runs the embedded migrations; against an already-migrated
    // database (the running #60 harness migrates it on every startup) this is
    // an idempotent no-op. No schema change, no data change.
    let pool = bamep_server::adapters::postgres::connect(&args.db_url)
        .await
        .unwrap_or_else(|e| die(format!("connect: {e}")));

    if let Some(job_id) = args.dump_only {
        ev!("--dump-only {job_id}: re-reading final durable state, no mutation");
        let ep_row: Option<(Uuid, String)> = sqlx::query_as(
            "SELECT e.id, e.identity_state::text FROM endpoints e JOIN jobs j ON j.endpoint_id = e.id WHERE j.id = $1",
        )
        .bind(job_id)
        .fetch_optional(&pool)
        .await
        .unwrap();
        let (ep_id, ident) = ep_row.unwrap_or_else(|| die("job_id not found"));
        ev!("endpoint | id={ep_id} identity_state={ident}");
        dump_context(&pool, job_id).await;
        // provenance verification: both transfers' stored provenance parses and
        // carries a #59-shaped tuple.
        let provs: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT t.id, t.source_provenance FROM transfers t \
             JOIN job_steps s ON s.id = t.job_step_id WHERE s.job_id = $1 ORDER BY s.step_order",
        )
        .bind(job_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        for (tid, p) in &provs {
            let v: Value = serde_json::from_str(p).unwrap_or_else(|e| die(format!("provenance {tid} not JSON: {e}")));
            let sr = &v["source_reference"];
            ev!(
                "provenance | transfer {tid}: descriptive_only={} sr={{inv={}, obs={}, agent={}}}",
                v["descriptive_only"], sr["inventory_revision_id"], sr["source_observation_id"], sr["agent_source_id"]
            );
        }
        ev!("RESULT: PASS (dump-only)");
        return;
    }

    // ---- 1. physical Endpoint + current authoritative inventory ----------
    let ep: Option<(Uuid, String, String, String, Option<Uuid>, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as(
            "SELECT id, inventory_signal, identity_state::text, trusted_bootstrap_state::text, \
                    current_inventory_revision_id, created_at \
             FROM endpoints WHERE id = $1",
        )
        .bind(args.endpoint_id)
        .fetch_optional(&pool)
        .await
        .unwrap_or_else(|e| die(format!("query endpoints: {e}")));
    let (ep_id, signal, identity_state, tb_state, current_rev, created_at) =
        ep.unwrap_or_else(|| die("physical Endpoint id not found in this database"));
    ev!("endpoint | id={ep_id}");
    ev!("endpoint | inventory_signal={signal} identity_state={identity_state} trusted_bootstrap_state={tb_state}");
    ev!("endpoint | created_at={}", created_at.to_rfc3339());
    let current_rev = current_rev
        .unwrap_or_else(|| die("physical Endpoint has no current_inventory_revision_id — no authoritative source epoch"));

    let revisions = load_revisions(&pool, ep_id).await;
    if revisions.is_empty() {
        die("physical Endpoint has zero inventory revisions");
    }
    let current = revisions
        .iter()
        .find(|r| r.revision_id == current_rev)
        .unwrap_or_else(|| die("current_inventory_revision_id does not match any inventory_revisions row"));

    ev!("inventory | current InventoryRevisionId = {}", current.revision_id);
    ev!("inventory | recorded_at = {}", current.recorded_at);
    ev!("inventory | capture_source_observation_id = {} (len {})", current.observation_id, current.observation_id.len());
    ev!("inventory | capturable_sources count = {}", current.agent_source_ids.len());
    for (i, id) in current.agent_source_ids.iter().enumerate() {
        ev!("inventory |   [{i}] agent_source_id = {id}");
    }

    // structural checks the #59 contract requires
    if current.observation_id.len() != 43 {
        die("capture_source_observation_id is not the canonical 43-char length");
    }
    if current.agent_source_ids.len() != 2 {
        die(format!("expected exactly 2 capturable_sources, found {}", current.agent_source_ids.len()));
    }
    {
        let mut uniq = current.agent_source_ids.clone();
        uniq.sort();
        uniq.dedup();
        if uniq.len() != current.agent_source_ids.len() {
            die("duplicate agent_source_id in the current epoch — no SourceReference is selectable (RF-4)");
        }
    }

    // ---- 2. select exactly one opaque source -> the exact tuple ----------
    if !current.agent_source_ids.contains(&args.select) {
        die(format!(
            "--select '{}' is not one of the current epoch's capturable_sources",
            args.select
        ));
    }
    let selected = SourceReference {
        inventory_revision_id: current.revision_id,
        source_observation_id: current.observation_id.clone(),
        agent_source_id: args.select.clone(),
    };
    ev!("SELECTED SourceReference (authoritative):");
    ev!("  inventory_revision_id = {}", selected.inventory_revision_id);
    ev!("  source_observation_id = {}", selected.source_observation_id);
    ev!("  agent_source_id       = {}", selected.agent_source_id);
    ev!("  (selected purely by the tuple; local PhysicalDriveN/model/serial were NOT consulted)");

    // ---- 2b. enrollment gate --------------------------------------------
    // `JobService::create_workflow` requires identity_state == Enrolled. CP1
    // deliberately left this Endpoint PendingEnrollment (trusted-bootstrap was
    // out of CP1 scope). Advance it through the REAL operator-decision path —
    // the same `EnrollmentService::approve_enrollment` call CP2's fixture uses.
    // Not a bypass, not a new seam; gated behind an explicit flag because it
    // mutates real physical Endpoint identity state.
    if identity_state != "Enrolled" {
        if !args.approve_endpoint {
            die(format!(
                "physical Endpoint is {identity_state}; create_workflow needs Enrolled. \
                 Re-run with --approve-endpoint to advance it via \
                 EnrollmentService::approve_enrollment (existing Application authority)."
            ));
        }
        ev!("enroll | physical Endpoint is {identity_state}; advancing via EnrollmentService::approve_enrollment(Actor::Operator)");
        let enrollment = EnrollmentService::new(
            Arc::new(PostgresEndpointRepository::new(pool.clone())),
            Arc::new(PostgresCredentialRedemptionRepository::new(pool.clone())),
        );
        enrollment
            .approve_enrollment(
                EndpointId(ep_id),
                Actor::Operator {
                    label: "issue-61-cp3-harness".into(),
                },
                chrono::Utc::now(),
            )
            .await
            .unwrap_or_else(|e| die(format!("approve_enrollment: {e:?}")));
        let now_state: String =
            sqlx::query_scalar("SELECT identity_state::text FROM endpoints WHERE id = $1")
                .bind(ep_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        ev!("enroll | physical Endpoint identity_state is now {now_state} (EndpointEnrolled + OperatorDecisionRecorded emitted)");
    }

    // ---- 3. Application-created durable context (step 0 = real) ----------
    let job_repo = Arc::new(PostgresJobRepository::new(pool.clone()));
    let transfer_repo = Arc::new(PostgresTransferRepository::new(pool.clone()));
    let arbiter = Arc::new(TechnicalResourceArbiter::new([(ResourceKind::new("network"), 10)]));
    let jobs = JobService::new(Arc::clone(&job_repo));
    let scheduling = JobSchedulingService::new(Arc::clone(&job_repo));
    let transfers = TransferService::new(Arc::clone(&transfer_repo));
    let dispatch = TransferDispatchService::new(Arc::clone(&job_repo), Arc::clone(&arbiter));
    let endpoint = EndpointId(ep_id);

    ev!("apply | JobService::create_workflow(endpoint, step_count=2)  [step0=real, step1=pressure]");
    let job = jobs
        .create_workflow(endpoint, 2)
        .await
        .unwrap_or_else(|e| die(format!("create_workflow: {e:?}")));
    let step0 = job.steps[0].id;
    let step1 = job.steps[1].id;
    ev!("apply | job_id={} step0={} step1={}", job.id.0, step0.0, step1.0);

    ev!("apply | JobSchedulingService::admit(job) -> Running");
    scheduling
        .admit(job.id)
        .await
        .unwrap_or_else(|e| die(format!("admit: {e:?}")));
    ev!("apply | JobSchedulingService::satisfy_current_step_preconditions(step0)");
    scheduling
        .satisfy_current_step_preconditions(job.id, step0)
        .await
        .unwrap_or_else(|e| die(format!("satisfy step0: {e:?}")));

    ev!("apply | TransferService::create_transfer_context(step0, AgentToServer, Sha256, chunk_size={CHUNK_SIZE}, descriptive-provenance[current tuple])");
    let ctx0 = transfers
        .create_transfer_context(
            endpoint,
            job.id,
            step0,
            TransferDirection::AgentToServer,
            DigestAlgorithm::Sha256,
            ChunkSize::new(CHUNK_SIZE).unwrap(),
            SourceProvenance::new(selected.descriptive_provenance("CP3 selected current physical source")),
        )
        .await
        .unwrap_or_else(|e| die(format!("create_transfer_context step0: {e:?}")));
    ev!("apply | -> transfer_id={} artifact_id={} (Artifact Incomplete, empty ChunkManifest)", ctx0.transfer.id.0, ctx0.transfer.artifact_id.0);

    ev!("apply | TransferDispatchService::commit_transfer_dispatch(step0) -> Attempt{{Dispatched}}");
    let committed = match dispatch
        .commit_transfer_dispatch(
            job.id,
            step0,
            ctx0.transfer.id,
            vec![ResourceClaim::new(ResourceKind::new("network"), 1)],
        )
        .await
        .unwrap_or_else(|e| die(format!("commit_transfer_dispatch: {e:?}")))
    {
        TransferDispatchResult::Committed { outcome, .. } => outcome,
        other => die(format!("transfer dispatch not committed: {other:?}")),
    };
    ev!(
        "apply | -> attempt_id={} action_id={} (action_type=bamep.m1.data-plane-transfer; NOT the M2 action)",
        committed.attempt.id.0, committed.attempt.action_id.0
    );

    // ---- 4. PRESSURE CHECK — stale but structurally-valid tuple ----------
    let stale_rev = revisions
        .iter()
        .filter(|r| r.revision_id != current.revision_id)
        .next_back()
        .unwrap_or_else(|| die("no previous inventory revision available for the stale pressure check"));
    let stale_agent_source_id = args
        .stale_agent_source_id
        .clone()
        .unwrap_or_else(|| stale_rev.agent_source_ids[0].clone());
    if !stale_rev.agent_source_ids.contains(&stale_agent_source_id) {
        die("--stale-agent-source-id is not in the previous revision's capturable_sources");
    }
    let stale = SourceReference {
        inventory_revision_id: stale_rev.revision_id,
        source_observation_id: stale_rev.observation_id.clone(),
        agent_source_id: stale_agent_source_id,
    };
    ev!("");
    ev!("pressure | STALE candidate (structurally valid, same Endpoint, PREVIOUS epoch — superseded by the current one):");
    ev!("pressure |   inventory_revision_id = {}  (current is {})", stale.inventory_revision_id, current.revision_id);
    ev!("pressure |   source_observation_id = {}  (current is {})", stale.source_observation_id, current.observation_id);
    ev!("pressure |   agent_source_id       = {}", stale.agent_source_id);
    ev!("pressure | feeding it to TransferService::create_transfer_context(step1) ...");
    let ctx1_result = transfers
        .create_transfer_context(
            endpoint,
            job.id,
            step1,
            TransferDirection::AgentToServer,
            DigestAlgorithm::Sha256,
            ChunkSize::new(CHUNK_SIZE).unwrap(),
            SourceProvenance::new(stale.descriptive_provenance("CP3 PRESSURE CHECK — deliberately STALE tuple")),
        )
        .await;
    match ctx1_result {
        Ok(ctx1) => {
            let stored: String = sqlx::query_scalar("SELECT source_provenance FROM transfers WHERE id = $1")
                .bind(ctx1.transfer.id.0)
                .fetch_one(&pool)
                .await
                .unwrap();
            let stored_matches = stored
                == stale.descriptive_provenance("CP3 PRESSURE CHECK — deliberately STALE tuple");
            ev!("pressure | RESULT: ACCEPTED with NO error. transfer_id={} artifact_id={}", ctx1.transfer.id.0, ctx1.transfer.artifact_id.0);
            ev!("pressure | the stale tuple was stored verbatim as opaque provenance text (round-trips exactly: {stored_matches})");
            ev!("pressure | -> the current M1-shaped creation path performs NO check of:");
            ev!("pressure |      current InventoryRevisionId  ==  source_reference.inventory_revision_id");
            ev!("pressure |      current capture_source_observation_id  ==  source_reference.source_observation_id");
            ev!("pressure |      agent_source_id resolves within the current capturable_sources");
            ev!("pressure | this is the ABSENT RF-2 / RF-6 Server-side freshness seam — recorded, NOT a bug in the M1 path, NOT fixed here.");
        }
        Err(e) => {
            ev!("pressure | RESULT: REJECTED — {e:?}");
            ev!("pressure | inspecting the exact reason (this would be an EXISTING unrelated check, not RF-2/RF-6)...");
        }
    }

    // ---- 5. durable state + zero-byte-read attestation ------------------
    ev!("");
    ev!("---- FINAL DURABLE STATE (job {}) ----", job.id.0);
    dump_context(&pool, job.id.0).await;

    ev!("");
    ev!("---- ZERO SOURCE BYTES READ ----");
    ev!("no-read | this process opened no \\\\.\\PhysicalDrive*, no /dev/sd*, no block device, no capture file");
    ev!("no-read | its only I/O is PostgreSQL (Application services) + stdout");
    ev!("no-read | the MiniPC was not contacted; no WinPE run, no probe execution, no Agent session in CP3");
    ev!("no-read | CP3 reuses the CP1-established physically-authenticated Endpoint; its current inventory epoch is unchanged since CP1");

    ev!("");
    ev!("---- M2 SEAMS NOT SATISFIED (unchanged from CP0/CP2) ----");
    for s in [
        "bamep.m2.endpoint-capture-transfer action (type/params/rejections) — not implemented",
        "Server-side SourceReference freshness validation (RF-2/RF-6) — DEMONSTRATED ABSENT by the pressure check above",
        "Agent-side SOURCE_REFERENCE_STALE — not implemented (no product consumer resolves an authoritative SourceReference)",
        "RF-6 atomic M2 target creation — create_workflow/create_transfer_context/commit are separate calls; no capture-intent JobStep",
        "structured SourceProvenance authority — SourceProvenance is opaque String; the tuple rode as descriptive text only",
    ] {
        ev!("  MISSING: {s}");
    }

    ev!("");
    ev!("RESULT: PASS");
}
