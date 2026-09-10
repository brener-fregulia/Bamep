//! Issue #70 host-proof building blocks: give one BVE an isolated, PXE-capable
//! provisioning network and prove DHCP/PXE discovery crosses the virtual NIC
//! boundary to a controlled peer — **without** exposing a test DHCP server to
//! the normal LAN and **without** this crate ever calling `sudo`.
//!
//! **The primary way to run the proof is `scripts/bve-network-proof.sh`**,
//! which orchestrates the whole cycle (short output, `--verbose` for logs) and
//! tracks the *real* `dnsmasq` via [`bamep_ve::fixture_pid_file`] +
//! `ip netns pids`, not the `sudo` wrapper PID. These subcommands stay for
//! manual debugging:
//!
//! - non-privileged: `plan`, `env` (machine-readable), `check`, `check-l2`,
//!   `verify-clean`;
//! - privileged (run under `sudo -E`): `setup` (+ opt-out
//!   `--no-netfilter-accommodation`), `start-fixture` (foreground, live log),
//!   `teardown` (fail-closed while the fixture still runs in the netns);
//! - normal user: `run-bve` (opens the user-owned TAP; never run as root).
//!
//! Scratch is split by ownership domain so a privileged step never creates a
//! directory a non-privileged step must write into:
//! [`bamep_ve::fixture_run_dir`] (root-owned, `leases`/`pid`) vs
//! [`bamep_ve::bve_run_dir`] (user-owned, process-specific control/storage/QMP).

use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use bamep_ve::{
    apply_dhcp_forward_accommodation, assert_l2_isolation, bve_run_dir,
    check_network_prerequisites, destroy_instance_storage, detect_host_prerequisites,
    ensure_system_base, fixture_command, fixture_dnsmasq_argv, fixture_lease_file,
    fixture_pid_file, fixture_run_dir, prepare_instance, prepare_network,
    remove_dhcp_forward_accommodation, residual_resources, teardown_network, BootMode, BveId,
    BveNetworkError, BveNetworkPlan, BveRuntime, BveStorageRoot, Firmware, LifecycleState,
    MacAddress, PreparedBveNetwork, RuntimeRoot, SourceDiskSpec, SystemBaseSpec,
};

type R = Result<(), Box<dyn Error>>;

/// The `iptables` FORWARD accommodation is opt-out with this flag.
const NO_NF_FLAG: &str = "--no-netfilter-accommodation";

fn main() -> R {
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
    let netfilter_accommodation = !args.iter().any(|a| a == NO_NF_FLAG);

    match cmd {
        "plan" => plan(&id_arg()?),
        "env" => env(&id_arg()?),
        "check" => check(),
        "check-l2" => check_l2(&id_arg()?),
        "verify-clean" => verify_clean(&id_arg()?),
        "setup" => setup(&id_arg()?, netfilter_accommodation),
        "start-fixture" => start_fixture(&id_arg()?),
        "run-bve" => run_bve(&id_arg()?),
        "teardown" => teardown(&id_arg()?),
        other => {
            eprintln!(
                "usage: bve_isolated_net <plan|env|check|check-l2|verify-clean|setup|start-fixture|run-bve|teardown> <bve-id> [{NO_NF_FLAG}]\n\
                 (setup / start-fixture / teardown are privileged — run under `sudo -E`)\n\
                 the scripts/bve-network-proof.sh harness drives the whole cycle;\n\
                 these subcommands are for manual debugging.\n\
                 unknown subcommand: {other:?}"
            );
            std::process::exit(2);
        }
    }
}

