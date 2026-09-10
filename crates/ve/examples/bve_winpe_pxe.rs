//! Issue #71 host-proof building blocks: PXE-boot the existing iPXE + wimboot
//! WinPE path in one BVE over **virtual UEFI** (OVMF non-Secure-Boot), on the
//! isolated #70 provisioning network, and observe stock WinPE reach its
//! automatic network-stack initialisation — all headless, with no ISO / disk /
//! alternate bootable device that could masquerade as network-boot success.
//!
//! **The primary way to run the proof is `scripts/bve-winpe-pxe-proof.sh`**,
//! which orchestrates the whole cycle (two accepted boots, stage parsing,
//! fail-closed shutdown ordering, cleanup). These subcommands stay for manual
//! debugging:
//!
//! - non-privileged: `plan`, `env`, `check`, `check-artifacts`, `verify-clean`;
//! - privileged (run under `sudo`): `setup` (isolated net + broad bridged
//!   FORWARD accommodation), `start-fixture` (stage artifacts + `dnsmasq`
//!   DHCP/TFTP + `python3` HTTP, foreground), `teardown`;
//! - normal user: `run-bve` (UEFI + E1000 + NetworkFirst, two boots; with
//!   `--evidence-log <fixture log>` it emits the per-boot line ranges the
//!   harness uses to prove WinPE readiness independently for each boot).
//!
//! The retained boot artifacts are NOT in the repo. Point
//! `BAMEP_BVE_WINPE_FIXTURE_ROOT` at a directory holding `snponly.efi`,
//! `wimboot`, `BCD`, `boot.sdi`, `boot.wim`; their SHA-256 is pinned in
//! `scripts/winpe-pxe-fixture.sha256`. A missing / mismatched artifact is an
//! actionable error — never a download, never a fallback.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use bamep_ve::{
    apply_bridged_forward_accommodation, bve_run_dir, check_network_prerequisites,
    check_uefi_firmware, destroy_instance_storage, detect_host_prerequisites, ensure_system_base,
    fixture_command, fixture_pid_file, fixture_run_dir, prepare_instance, prepare_network,
    remove_bridged_forward_accommodation, residual_resources, teardown_network,
    winpe_boot_ipxe_script, winpe_fixture_dnsmasq_argv, winpe_http_base_url,
    winpe_http_fixture_command, winpe_http_root, winpe_tftp_root, BootMode, BveId, BveNetworkPlan,
    BveRuntime, BveStorageRoot, Firmware, LifecycleState, MacAddress, NicModel, PreparedBveNetwork,
    RuntimeRoot, SystemBaseSpec, PYTHON3_BINARY, WINPE_TFTP_BOOTFILE,
};

type R = Result<(), Box<dyn Error>>;

/// Print the error with `Display` (readable multi-line messages) rather than the
/// `Result`-returning-`main` default of `Debug`.
fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

const FIXTURE_ROOT_ENV: &str = "BAMEP_BVE_WINPE_FIXTURE_ROOT";
const MANIFEST_ENV: &str = "BAMEP_BVE_WINPE_MANIFEST";
const DEFAULT_MANIFEST: &str = "scripts/winpe-pxe-fixture.sha256";

/// How long each boot is held so the PXE -> iPXE -> wimboot (340 MB) -> WinPE
/// chain and the WinPE-originated DHCP DORA can complete and be logged. The
/// physical evidence saw the WinPE DORA ~40 s after the PXE DORA; this leaves
/// margin for the virtual run.
const BOOT_HOLD: Duration = Duration::from_secs(120);

