//! Issue #73 host-proof building blocks: PXE-boot the existing **BARE**
//! `bzImage` + `rootfs.cpio.gz` (Issue #72 / ADR-0026) in one BVE over
//! **virtual UEFI** (OVMF non-Secure-Boot), on the isolated #70 provisioning
//! network, reusing the exact Issue #71 iPXE + `snponly.efi` 2.0.0 UEFI/PXE
//! bootstrap unchanged — and observe BARE reach `BARE READY` /
//! `BARE NET_READY`, headless, with no ISO / disk / alternate bootable device
//! that could masquerade as network-boot success.
//!
//! The Issue #73 spike (`docs/reference/bve-bare-uefi-pxe-host-proof.md`)
//! proved the current BARE kernel (`CONFIG_EFI=y` / `CONFIG_EFI_STUB=y`
//! already built in) crosses this exact chain unchanged — no kernel/Buildroot
//! change was needed, so none was made.
//!
//! **The primary way to run the proof is `scripts/bve-bare-pxe-proof.sh`**,
//! which orchestrates the whole cycle (two accepted boots, stage parsing
//! against TWO separate evidence authorities, fail-closed shutdown ordering,
//! cleanup). These subcommands stay for manual debugging:
//!
//! - non-privileged: `plan`, `env`, `check`, `check-artifacts`, `verify-clean`;
//! - privileged (run under `sudo`): `setup` (isolated net + broad bridged
//!   FORWARD accommodation, unchanged from #71), `start-fixture` (stage
//!   BARE's artifacts + `dnsmasq` DHCP/TFTP + `python3` HTTP, foreground),
//!   `teardown`;
//! - normal user: `run-bve` (UEFI + VirtioNetPci + NetworkFirst, two boots,
//!   serial capture on; with `--evidence-log <fixture log>` it emits the
//!   per-boot fixture-log line ranges AND the per-boot serial-log line
//!   ranges the harness uses to prove BARE readiness independently for each
//!   boot, against its own authority. `--serial-out <file>` persists the
//!   serial capture there BEFORE `runtime.destroy()` removes the per-instance
//!   `serial.log` — `destroy()`'s cleanup is correct and unchanged (Issue
//!   #72's lifecycle); without `--serial-out` the emitted `BVE_SERIAL_LOG`
//!   path is the internal one and does NOT survive disposal, exactly like
//!   #72's `bve_bare.rs`).
//!
//! BARE artifacts (`bzImage` / `rootfs.cpio.gz`) are NOT in the repo; build
//! them with `scripts/build-bare.sh` — this proof never rebuilds BARE.
//! `snponly.efi` is NOT in the repo either — it is the exact Issue #71
//! artifact (`scripts/winpe-pxe-fixture.sha256` is the one authoritative
//! source for its hash; this file re-reads that manifest rather than
//! repinning the hash). Point `--snponly <path>` (or
//! `BAMEP_BVE_BARE_PXE_SNPONLY`) at it, or simply reuse an already-staged
//! `BAMEP_BVE_WINPE_FIXTURE_ROOT` from Issue #71 — it holds the same file.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

use bamep_ve::{
    apply_bridged_forward_accommodation, bare_boot_ipxe_script, bve_run_dir,
    check_network_prerequisites, check_uefi_firmware, destroy_instance_storage,
    detect_host_prerequisites, ensure_system_base, fixture_command, fixture_pid_file,
    fixture_run_dir, prepare_instance, prepare_network, remove_bridged_forward_accommodation,
    residual_resources, teardown_network, winpe_fixture_dnsmasq_argv, winpe_http_fixture_command,
    winpe_http_root, winpe_tftp_root, BootMode, BveId, BveNetworkPlan, BveRuntime, BveStorageRoot,
    Firmware, LifecycleState, MacAddress, NicModel, PreparedBveNetwork, RuntimeRoot,
    SystemBaseSpec, BARE_PXE_INITRD_NAME, BARE_PXE_KERNEL_NAME, PYTHON3_BINARY,
    WINPE_TFTP_BOOTFILE,
};

type R = Result<(), Box<dyn Error>>;

/// Print the error with `Display` (readable multi-line messages) rather than
/// the `Result`-returning-`main` default of `Debug`.
fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