/// Machine-readable derived state for a harness (`eval "$(… env <id>)"`).
fn env(id: &BveId) -> R {
    let p = BveNetworkPlan::for_bve(id);
    println!("BVE_ID={id}");
    println!("BVE_SHORT_HASH={}", p.short_hash());
    println!("BVE_BRIDGE={}", p.bridge());
    println!("BVE_TAP={}", p.tap());
    println!("BVE_VETH_HOST={}", p.veth_host());
    println!("BVE_VETH_PEER={}", p.veth_peer());
    println!("BVE_NETNS={}", p.netns());
    println!("BVE_MAC={}", MacAddress::deterministic_for(id));
    println!("BVE_FIXTURE_DIR={}", fixture_run_dir(&p).display());
    println!("BVE_FIXTURE_PIDFILE={}", fixture_pid_file(&p).display());
    println!("BVE_FIXTURE_LEASEFILE={}", fixture_lease_file(&p).display());
    Ok(())
}

fn plan(id: &BveId) -> R {
    let p = BveNetworkPlan::for_bve(id);
    println!("bve id     : {id}");
    println!("short hash : {}", p.short_hash());
    println!("bridge     : {}", p.bridge());
    println!("tap        : {}", p.tap());
    println!("veth host  : {}", p.veth_host());
    println!("veth peer  : {}", p.veth_peer());
    println!("netns      : {}", p.netns());
    println!(
        "fixture    : {} (RFC 5737 TEST-NET-1, no uplink, no route, no NAT)",
        bamep_ve::FIXTURE_PEER_CIDR
    );
    Ok(())
}

fn check() -> R {
    check_network_prerequisites()?;
    println!(
        "ok: `ip` runnable and {} is a usable character device",
        bamep_ve::TUN_DEVICE
    );
    println!("note: creating the bridge/TAP/veth/netns still needs CAP_NET_ADMIN (run `setup` under sudo)");
    Ok(())
}

fn check_l2(id: &BveId) -> R {
    let members = assert_l2_isolation(&BveNetworkPlan::for_bve(id))?;
    println!("L2 isolation ok: private bridge carries only {members:?}");
    println!("(no eth*/wl*/docker*/bond* — no physical uplink)");
    Ok(())
}