fn run() -> R {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("");
    let id_arg = || -> Result<BveId, Box<dyn Error>> {
        let raw = args
            .iter()
            .skip(2)
            .find(|a| !a.starts_with("--"))
            .ok_or("missing <bve-id> argument")?;
        Ok(BveId::new(raw.clone())?)
    };

    match cmd {
        "plan" => plan(&id_arg()?),
        "env" => env(&id_arg()?),
        "check" => check(),
        "check-artifacts" => check_artifacts().map(|_| ()),
        "verify-clean" => verify_clean(&id_arg()?),
        "setup" => setup(&id_arg()?),
        "start-fixture" => start_fixture(&id_arg()?),
        "run-bve" => run_bve(&id_arg()?),
        "teardown" => teardown(&id_arg()?),
        other => {
            eprintln!(
                "usage: bve_winpe_pxe <plan|env|check|check-artifacts|verify-clean|setup|\
                 start-fixture|run-bve|teardown> <bve-id> \\\n\
                 \t[--fixture-root <dir>] [--manifest <file>] [--evidence-log <file>]\n\
                 (setup / start-fixture / teardown are privileged — run under sudo; the\n\
                 artifact paths are CLI flags so they survive sudo's env reset, and also\n\
                 fall back to $BAMEP_BVE_WINPE_FIXTURE_ROOT / $BAMEP_BVE_WINPE_MANIFEST)\n\
                 scripts/bve-winpe-pxe-proof.sh drives the whole cycle; these are for debugging.\n\
                 unknown subcommand: {other:?}"
            );
            std::process::exit(2);
        }
    }
}

/// The value after `--<name>` on the command line, if present.
fn flag_value(name: &str) -> Option<String> {
    let flag = format!("--{name}");
    let mut it = std::env::args();
    while let Some(a) = it.next() {
        if a == flag {
            return it.next();
        }
        if let Some(v) = a.strip_prefix(&format!("{flag}=")) {
            return Some(v.to_string());
        }
    }
    None
}

fn plan(id: &BveId) -> R {
    let p = BveNetworkPlan::for_bve(id);
    println!("bve id      : {id}");
    println!("mac         : {}", MacAddress::deterministic_for(id));
    println!("firmware    : Uefi (OVMF non-Secure-Boot)   nic: e1000   boot: NetworkFirst");
    println!("netns       : {}", p.netns());
    println!("tap / veth  : {} <-> {}", p.tap(), p.veth_host());
    println!("tftp root   : {}", winpe_tftp_root(&p).display());
    println!("http root   : {}", winpe_http_root(&p).display());
    println!("http url    : {}", winpe_http_base_url());
    println!("bootfile    : {WINPE_TFTP_BOOTFILE} (firmware EFI-x64, pre-iPXE)");
    Ok(())
}

fn env(id: &BveId) -> R {
    let p = BveNetworkPlan::for_bve(id);
    println!("BVE_ID={id}");
    println!("BVE_SHORT_HASH={}", p.short_hash());
    println!("BVE_MAC={}", MacAddress::deterministic_for(id));
    println!("BVE_NETNS={}", p.netns());
    println!("BVE_TAP={}", p.tap());
    println!("BVE_VETH_HOST={}", p.veth_host());
    println!("BVE_FIXTURE_DIR={}", fixture_run_dir(&p).display());
    println!("BVE_FIXTURE_PIDFILE={}", fixture_pid_file(&p).display());
    println!("BVE_HTTP_PIDFILE={}", http_pid_file(&p).display());
    println!("BVE_TFTP_ROOT={}", winpe_tftp_root(&p).display());
    println!("BVE_HTTP_ROOT={}", winpe_http_root(&p).display());
    println!("BVE_HTTP_URL={}", winpe_http_base_url());
    Ok(())
}

fn check() -> R {
    check_network_prerequisites()?;
    check_uefi_firmware()?;
    which(PYTHON3_BINARY)?;
    which("dnsmasq")?;
    println!("ok: isolated-network prereqs, OVMF firmware, python3 and dnsmasq are all present");
    println!("note: setup / start-fixture / teardown still need CAP_NET_ADMIN (run under sudo)");
    Ok(())
}

// ---- artifact manifest -----------------------------------------------------

struct Artifact {
    name: String,
    sha256: String,
}

fn manifest_path() -> PathBuf {
    flag_value("manifest")
        .or_else(|| std::env::var(MANIFEST_ENV).ok())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_MANIFEST))
}