const SNPONLY_FLAG: &str = "snponly";
const SNPONLY_ENV: &str = "BAMEP_BVE_BARE_PXE_SNPONLY";
/// Issue #71's already-staged fixture root — reused verbatim: it holds the
/// exact same qualified `snponly.efi`. Never a second, BARE-specific env var
/// for the same artifact unless `--snponly` / [`SNPONLY_ENV`] is unset.
const WINPE_FIXTURE_ROOT_ENV: &str = "BAMEP_BVE_WINPE_FIXTURE_ROOT";
const MANIFEST_FLAG: &str = "manifest";
/// The ONE authoritative source for `snponly.efi`'s identity (Issue #71).
/// This file is read, never re-pinned — see the module doc.
const DEFAULT_MANIFEST: &str = "scripts/winpe-pxe-fixture.sha256";
const SNPONLY_NAME: &str = "snponly.efi";

/// How long each boot is held so UEFI -> PXE -> TFTP snponly.efi -> iPXE ->
/// HTTP boot.ipxe -> HTTP bzImage/rootfs.cpio.gz -> Linux EFI stub -> BusyBox
/// init can complete and emit both BARE markers. The Issue #73 spike observed
/// the full chain complete well inside this on a QEMU/KVM host; UEFI POST +
/// PXE/iPXE negotiation adds real time over #72's direct-boot 25s, but the
/// transferred payload (~16 MB total) is trivial next to #71's 340 MB.
const DEFAULT_BOOT_HOLD: Duration = Duration::from_secs(45);

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
                "usage: bve_bare_pxe <plan|env|check|check-artifacts|verify-clean|setup|\
                 start-fixture|run-bve|teardown> <bve-id> \\\n\
                 \t[--kernel <bzImage> --initrd <rootfs.cpio.gz>]   (check-artifacts / start-fixture)\n\
                 \t[--snponly <path>] [--manifest <file>]           (check-artifacts / start-fixture)\n\
                 \t[--evidence-log <file>] [--serial-out <file>] [--boot-hold <secs>]  (run-bve)\n\
                 (setup / start-fixture / teardown are privileged — run under sudo)\n\
                 scripts/bve-bare-pxe-proof.sh drives the whole cycle; these are for debugging.\n\
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
    println!(
        "firmware    : Uefi (OVMF non-Secure-Boot)   nic: virtio-net-pci   boot: NetworkFirst"
    );
    println!("netns       : {}", p.netns());
    println!("tap / veth  : {} <-> {}", p.tap(), p.veth_host());
    println!("tftp root   : {}", winpe_tftp_root(&p).display());
    println!("http root   : {}", winpe_http_root(&p).display());
    println!("bootfile    : {WINPE_TFTP_BOOTFILE} (Issue #71's snponly.efi, reused unchanged)");
    println!(
        "payload     : {BARE_PXE_KERNEL_NAME} + {BARE_PXE_INITRD_NAME} (Issue #72, unchanged)"
    );
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
    println!("BVE_VETH_PEER={}", p.veth_peer());
    println!("BVE_FIXTURE_DIR={}", fixture_run_dir(&p).display());
    println!("BVE_FIXTURE_PIDFILE={}", fixture_pid_file(&p).display());
    println!("BVE_HTTP_PIDFILE={}", http_pid_file(&p).display());
    println!("BVE_TFTP_ROOT={}", winpe_tftp_root(&p).display());
    println!("BVE_HTTP_ROOT={}", winpe_http_root(&p).display());
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

// ---- artifacts: BARE kernel/initrd + the reused #71 snponly.efi ----------

struct BareArtifacts {
    kernel: PathBuf,
    initrd: PathBuf,
    snponly: PathBuf,
}

fn require_artifact(kind: &str, flag: &str) -> Result<PathBuf, Box<dyn Error>> {
    let raw = flag_value(flag).ok_or_else(|| {
        format!("missing --{flag} <path> ({kind}); build it with scripts/build-bare.sh")
    })?;
    let path = PathBuf::from(raw);
    let meta = fs::metadata(&path).map_err(|_| {
        format!(
            "{kind} not found: {} (build it with scripts/build-bare.sh; this proof never \
             rebuilds BARE)",
            path.display()
        )
    })?;
    if !meta.is_file() || meta.len() == 0 {
        return Err(format!("{kind} is not a usable file: {}", path.display()).into());
    }
    Ok(path)
}

fn manifest_path() -> PathBuf {
    flag_value(MANIFEST_FLAG)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_MANIFEST))
}

/// The SHA-256 Issue #71 already pins for `snponly.efi`
/// (`scripts/winpe-pxe-fixture.sha256`) — read directly from that manifest so
/// the hash keeps exactly one authoritative source. Issue #73 never repins
/// it, and never writes a second manifest for the same fact.
fn expected_snponly_sha256() -> Result<String, Box<dyn Error>> {
    let path = manifest_path();
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {} (Issue #71's manifest): {e}", path.display()))?;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((sha, name)) = line.split_once("  ") {
            if name.trim() == SNPONLY_NAME {
                return Ok(sha.trim().to_lowercase());
            }
        }
    }
    Err(format!("{} has no {SNPONLY_NAME} entry", path.display()).into())
}