fn verify_clean(id: &BveId) -> R {
    let plan = BveNetworkPlan::for_bve(id);
    let residual = residual_resources(&plan);
    if residual.is_empty() {
        println!("clean: no bridge/TAP/veth/netns for {id} exists");
        println!(
            "iptables: confirm no accommodation rule with:  sudo iptables -S FORWARD | grep -E '{}|{}'",
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

fn setup(id: &BveId, netfilter_accommodation: bool) -> R {
    let owner = tap_owner()?;
    println!("preparing isolated network for {id} (TAP owner: {owner})");
    let prepared = prepare_network(BveNetworkPlan::for_bve(id), &owner)?;
    println!("created (in order):");
    for r in prepared.plan().creation_order() {
        println!("  {:?} {}", r.kind, r.name);
    }
    let members = assert_l2_isolation(prepared.plan())?;
    println!("L2 isolation ok: bridge ports = {members:?}");

    if netfilter_accommodation {
        match apply_dhcp_forward_accommodation(prepared.plan()) {
            Ok(()) => {
                println!("netfilter: applied 2 scoped, reversible FORWARD ACCEPT rules for UDP/67");
                println!(
                    "           (physdev {} <-> {}; removed by teardown)",
                    prepared.plan().tap(),
                    prepared.plan().veth_host()
                );
            }
            Err(BveNetworkError::ToolUnavailable { .. }) => {
                println!("netfilter: iptables absent — skipping accommodation (host likely does not filter bridged DHCP)");
            }
            Err(e) => {
                // Roll the network back so we don't leave half a proof set up.
                let _ = teardown_network(prepared);
                return Err(e.into());
            }
        }
    } else {
        println!("netfilter: {NO_NF_FLAG} given — no FORWARD rule applied");
    }

    println!(
        "next: `sudo -E … start-fixture {id}` (terminal A), then `… run-bve {id}` (normal user)"
    );
    Ok(())
}

fn start_fixture(id: &BveId) -> R {
    let plan = BveNetworkPlan::for_bve(id);
    if !PathBuf::from("/run/netns").join(plan.netns()).exists() {
        return Err(format!(
            "netns {} does not exist — run `setup {id}` first",
            plan.netns()
        )
        .into());
    }
    fs::create_dir_all(fixture_run_dir(&plan))?;
    let argv = fixture_dnsmasq_argv(&plan);
    let (program, args) = fixture_command(&plan, &argv);
    println!("fixture: {program} {}", args.join(" "));
    println!(
        "watching DHCP/PXE on {} — Ctrl-C to stop.",
        plan.veth_peer()
    );
    println!(
        "SHUTDOWN ORDER: Ctrl-C here and wait for 'fixture exited' BEFORE running teardown.\n"
    );

    // `.status()` waits for dnsmasq to actually exit. A terminal Ctrl-C reaches
    // the child too, so on return dnsmasq is gone and the netns interface is
    // safe to remove.
    let status = Command::new(program).args(args).status()?;
    println!("\nfixture exited: {status}");
    println!("now safe to run: sudo -E … teardown {id}");
    Ok(())
}

fn run_bve(id: &BveId) -> R {
    check_network_prerequisites()?;
    let prerequisites = detect_host_prerequisites()?;
    let net = PreparedBveNetwork::from_prepared_plan(BveNetworkPlan::for_bve(id));

    let scratch = bve_run_dir(net.plan());
    let _guard = RemoveOnDrop(scratch.clone());
    fs::create_dir_all(&scratch)?;
    let runtime_root = RuntimeRoot::new(scratch.join("control"));
    let storage_root = BveStorageRoot::new(scratch.join("storage"))?;

    let base = ensure_system_base(&storage_root, &SystemBaseSpec::new(512 * 1024 * 1024)?)?;
    let storage = prepare_instance(
        &storage_root,
        id,
        Some(&SourceDiskSpec::new(64 * 1024 * 1024)?),
    )?;
    let definition = net
        .attach(storage.define_bve(id.clone(), 1, 256, Firmware::Default)?)
        .with_boot_mode(BootMode::NetworkFirst);

    println!("BVE {id}: deterministic MAC = {}", definition.mac());
    println!("(this MAC must appear in the fixture's DHCPDISCOVER log)");

    let mut runtime =
        BveRuntime::create_with_isolated_network(&runtime_root, definition, storage.clone(), &net)?;
    runtime.start(&prerequisites)?;
    let state = runtime.observe()?;
    println!("started: {state:?} — firmware is running PXE; holding 20s for the DHCP/PXE exchange");
    assert_eq!(state, LifecycleState::Running);
    std::thread::sleep(Duration::from_secs(20));

    runtime.stop()?;
    assert_eq!(runtime.observe()?, LifecycleState::Stopped);
    runtime.destroy()?;
    destroy_instance_storage(storage)?;
    let _ = base;
    println!("BVE stopped and disposed. Network resources are left for `teardown {id}`.");
    Ok(())
}

fn teardown(id: &BveId) -> R {
    let plan = BveNetworkPlan::for_bve(id);
    let prepared = PreparedBveNetwork::from_prepared_plan(plan.clone());
    let fixture_dir = fixture_run_dir(&plan);

    // 1. netfilter accommodation removed explicitly first (also swept by
    //    teardown_network, but making it a visible step matches the required
    //    ordering).
    remove_dhcp_forward_accommodation(&plan)?;
    println!("netfilter: FORWARD accommodation removed (idempotent)");

    // 2. network resources — fails closed if dnsmasq is still in the netns.
    println!("removing (in order):");
    for r in prepared.teardown_order() {
        println!("  {:?} {}", r.kind, r.name);
    }
    match teardown_network(prepared) {
        Ok(()) => {}
        Err(BveNetworkError::FixtureStillRunning { netns, pids }) => {
            return Err(format!(
                "the DHCP/PXE fixture is still running in netns {netns} (pids {pids:?}). \
                 Ctrl-C the `start-fixture` terminal, wait for 'fixture exited', then retry teardown."
            )
            .into());
        }
        Err(e) => return Err(e.into()),
    }

    let _ = fs::remove_dir_all(&fixture_dir);
    println!("done. confirm with: bve_isolated_net verify-clean {id}");
    Ok(())
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