fn parse_manifest() -> Result<Vec<Artifact>, Box<dyn Error>> {
    let path = manifest_path();
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("cannot read artifact manifest {}: {e}", path.display()))?;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (sha, name) = line
            .split_once("  ")
            .ok_or_else(|| format!("malformed manifest line: {line:?}"))?;
        out.push(Artifact {
            name: name.trim().to_string(),
            sha256: sha.trim().to_lowercase(),
        });
    }
    if out.is_empty() {
        return Err(format!("artifact manifest {} has no entries", path.display()).into());
    }
    Ok(out)
}

fn fixture_root() -> Result<PathBuf, Box<dyn Error>> {
    let root = flag_value("fixture-root")
        .or_else(|| std::env::var(FIXTURE_ROOT_ENV).ok())
        .ok_or_else(|| {
            format!(
                "no fixture root — pass `--fixture-root <dir>` or set {FIXTURE_ROOT_ENV}, \
                 pointing at the directory holding the retained snponly.efi / wimboot / BCD / \
                 boot.sdi / boot.wim (see scripts/winpe-pxe-fixture.provenance.md). This proof \
                 never downloads artifacts."
            )
        })?;
    let root = PathBuf::from(root);
    if !root.is_dir() {
        return Err(format!("fixture root {} is not a directory", root.display()).into());
    }
    Ok(root)
}

/// Validates every manifest entry against `$BAMEP_BVE_WINPE_FIXTURE_ROOT`.
/// Returns the resolved `(root, artifacts)` for staging.
fn check_artifacts() -> Result<(PathBuf, Vec<Artifact>), Box<dyn Error>> {
    let root = fixture_root()?;
    let artifacts = parse_manifest()?;
    for a in &artifacts {
        let path = root.join(&a.name);
        let meta = fs::metadata(&path).map_err(|_| {
            format!(
                "required artifact missing: {}\n  expected SHA-256 {}\n  \
                 stage it there — this proof never downloads a replacement",
                path.display(),
                a.sha256
            )
        })?;
        if meta.len() == 0 {
            return Err(format!("artifact {} is empty", path.display()).into());
        }
        let actual = sha256_file(&path)?;
        if actual != a.sha256 {
            return Err(format!(
                "artifact hash mismatch for {}\n  expected {}\n  actual   {}\n  \
                 this is not the qualified artifact (ADR-0021); do not substitute a different one",
                path.display(),
                a.sha256,
                actual
            )
            .into());
        }
        println!(
            "  ok  {:<12} {}  {}",
            a.name,
            &a.sha256[..16],
            path.display()
        );
    }
    println!(
        "artifacts: {} files verified in {}",
        artifacts.len(),
        root.display()
    );
    Ok((root, artifacts))
}

fn sha256_file(path: &Path) -> Result<String, Box<dyn Error>> {
    let out = Command::new("sha256sum")
        .arg(path)
        .output()
        .map_err(|e| format!("cannot run sha256sum: {e}"))?;
    if !out.status.success() {
        return Err(format!("sha256sum failed for {}", path.display()).into());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase())
}

// ---- lifecycle subcommands ----------------------------------------------

fn verify_clean(id: &BveId) -> R {
    let plan = BveNetworkPlan::for_bve(id);
    let residual = residual_resources(&plan);
    if residual.is_empty() {
        println!("clean: no bridge/TAP/veth/netns for {id}");
        println!(
            "iptables: confirm no rule with:  sudo iptables -S FORWARD | grep -E '{}|{}'",
            plan.tap(),
            plan.veth_host()
        );
        Ok(())
    } else {
        for r in &residual {
            eprintln!("  STILL PRESENT: {:?} {}", r.kind, r.name);
        }
        Err(format!(
            "{} residual resource(s) — run `teardown {id}`",
            residual.len()
        )
        .into())
    }
}