fn snponly_path() -> Result<PathBuf, Box<dyn Error>> {
    if let Some(p) = flag_value(SNPONLY_FLAG).or_else(|| std::env::var(SNPONLY_ENV).ok()) {
        return Ok(PathBuf::from(p));
    }
    if let Ok(root) = std::env::var(WINPE_FIXTURE_ROOT_ENV) {
        return Ok(PathBuf::from(root).join(SNPONLY_NAME));
    }
    Err(format!(
        "no snponly.efi — pass --{SNPONLY_FLAG} <path>, set {SNPONLY_ENV}, or reuse an \
         already-staged {WINPE_FIXTURE_ROOT_ENV} from Issue #71 (same qualified artifact; this \
         proof never downloads a replacement, see scripts/winpe-pxe-fixture.provenance.md)"
    )
    .into())
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

fn check_artifacts() -> Result<BareArtifacts, Box<dyn Error>> {
    let kernel = require_artifact("BARE kernel (bzImage)", "kernel")?;
    let initrd = require_artifact("BARE initramfs (rootfs.cpio.gz)", "initrd")?;
    println!("  ok  {:<16} {}", BARE_PXE_KERNEL_NAME, kernel.display());
    println!("  ok  {:<16} {}", BARE_PXE_INITRD_NAME, initrd.display());

    let snponly = snponly_path()?;
    let meta = fs::metadata(&snponly).map_err(|_| {
        format!(
            "required artifact missing: {} — stage it there (the exact Issue #71 artifact); \
             this proof never downloads a replacement",
            snponly.display()
        )
    })?;
    if meta.len() == 0 {
        return Err(format!("artifact {} is empty", snponly.display()).into());
    }
    let expected = expected_snponly_sha256()?;
    let actual = sha256_file(&snponly)?;
    if actual != expected {
        return Err(format!(
            "snponly.efi hash mismatch\n  expected (scripts/winpe-pxe-fixture.sha256): {expected}\n  \
             actual                                     : {actual}\n  \
             this is not the Issue #71 qualified artifact; do not substitute a different one",
        )
        .into());
    }
    println!(
        "  ok  {:<16} {}  {}",
        SNPONLY_NAME,
        &actual[..16],
        snponly.display()
    );

    Ok(BareArtifacts {
        kernel,
        initrd,
        snponly,
    })
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

    // Same accommodation Issue #71 needed (DHCP + TFTP + HTTP across the
    // bridge on a br_netfilter-filtering host) — unchanged, ADR-0024 amendment.
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

fn stage_artifacts(plan: &BveNetworkPlan, artifacts: &BareArtifacts) -> R {
    let tftp = winpe_tftp_root(plan);
    let http = winpe_http_root(plan);
    fs::create_dir_all(&tftp)?;
    fs::create_dir_all(&http)?;
    fs::copy(&artifacts.snponly, tftp.join(WINPE_TFTP_BOOTFILE))?;
    fs::copy(&artifacts.kernel, http.join(BARE_PXE_KERNEL_NAME))?;
    fs::copy(&artifacts.initrd, http.join(BARE_PXE_INITRD_NAME))?;
    fs::write(http.join("boot.ipxe"), bare_boot_ipxe_script())?;
    println!(
        "staged: {WINPE_TFTP_BOOTFILE} -> tftp; {BARE_PXE_KERNEL_NAME}+{BARE_PXE_INITRD_NAME} \
         -> http; boot.ipxe generated"
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
    let artifacts = check_artifacts()?;
    fs::create_dir_all(fixture_run_dir(&plan))?;
    stage_artifacts(&plan, &artifacts)?;

    // Same dnsmasq argv shape as Issue #71 (crates/ve/src/network.rs) — two
    // mutually exclusive stages (firmware EFI-x64 non-iPXE -> TFTP
    // snponly.efi; iPXE user-class -> HTTP boot.ipxe), unchanged. Both
    // children inherit THIS process's stdout/stderr, so the orchestrating
    // shell script's single redirection captures dnsmasq's DHCP/TFTP log AND
    // python's HTTP access log in one fixture evidence authority — the same
    // mechanism #71 already relies on for its wimboot/BCD/boot.sdi/boot.wim
    // HTTP evidence.
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

/// Persists the runtime's internal serial capture before `runtime.destroy()`
/// removes it. `destroy()` deliberately discards the per-instance
/// `<instance-dir>/serial.log` as transitory control state (Issue #72's
/// lifecycle — correct, unchanged, not this proof's concern); keeping proof
/// evidence readable AFTER disposal is the harness's job, exactly like #72's
/// `bve_bare.rs --serial-out`. Returns the path the evidence now lives at:
/// `dest` (copied there) when given, or `source` unchanged otherwise — the
/// caller MUST pass `dest` for the path to survive `destroy()`.
fn persist_serial_log(source: &Path, dest: Option<&Path>) -> Result<PathBuf, Box<dyn Error>> {
    match dest {
        Some(out) => {
            fs::copy(source, out)?;
            Ok(out.to_path_buf())
        }
        None => Ok(source.to_path_buf()),
    }
}

/// Line count of `path`, or 0 if it is `None` / missing.
fn log_lines(path: Option<&Path>) -> usize {
    match path {
        Some(p) => fs::read(p).map(|b| bytecount_newlines(&b)).unwrap_or(0),
        None => 0,
    }
}

fn bytecount_newlines(b: &[u8]) -> usize {
    b.iter().filter(|&&c| c == b'\n').count()
}

/// The two 1-indexed inclusive `start:end` line ranges for boot #1 and
/// boot #2, given a log's line count before boot #1, after boot #1, and
/// after boot #2. An empty boot (no new lines) yields `n+1:n`, which the
/// harness treats as "not proven" (same convention as #71/#72).
fn boot_log_ranges(before1: usize, after1: usize, after2: usize) -> (String, String) {
    (
        format!("{}:{}", before1 + 1, after1.max(before1)),
        format!("{}:{}", after1 + 1, after2.max(after1)),
    )
}

fn run_bve(id: &BveId) -> R {
    // No --kernel/--initrd here: by the time run-bve executes, `start-fixture`
    // has already staged BARE's artifacts into the fixture's HTTP root (same
    // division of labour as #71 — run-bve only boots the BVE and lets iPXE
    // fetch the payload; it never touches the kernel/initrd files itself).
    let boot_hold = flag_value("boot-hold")
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_BOOT_HOLD);

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
    // no source disk, no cdrom — the ONLY path to BARE is the NIC (anti
    // false-positive, same guard #71 used for WinPE).
    let base = ensure_system_base(&storage_root, &SystemBaseSpec::new(512 * 1024 * 1024)?)?;
    let storage = prepare_instance(&storage_root, id, None)?;
    let definition = net
        .attach(storage.define_bve(id.clone(), 2, 1024, Firmware::Uefi)?)
        .with_boot_mode(BootMode::NetworkFirst)
        .with_nic_model(NicModel::VirtioNetPci);

    println!(
        "BVE {id}: UEFI + virtio-net-pci + NetworkFirst, MAC = {}",
        definition.mac()
    );
    println!("(this MAC must appear in the fixture's DHCPDISCOVER, then again requesting {BARE_PXE_KERNEL_NAME}/{BARE_PXE_INITRD_NAME})");

    let evidence_log = flag_value("evidence-log").map(PathBuf::from);
    let serial_out = flag_value("serial-out").map(PathBuf::from);

    let mut runtime =
        BveRuntime::create_with_isolated_network(&runtime_root, definition, storage.clone(), &net)?
            .with_serial_capture();
    let serial_log = runtime
        .serial_log()
        .expect("serial capture was enabled")
        .to_path_buf();

    // Both boots reuse the SAME runtime / definition / storage / OVMF VARS —
    // only stop/start between them, no hidden recreate (Issue #73
    // repeatability, same pattern as #71/#72).
    let one_boot = |runtime: &mut BveRuntime, boot: u8| -> R {
        runtime.start(&prerequisites)?;
        assert_eq!(runtime.observe()?, LifecycleState::Running);
        println!(
            "boot #{boot}: Running — holding {}s for UEFI PXE -> iPXE -> BARE -> BARE_READY -> BARE_NET_READY",
            boot_hold.as_secs()
        );
        std::thread::sleep(boot_hold);
        runtime.stop()?;
        assert_eq!(runtime.observe()?, LifecycleState::Stopped);
        // Let the fixture flush this boot's last log lines before we mark the
        // boundary (same margin #71 uses).
        std::thread::sleep(Duration::from_secs(3));
        println!("boot #{boot}: stopped");
        Ok(())
    };

    let fixture_before1 = log_lines(evidence_log.as_deref());
    let serial_before1 = log_lines(Some(&serial_log));
    one_boot(&mut runtime, 1)?;
    let fixture_after1 = log_lines(evidence_log.as_deref());
    let serial_after1 = log_lines(Some(&serial_log));
    one_boot(&mut runtime, 2)?;
    let fixture_after2 = log_lines(evidence_log.as_deref());
    let serial_after2 = log_lines(Some(&serial_log));

    if evidence_log.is_some() {
        let (r1, r2) = boot_log_ranges(fixture_before1, fixture_after1, fixture_after2);
        println!("BVE_BOOT1_FIXTURE_RANGE={r1}");
        println!("BVE_BOOT2_FIXTURE_RANGE={r2}");
    }
    let (sr1, sr2) = boot_log_ranges(serial_before1, serial_after1, serial_after2);
    println!("BVE_BOOT1_SERIAL_RANGE={sr1}");
    println!("BVE_BOOT2_SERIAL_RANGE={sr2}");

    // Persist the serial capture BEFORE destroy() removes the instance
    // directory it lives in (same ordering as #72's bve_bare.rs).
    let persisted_serial_log = persist_serial_log(&serial_log, serial_out.as_deref())?;
    println!("BVE_SERIAL_LOG={}", persisted_serial_log.display());

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
    println!("done. confirm with: bve_bare_pxe verify-clean {id}");
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
        let (r1, r2) = boot_log_ranges(100, 257, 403);
        assert_eq!(r1, "101:257");
        assert_eq!(r2, "258:403");
    }

    #[test]
    fn boot_log_ranges_mark_an_empty_boot_as_n_plus_1_to_n() {
        let (r1, r2) = boot_log_ranges(10, 40, 40);
        assert_eq!(r1, "11:40");
        assert_eq!(r2, "41:40", "an empty boot #2 range is not satisfiable");
    }

    #[test]
    fn persist_serial_log_copies_to_an_external_path_that_survives_the_source_being_removed() {
        // Reproduces the exact owner-run failure: BveRuntime::destroy() deliberately
        // removes the per-instance serial.log as transitory control state (Issue
        // #72's lifecycle — unchanged, correct, not touched here). Proof evidence
        // must be persisted OUTSIDE the instance directory before destroy() runs,
        // exactly like #72's bve_bare.rs --serial-out.
        let dir = std::env::temp_dir().join(format!(
            "bamep-bve-bare-pxe-persist-test-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("serial.log"); // stands in for <instance-dir>/serial.log
        fs::write(&source, "boot #1 evidence\nboot #2 evidence\n").unwrap();
        let dest = dir.join("external-serial.log"); // stands in for --serial-out

        let persisted = persist_serial_log(&source, Some(&dest)).unwrap();
        assert_eq!(persisted, dest);

        // Simulate runtime.destroy() removing the internal instance file.
        fs::remove_file(&source).unwrap();

        assert!(
            persisted.exists(),
            "the external copy must survive the internal serial.log being removed by destroy()"
        );
        assert_eq!(
            fs::read_to_string(&persisted).unwrap(),
            "boot #1 evidence\nboot #2 evidence\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_serial_log_without_an_external_path_returns_the_internal_path_unchanged() {
        // No --serial-out: matches #72's bve_bare.rs — the caller gets the
        // internal path back for manual debugging; it is NOT guaranteed to
        // survive destroy() (the proof script must always pass --serial-out).
        let source = Path::new("/some/instance/dir/serial.log");
        assert_eq!(persist_serial_log(source, None).unwrap(), source);
    }

    #[test]
    fn boot_log_ranges_never_go_backwards_even_on_a_shrinking_log() {
        let (r1, r2) = boot_log_ranges(50, 30, 20);
        assert_eq!(r1, "51:50");
        assert_eq!(r2, "31:30");
    }

    #[test]
    fn expected_snponly_sha256_reads_issue71s_manifest_not_a_new_pin() {
        // scripts/winpe-pxe-fixture.sha256 is the one authoritative source;
        // this just asserts the parser finds the snponly.efi line in the
        // REAL repo-owned manifest (run from the crate dir by `cargo test`,
        // so walk up to the repo root the same way the proof scripts `cd`
        // there).
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("crates/ve/../.. is the repo root");
        let manifest = repo_root.join(DEFAULT_MANIFEST);
        let text = fs::read_to_string(&manifest).expect("scripts/winpe-pxe-fixture.sha256 exists");
        assert!(
            text.lines().any(|l| l.trim_end().ends_with(SNPONLY_NAME)),
            "manifest must still carry a snponly.efi entry"
        );
    }
}