fn setup(id: &BveId) -> R {
    let owner = tap_owner()?;
    println!("preparing isolated network for {id} (TAP owner: {owner})");
    let prepared = prepare_network(BveNetworkPlan::for_bve(id), &owner)?;
    for r in prepared.plan().creation_order() {
        println!("  created {:?} {}", r.kind, r.name);
    }

    // Issue #71 needs DHCP + TFTP + HTTP + dynamic ports between the BVE TAP and
    // the fixture veth. On a br_netfilter-filtering host the narrow #70 UDP/67
    // rule is not enough — apply the broad, physdev-scoped, reversible rule
    // between exactly this BVE's two bridge ports (ADR-0024 amendment).
    match apply_bridged_forward_accommodation(prepared.plan()) {
        Ok(()) => println!(
            "netfilter: bridged FORWARD accommodation applied ({} <-> {}, both directions, \
             all protocols; removed by teardown)",
            prepared.plan().tap(),
            prepared.plan().veth_host()
        ),
        Err(bamep_ve::BveNetworkError::ToolUnavailable { .. }) => {
            println!("netfilter: iptables absent — skipping (host likely does not filter bridged traffic)")
        }
        Err(e) => {
            let _ = teardown_network(prepared);
            return Err(e.into());
        }
    }
    println!("next: `sudo … start-fixture {id}`, then `… run-bve {id}` (normal user)");
    Ok(())
}

fn http_pid_file(plan: &BveNetworkPlan) -> PathBuf {
    fixture_run_dir(plan).join("http.pid")
}

fn stage_artifacts(plan: &BveNetworkPlan) -> R {
    let (root, artifacts) = check_artifacts()?;
    let tftp = winpe_tftp_root(plan);
    let http = winpe_http_root(plan);
    fs::create_dir_all(&tftp)?;
    fs::create_dir_all(&http)?;
    for a in &artifacts {
        let src = root.join(&a.name);
        let dest_dir = if a.name == WINPE_TFTP_BOOTFILE {
            &tftp
        } else {
            &http
        };
        fs::copy(&src, dest_dir.join(&a.name))?;
    }
    fs::write(http.join("boot.ipxe"), winpe_boot_ipxe_script())?;
    println!(
        "staged: {} -> tftp; wimboot+BCD+boot.sdi+boot.wim -> http; boot.ipxe generated",
        WINPE_TFTP_BOOTFILE
    );
    Ok(())
}

fn start_fixture(id: &BveId) -> R {
    let plan = BveNetworkPlan::for_bve(id);
    if !Path::new("/run/netns").join(plan.netns()).exists() {
        return Err(format!(
            "netns {} does not exist — run `setup {id}` first",
            plan.netns()
        )
        .into());
    }
    fs::create_dir_all(fixture_run_dir(&plan))?;
    stage_artifacts(&plan)?;

    let (dp, da) = fixture_command(&plan, &winpe_fixture_dnsmasq_argv(&plan));
    let (hp, ha) = winpe_http_fixture_command(&plan);
    println!("dnsmasq: {dp} {}", da.join(" "));
    println!("http   : {hp} {}", ha.join(" "));
    println!(
        "watching DHCP/TFTP/HTTP on {} — Ctrl-C to stop.\n\
         SHUTDOWN ORDER: stop the fixture and wait for the netns to be process-free BEFORE teardown.\n",
        plan.veth_peer()
    );

    let mut dnsmasq = Command::new(&dp).args(&da).spawn()?;
    let mut http = Command::new(&hp).args(&ha).spawn()?;
    fs::write(http_pid_file(&plan), http.id().to_string())?;

    // Block until either child exits or we are signalled; then bring the other
    // down so the netns is left process-free for a fail-closed teardown.
    let status = loop {
        if let Some(s) = dnsmasq.try_wait()? {
            let _ = kill(&mut http);
            break format!("dnsmasq exited: {s}");
        }
        if let Some(s) = http.try_wait()? {
            let _ = kill(&mut dnsmasq);
            break format!("http exited: {s}");
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let _ = dnsmasq.wait();
    let _ = http.wait();
    let _ = fs::remove_file(http_pid_file(&plan));
    println!("\nfixture stopped ({status}). now safe to run: sudo … teardown {id}");
    Ok(())
}

fn kill(child: &mut Child) -> std::io::Result<()> {
    child.kill()
}

/// Line count of `path`, or 0 if it is `None` / missing. Used to bound each
/// boot's slice of the shared fixture log — this example never interprets the
/// log's DHCP content, only where one boot's lines start and end.
fn log_lines(path: Option<&Path>) -> usize {
    match path {
        Some(p) => fs::read(p).map(|b| bytecount_newlines(&b)).unwrap_or(0),
        None => 0,
    }
}

fn bytecount_newlines(b: &[u8]) -> usize {
    b.iter().filter(|&&c| c == b'\n').count()
}

/// The two 1-indexed inclusive `start:end` line ranges for boot #1 and boot #2,
/// given the fixture-log line count before boot #1, after boot #1, and after
/// boot #2. An empty boot (no new lines) yields `n+1:n`, which the harness
/// treats as "not proven".
fn boot_log_ranges(before1: usize, after1: usize, after2: usize) -> (String, String) {
    (
        format!("{}:{}", before1 + 1, after1.max(before1)),
        format!("{}:{}", after1 + 1, after2.max(after1)),
    )
}

fn run_bve(id: &BveId) -> R {
    check_network_prerequisites()?;
    check_uefi_firmware()?;
    let prerequisites = detect_host_prerequisites()?;
    let net = PreparedBveNetwork::from_prepared_plan(BveNetworkPlan::for_bve(id));

    let scratch = bve_run_dir(net.plan());
    let _guard = RemoveOnDrop(scratch.clone());
    fs::create_dir_all(&scratch)?;
    let runtime_root = RuntimeRoot::new(scratch.join("control"));
    let storage_root = BveStorageRoot::new(scratch.join("storage"))?;

    // A blank system overlay over a blank base: nothing bootable on disk, and
    // no source disk, no cdrom — the ONLY path to WinPE is the NIC (anti
    // false-positive, Issue #71).
    let base = ensure_system_base(&storage_root, &SystemBaseSpec::new(512 * 1024 * 1024)?)?;
    let storage = prepare_instance(&storage_root, id, None)?;
    let definition = net
        .attach(storage.define_bve(id.clone(), 2, 4096, Firmware::Uefi)?)
        .with_boot_mode(BootMode::NetworkFirst)
        .with_nic_model(NicModel::E1000);

    println!(
        "BVE {id}: UEFI + e1000 + NetworkFirst, MAC = {}",
        definition.mac()
    );
    println!("(this MAC must appear in the fixture's DHCPDISCOVER, then again as an MSFT 5.0 / MININT-* DORA)");

    let evidence_log = flag_value("evidence-log").map(PathBuf::from);

    let mut runtime =
        BveRuntime::create_with_isolated_network(&runtime_root, definition, storage.clone(), &net)?;

    // Both boots reuse the SAME runtime / definition / storage / OVMF VARS —
    // only stop/start between them, no hidden recreate (Issue #71 repeatability).
    let one_boot = |runtime: &mut BveRuntime, boot: u8| -> R {
        runtime.start(&prerequisites)?;
        assert_eq!(runtime.observe()?, LifecycleState::Running);
        println!(
            "boot #{boot}: Running — holding {}s for PXE -> iPXE -> wimboot -> WinPE ready",
            BOOT_HOLD.as_secs()
        );
        std::thread::sleep(BOOT_HOLD);
        runtime.stop()?;
        assert_eq!(runtime.observe()?, LifecycleState::Stopped);
        // Let the fixture flush this boot's last log lines before we mark the
        // boundary.
        std::thread::sleep(Duration::from_secs(3));
        println!("boot #{boot}: stopped");
        Ok(())
    };

    let before1 = log_lines(evidence_log.as_deref());
    one_boot(&mut runtime, 1)?;
    let after1 = log_lines(evidence_log.as_deref());
    one_boot(&mut runtime, 2)?;
    let after2 = log_lines(evidence_log.as_deref());

    if evidence_log.is_some() {
        let (r1, r2) = boot_log_ranges(before1, after1, after2);
        // Machine-readable boundaries for the harness; not DHCP interpretation.
        println!("BVE_BOOT1_LOG_RANGE={r1}");
        println!("BVE_BOOT2_LOG_RANGE={r2}");
    }

    runtime.destroy()?;
    destroy_instance_storage(storage)?;
    let _ = base;
    println!("both boots done; BVE disposed. Network resources are left for `teardown {id}`.");
    Ok(())
}

fn teardown(id: &BveId) -> R {
    let plan = BveNetworkPlan::for_bve(id);
    let prepared = PreparedBveNetwork::from_prepared_plan(plan.clone());

    remove_bridged_forward_accommodation(&plan)?;
    println!("netfilter: bridged FORWARD accommodation removed (idempotent)");

    for r in prepared.teardown_order() {
        println!("  removing {:?} {}", r.kind, r.name);
    }
    match teardown_network(prepared) {
        Ok(()) => {}
        Err(bamep_ve::BveNetworkError::FixtureStillRunning { netns, pids }) => {
            return Err(format!(
                "fixture still running in netns {netns} (pids {pids:?}). Stop `start-fixture`, \
                 wait for the netns to be process-free, then retry teardown."
            )
            .into())
        }
        Err(e) => return Err(e.into()),
    }
    let _ = fs::remove_dir_all(fixture_run_dir(&plan));
    println!("done. confirm with: bve_winpe_pxe verify-clean {id}");
    Ok(())
}

// ---- helpers -------------------------------------------------------------

fn which(bin: &str) -> Result<(), Box<dyn Error>> {
    Command::new(bin)
        .arg("--version")
        .output()
        .map(|_| ())
        .map_err(|e| format!("required tool {bin:?} is not runnable: {e}").into())
}

fn tap_owner() -> Result<String, Box<dyn Error>> {
    let owner = std::env::var("SUDO_USER")
        .or_else(|_| std::env::var("USER"))
        .map_err(|_| "cannot determine the invoking user (set SUDO_USER or USER)")?;
    if owner.is_empty()
        || !owner
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!("refusing an unusual user name {owner:?}").into());
    }
    Ok(owner)
}

struct RemoveOnDrop(PathBuf);
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytecount_newlines_counts_lines_not_bytes() {
        assert_eq!(bytecount_newlines(b""), 0);
        assert_eq!(bytecount_newlines(b"one line no newline"), 0);
        assert_eq!(bytecount_newlines(b"a\nb\nc\n"), 3);
        assert_eq!(bytecount_newlines(b"a\nb\nc"), 2);
    }

    #[test]
    fn log_lines_is_zero_without_a_path_or_file() {
        assert_eq!(log_lines(None), 0);
        assert_eq!(
            log_lines(Some(Path::new("/bamep/definitely/not/here.log"))),
            0
        );
    }

    #[test]
    fn boot_log_ranges_are_1_indexed_inclusive_and_contiguous() {
        // fixture log grew 100 -> 257 during boot #1, 257 -> 403 during boot #2
        let (r1, r2) = boot_log_ranges(100, 257, 403);
        assert_eq!(r1, "101:257");
        assert_eq!(r2, "258:403");
    }

    #[test]
    fn boot_log_ranges_mark_an_empty_boot_as_n_plus_1_to_n() {
        // boot #2 produced no new lines (it hung) -> range end < start
        let (r1, r2) = boot_log_ranges(10, 40, 40);
        assert_eq!(r1, "11:40");
        assert_eq!(r2, "41:40", "an empty boot #2 range is not satisfiable");
    }

    #[test]
    fn boot_log_ranges_never_go_backwards_even_on_a_shrinking_log() {
        // defensive: a rotated/truncated log must not produce a wild range
        let (r1, r2) = boot_log_ranges(50, 30, 20);
        assert_eq!(r1, "51:50");
        assert_eq!(r2, "31:30");
    }
}
