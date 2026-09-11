//! Issue #70: one BVE's isolated, PXE-capable provisioning network.
//!
//! Topology (ADR-0024, "Design B"):
//!
//! ```text
//! QEMU (host netns, the same owned std::process::Child as #67)
//!   └─ -netdev tap → bvtap<h>  ─┐
//!                               ├─ private bridge bvbr<h>   (NO IP, NO physical uplink)
//!                bvh<h> (veth) ─┘        │
//!                                        └─ bvp<h> (veth peer) → netns bve-<h>
//!                                                                  ├─ 192.0.2.1/24, lo up
//!                                                                  ├─ no default route, no NAT
//!                                                                  └─ dnsmasq (bound to bvp<h> only)
//! ```
//!
//! Privilege model (ADR-0024 §"Privilege"):
//!
//! - creating the bridge/TAP/veth/netns needs `CAP_NET_ADMIN`; [`prepare`] and
//!   [`teardown`] shell out to `ip` via **argv** (never a shell string) and
//!   fail with [`BveNetworkError::PrivilegeRequired`] — an actionable error —
//!   when unprivileged. This crate **never** calls `sudo`, never prompts for a
//!   password, and never edits `/etc/qemu/bridge.conf`;
//! - the privileged preparation is an explicit owner-run step
//!   (`examples/bve_isolated_net`), separate from the VM lifecycle exactly as
//!   `qemu-img` storage preparation is separate from `qemu-system-x86_64`;
//! - once the TAP exists (created with `user <uid>`), `qemu-system-x86_64`
//!   opens it as the **normal user** — `start`/`observe`/`reset`/`stop`/
//!   `destroy` need no privilege;
//! - [`prepare`] performs **no** firewall / routing / sysctl change. On a host
//!   whose `br_netfilter` drops bridged DHCP (evidenced: WSL2 + Docker),
//!   [`apply_dhcp_forward_accommodation`] adds two `physdev`-scoped, UDP-67,
//!   runtime-only `FORWARD` ACCEPT rules limited to this BVE's TAP and fixture
//!   veth; [`remove_dhcp_forward_accommodation`] and [`teardown`] remove them.
//!   No sysctl is ever touched and nothing is persisted.
//!
//! Ownership and safety:
//!
//! - every resource name is *derived* from the validated [`BveId`] plus a
//!   fixed prefix plus a short stable hash — never a caller string — and fits
//!   the kernel `IFNAMSIZ` limit ([`crate::definition::MAX_IFNAME_LEN`]);
//! - if any target name already exists before setup:
//!   [`BveNetworkError::ResourceAlreadyExists`]. It is **not** adopted and
//!   **not** deleted;
//! - a partial setup rolls back **only** the resources that attempt created,
//!   in reverse order — never a `bv*` scan;
//! - [`teardown`] removes exactly the recorded resources and fails closed on
//!   an unexpected state (TAP still in use).
//!
//! This module owns exactly **one** isolated BVE network. It is not a network
//! framework: no `NetworkManager`, no `NetworkBackend` trait, no
//! `VirtualSwitch`.

use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

use crate::definition::{fnv1a_64, BveDefinition, BveId, IfName, NetworkAttachment};

/// The `ip` tool (iproute2) used for every network resource operation.
pub const IP_BINARY: &str = "ip";

/// The `iptables` tool, used **only** for the optional host-proof DHCP-forward
/// accommodation ([`apply_dhcp_forward_accommodation`]). The ADR-0024 topology
/// itself performs no firewall change.
pub const IPTABLES_BINARY: &str = "iptables";

/// The DHCP/PXE fixture daemon. Present on the reference host; never installed
/// automatically.
pub const DNSMASQ_BINARY: &str = "dnsmasq";

/// The TUN/TAP clone device a TAP is created from.
pub const TUN_DEVICE: &str = "/dev/net/tun";

/// The fixture peer address inside the dedicated netns. RFC 5737 TEST-NET-1
/// ("documentation") range — never routable on a real LAN.
pub const FIXTURE_PEER_IP: &str = "192.0.2.1";
/// [`FIXTURE_PEER_IP`] with its `/24` prefix.
pub const FIXTURE_PEER_CIDR: &str = "192.0.2.1/24";
/// First address the fixture hands out.
pub const FIXTURE_DHCP_FIRST: &str = "192.0.2.50";
/// Last address the fixture hands out.
pub const FIXTURE_DHCP_LAST: &str = "192.0.2.100";

/// Physical / host-uplink interface name prefixes that must never appear on
/// the private bridge.
const PHYSICAL_PREFIXES: [&str; 6] = ["eth", "en", "wl", "docker", "bond", "br-"];

/// The kind of host resource, for ordered teardown and messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetResourceKind {
    /// A Linux bridge (`ip link add … type bridge`).
    Bridge,
    /// A persistent TAP (`ip tuntap add … mode tap`).
    Tap,
    /// A veth pair, addressed by its host-side name (`ip link add … type veth`).
    Veth,
    /// A network namespace (`ip netns add`).
    Netns,
}

/// One host resource this crate owns for a BVE's isolated network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetResource {
    /// What kind of resource it is.
    pub kind: NetResourceKind,
    /// Its exact name.
    pub name: String,
}

impl NetResource {
    fn new(kind: NetResourceKind, name: impl Into<String>) -> Self {
        Self {
            kind,
            name: name.into(),
        }
    }
}

/// The deterministic set of host-resource names for one BVE's isolated
/// network. Pure: constructing a plan touches no host state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BveNetworkPlan {
    id: BveId,
    short_hash: String,
    bridge: IfName,
    tap: IfName,
    veth_host: IfName,
    veth_peer: IfName,
    netns: String,
}

impl BveNetworkPlan {
    /// Derives the plan for `id`.
    ///
    /// Names are `<prefix><8 hex>` where the hex is the low 32 bits of
    /// [`fnv1a_64`] of the id — deterministic, stable across hosts and builds,
    /// and at most 13 bytes (well inside `IFNAMSIZ`). A hash collision between
    /// two ids would produce identical names; that is caught at setup by the
    /// [`BveNetworkError::ResourceAlreadyExists`] check, never silently shared.
    pub fn for_bve(id: &BveId) -> Self {
        let short_hash = format!("{:08x}", fnv1a_64(id.as_str().as_bytes()) as u32);
        let name = |prefix: &str| {
            IfName::new(format!("{prefix}{short_hash}"))
                .expect("<=5-char prefix + 8 hex is always a valid <=15-byte interface name")
        };
        Self {
            id: id.clone(),
            bridge: name("bvbr"),
            tap: name("bvtap"),
            veth_host: name("bvh"),
            veth_peer: name("bvp"),
            netns: format!("bve-{short_hash}"),
            short_hash,
        }
    }

    /// The BVE this plan belongs to.
    pub fn bve_id(&self) -> &BveId {
        &self.id
    }

    /// The short stable hash the names are built from.
    pub fn short_hash(&self) -> &str {
        &self.short_hash
    }

    /// The private bridge (no IP, no uplink).
    pub fn bridge(&self) -> &IfName {
        &self.bridge
    }

    /// The TAP the BVE's virtio NIC opens.
    pub fn tap(&self) -> &IfName {
        &self.tap
    }

    /// The host-side veth end (enslaved to the bridge).
    pub fn veth_host(&self) -> &IfName {
        &self.veth_host
    }

    /// The fixture-side veth end (moved into the netns).
    pub fn veth_peer(&self) -> &IfName {
        &self.veth_peer
    }

    /// The dedicated network namespace hosting the DHCP/PXE fixture.
    pub fn netns(&self) -> &str {
        &self.netns
    }

    /// Every name that must be **absent** before setup. If any exists, setup
    /// fails and adopts nothing.
    pub fn collision_candidates(&self) -> Vec<NetResource> {
        vec![
            NetResource::new(NetResourceKind::Bridge, self.bridge.as_str()),
            NetResource::new(NetResourceKind::Tap, self.tap.as_str()),
            NetResource::new(NetResourceKind::Veth, self.veth_host.as_str()),
            NetResource::new(NetResourceKind::Veth, self.veth_peer.as_str()),
            NetResource::new(NetResourceKind::Netns, self.netns.clone()),
        ]
    }

    /// The resources a full setup creates, **in creation order**. Teardown and
    /// rollback walk this in reverse. The veth pair is recorded by its
    /// host-side name (deleting it removes the peer too); the peer that ends
    /// up inside the netns is removed with the netns.
    pub fn creation_order(&self) -> Vec<NetResource> {
        vec![
            NetResource::new(NetResourceKind::Bridge, self.bridge.as_str()),
            NetResource::new(NetResourceKind::Tap, self.tap.as_str()),
            NetResource::new(NetResourceKind::Veth, self.veth_host.as_str()),
            NetResource::new(NetResourceKind::Netns, self.netns.clone()),
        ]
    }
}

/// A realised isolated network for one BVE: the plan plus the exact resources
/// that were created for it, in creation order. Consumed by [`teardown`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedBveNetwork {
    plan: BveNetworkPlan,
    created: Vec<NetResource>,
}

impl PreparedBveNetwork {
    /// Wraps a plan whose resources a caller has already realised (a previous
    /// [`prepare`], or the host proof). Records the full [`creation order`]
    /// as owned.
    ///
    /// [`creation order`]: BveNetworkPlan::creation_order
    pub fn from_prepared_plan(plan: BveNetworkPlan) -> Self {
        let created = plan.creation_order();
        Self { plan, created }
    }

    /// The plan.
    pub fn plan(&self) -> &BveNetworkPlan {
        &self.plan
    }

    /// The TAP name a BVE definition must open to use this network.
    pub fn tap_ifname(&self) -> &IfName {
        self.plan.tap()
    }

    /// The [`NetworkAttachment`] for this prepared network.
    pub fn attachment(&self) -> NetworkAttachment {
        NetworkAttachment::IsolatedTap {
            ifname: self.plan.tap.clone(),
        }
    }

    /// Attaches this prepared network to `definition` (sets its isolated-TAP
    /// attachment). This is the only public way a TAP name reaches a
    /// [`BveDefinition`].
    pub fn attach(&self, definition: BveDefinition) -> BveDefinition {
        definition.with_isolated_tap(self.plan.tap.clone())
    }

    /// The resources [`teardown`] will remove, in the order it removes them
    /// (reverse of creation).
    pub fn teardown_order(&self) -> Vec<NetResource> {
        let mut order = self.created.clone();
        order.reverse();
        order
    }
}

/// Why an isolated-network operation failed.
#[derive(Debug, thiserror::Error)]
pub enum BveNetworkError {
    /// A required tool could not be executed at all.
    #[error("required tool {tool:?} could not be executed: {source}")]
    ToolUnavailable {
        /// The tool that could not run.
        tool: String,
        /// The underlying spawn error.
        #[source]
        source: std::io::Error,
    },

    /// `/dev/net/tun` is missing or not a character device.
    #[error("{TUN_DEVICE} is not an available character device: {detail}")]
    TunUnavailable {
        /// What was wrong.
        detail: String,
    },

    /// An operation needs `CAP_NET_ADMIN` this process does not have. The
    /// isolated-network setup must be run as an explicit privileged step.
    #[error(
        "operation {operation:?} requires network-admin privilege (CAP_NET_ADMIN); \
         run the isolated-network setup as an explicit privileged step — this runtime \
         never calls sudo"
    )]
    PrivilegeRequired {
        /// The operation that was refused.
        operation: String,
    },

    /// A target resource already exists. It is not adopted and not deleted.
    #[error("host network resource already exists and will not be adopted or deleted: {kind:?} {name:?}")]
    ResourceAlreadyExists {
        /// The kind of resource.
        kind: NetResourceKind,
        /// Its name.
        name: String,
    },

    /// A privileged `ip` step failed for a reason other than missing
    /// privilege.
    #[error("network setup step {operation:?} failed: {stderr}")]
    SetupFailed {
        /// A short label for the step.
        operation: String,
        /// Captured `ip` stderr.
        stderr: String,
    },

    /// Cleanup could not remove a resource this crate created.
    #[error("network cleanup could not remove {resource:?}: {detail}")]
    CleanupFailed {
        /// The resource left behind.
        resource: String,
        /// Why removal failed.
        detail: String,
    },

    /// The private bridge is not L2-isolated: it carries a port that is not
    /// this BVE's TAP or fixture veth (for example a physical interface).
    #[error("private bridge {bridge:?} is not L2-isolated: {detail}")]
    L2IsolationViolated {
        /// The bridge inspected.
        bridge: String,
        /// What was found.
        detail: String,
    },

    /// Teardown was attempted while the TAP still has a client attached (the
    /// VM is probably still running). Fails closed.
    #[error("refusing to tear down {tap:?}: it still has a client attached (stop and destroy the BVE first)")]
    NetworkStillInUse {
        /// The TAP still in use.
        tap: String,
    },

    /// Teardown was attempted while the DHCP/PXE fixture (or any process) is
    /// still running inside the netns. Removing the interface under a live
    /// `dnsmasq` is what produced `error binding DHCP socket to device …` in
    /// the first host proof. Fails closed: stop the fixture first.
    #[error("refusing to tear down netns {netns:?}: {pids:?} still running inside it (stop the DHCP/PXE fixture first)")]
    FixtureStillRunning {
        /// The netns that still has processes.
        netns: String,
        /// The PIDs found.
        pids: Vec<String>,
    },

    /// A `BveDefinition` handed to the isolated-network runtime path does not
    /// match the prepared network.
    #[error("BVE definition network does not match the prepared isolated network: {detail}")]
    DefinitionNetworkMismatch {
        /// What disagreed.
        detail: String,
    },

    /// `ip`/`bridge` produced output that could not be parsed.
    #[error("could not parse {tool} output: {detail}")]
    UnparseableToolOutput {
        /// Which tool.
        tool: String,
        /// What went wrong.
        detail: String,
    },
}

/// Confirms `ip` is runnable and `/dev/net/tun` is a usable character device.
/// **Read-only** — safe in any context.
pub fn check_network_prerequisites() -> Result<(), BveNetworkError> {
    Command::new(IP_BINARY)
        .arg("-V")
        .output()
        .map_err(|source| BveNetworkError::ToolUnavailable {
            tool: IP_BINARY.to_string(),
            source,
        })?;

    let meta = std::fs::metadata(TUN_DEVICE).map_err(|e| BveNetworkError::TunUnavailable {
        detail: e.to_string(),
    })?;
    if !meta.file_type().is_char_device() {
        return Err(BveNetworkError::TunUnavailable {
            detail: format!("{TUN_DEVICE} exists but is not a character device"),
        });
    }
    Ok(())
}

/// The exact `dnsmasq` argument vector for the disposable DHCP/PXE fixture.
/// Pure — spawns nothing.
///
/// The fixture binds **only** to the netns peer interface, disables DNS
/// (`--port=0`), serves a small DHCP range from the TEST-NET-1 block, and
/// offers a PXE boot filename so the exchange is a genuine PXE offer.
/// `--log-dhcp` makes it record `DHCPDISCOVER(<peer>) <MAC>` — the evidence
/// that the BVE's deterministic MAC crossed the NIC boundary. It runs
/// `--keep-in-foreground` and logs to stdout; its `--pid-file` is
/// [`fixture_pid_file`] (a stable path a harness reads to find and stop the
/// *real* `dnsmasq`, never the `sudo` wrapper PID).
pub fn fixture_dnsmasq_argv(plan: &BveNetworkPlan) -> Vec<String> {
    vec![
        "--keep-in-foreground".to_string(),
        "--log-facility=-".to_string(),
        "--log-dhcp".to_string(),
        "--no-resolv".to_string(),
        "--no-hosts".to_string(),
        "--port=0".to_string(),
        "--no-ping".to_string(),
        "--bind-interfaces".to_string(),
        format!("--interface={}", plan.veth_peer()),
        format!("--listen-address={FIXTURE_PEER_IP}"),
        "--dhcp-authoritative".to_string(),
        format!("--dhcp-range={FIXTURE_DHCP_FIRST},{FIXTURE_DHCP_LAST},255.255.255.0,5m"),
        "--dhcp-boot=bootx64.efi".to_string(),
        format!("--dhcp-leasefile={}", fixture_lease_file(plan).display()),
        format!("--pid-file={}", fixture_pid_file(plan).display()),
    ]
}

/// Scratch directory for the **privileged** DHCP/PXE fixture (`leases`,
/// `dnsmasq.pid`). It is created and owned by whoever runs the fixture —
/// normally `root` via `sudo` — so its files may legitimately be
/// `root`/`nobody`-owned. It belongs exclusively to the privileged domain.
///
/// A non-privileged BVE run must **never** place its control/storage tree here
/// or under any directory this path's owner creates — use [`bve_run_dir`],
/// which is disjoint. Keyed by the plan's stable short hash so [`teardown`]
/// can remove exactly this directory.
pub fn fixture_run_dir(plan: &BveNetworkPlan) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("bamep-bve-fixture-{}", plan.short_hash()))
}

/// The `dnsmasq` pid-file path ([`fixture_run_dir`]`/dnsmasq.pid`). The single
/// source of truth for locating the **real** fixture process — a harness reads
/// this, not the `sudo`/`ip` wrapper PID, and confirms the PID against
/// `ip netns pids <netns>` before stopping it.
pub fn fixture_pid_file(plan: &BveNetworkPlan) -> std::path::PathBuf {
    fixture_run_dir(plan).join("dnsmasq.pid")
}

/// The `dnsmasq` DHCP lease-file path ([`fixture_run_dir`]`/leases`).
pub fn fixture_lease_file(plan: &BveNetworkPlan) -> std::path::PathBuf {
    fixture_run_dir(plan).join("leases")
}

/// Scratch directory for a **non-privileged** BVE run against a prepared
/// isolated network (its control dir, storage root, QMP socket).
///
/// Process-specific, so a repeated run never collides and — critically — a run
/// started by the normal user never inherits a directory the privileged
/// fixture created. Its only shared ancestor with [`fixture_run_dir`] is the
/// OS temp root itself, which neither side owns. Self-cleaned by the run; not
/// touched by [`teardown`].
pub fn bve_run_dir(plan: &BveNetworkPlan) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "bamep-bve-run-{}-{}",
        plan.short_hash(),
        std::process::id()
    ))
}

/// The `(program, args)` to run the fixture inside its netns:
/// `ip netns exec <ns> dnsmasq <argv…>`. Privileged (the caller runs it as an
/// explicit step). Pure builder — spawns nothing.
pub fn fixture_command(plan: &BveNetworkPlan, dnsmasq_argv: &[String]) -> (String, Vec<String>) {
    let mut args = vec![
        "netns".to_string(),
        "exec".to_string(),
        plan.netns().to_string(),
        DNSMASQ_BINARY.to_string(),
    ];
    args.extend(dnsmasq_argv.iter().cloned());
    (IP_BINARY.to_string(), args)
}

// ---------------------------------------------------------------------------
// Issue #71: WinPE UEFI-PXE host-proof fixture.
//
// A superset of the #70 DHCP-only fixture: `dnsmasq` also serves TFTP and does
// architecture / iPXE-user-class tagging, and a disposable `python3` HTTP
// server (in the same netns) serves the boot script + wimboot + the pristine
// WinPE assets. Kept a *host-validation fixture*, never production DHCP/PXE.
// The #70 `fixture_dnsmasq_argv` is left untouched so `bve-network-proof.sh`
// keeps exercising its current contract.
// ---------------------------------------------------------------------------

/// The disposable HTTP fixture binary (Python standard library). Present on the
/// reference host; never installed automatically.
pub const PYTHON3_BINARY: &str = "python3";

/// TCP port the WinPE HTTP fixture listens on, inside the isolated netns only
/// (bound to [`FIXTURE_PEER_IP`]). A host port conflict is impossible.
pub const WINPE_FIXTURE_HTTP_PORT: u16 = 8080;

/// The bootstrap NBP offered over TFTP to a firmware EFI-x64 PXE client that is
/// not yet iPXE. The harness stages the retained official non-Secure-Boot
/// `snponly.efi` at this name in the TFTP root.
pub const WINPE_TFTP_BOOTFILE: &str = "snponly.efi";

/// TFTP root for the WinPE fixture: [`fixture_run_dir`]`/tftp`. Holds exactly
/// [`WINPE_TFTP_BOOTFILE`].
pub fn winpe_tftp_root(plan: &BveNetworkPlan) -> std::path::PathBuf {
    fixture_run_dir(plan).join("tftp")
}

/// HTTP root for the WinPE fixture: [`fixture_run_dir`]`/http`. Holds
/// `boot.ipxe`, `wimboot`, `BCD`, `boot.sdi`, `boot.wim`.
pub fn winpe_http_root(plan: &BveNetworkPlan) -> std::path::PathBuf {
    fixture_run_dir(plan).join("http")
}

/// The `http://<peer>:<port>` base URL the fixture serves the WinPE chain from.
pub fn winpe_http_base_url() -> String {
    format!("http://{FIXTURE_PEER_IP}:{WINPE_FIXTURE_HTTP_PORT}")
}

/// The `boot.ipxe` script the fixture serves: fetch `wimboot` as the kernel and
/// the pristine `BCD` + `boot.sdi` + `boot.wim` as initrds, then boot. This is
/// the ADR-0021 mechanism (iPXE + wimboot + external pristine WinPE assets)
/// minus the physical Secure-Boot wrapper. Pure — writes nothing.
pub fn winpe_boot_ipxe_script() -> String {
    let base = winpe_http_base_url();
    format!(
        "#!ipxe\n\
         echo Bamep BVE Issue 71 WinPE UEFI-PXE proof\n\
         kernel {base}/wimboot\n\
         initrd {base}/BCD BCD\n\
         initrd {base}/boot.sdi boot.sdi\n\
         initrd {base}/boot.wim boot.wim\n\
         boot\n"
    )
}

// ---------------------------------------------------------------------------
// Issue #73: BARE UEFI-PXE host-proof payload.
//
// Reuses the Issue #71 fixture wholesale — [`winpe_tftp_root`],
// [`winpe_http_root`], [`winpe_fixture_dnsmasq_argv`],
// [`winpe_http_fixture_command`], [`WINPE_TFTP_BOOTFILE`]
// (`snponly.efi`) — unchanged: the Issue #73 spike proved the same
// `snponly.efi` 2.0.0 UEFI/iPXE bootstrap already qualified by #71 also
// carries BARE. Only the served `boot.ipxe` payload and the two staged
// files differ: a Linux `bzImage` (booted via its EFI stub — already
// present in the BARE kernel `.config`, no kernel change) and a standalone
// `rootfs.cpio.gz` initramfs, instead of `wimboot` + WinPE assets.
// ---------------------------------------------------------------------------

/// The HTTP filename BARE's kernel is staged and requested under.
pub const BARE_PXE_KERNEL_NAME: &str = "bzImage";
/// The HTTP filename BARE's initramfs is staged and requested under.
pub const BARE_PXE_INITRD_NAME: &str = "rootfs.cpio.gz";

/// The `boot.ipxe` script BARE's UEFI-PXE proof serves: fetch [`BARE_PXE_KERNEL_NAME`]
/// as an EFI kernel (iPXE hands an EFI-stub-capable `bzImage` to the firmware
/// loader on UEFI; BARE's kernel already builds `CONFIG_EFI`/`CONFIG_EFI_STUB`
/// — Issue #73 spike, `docs/reference/bve-bare-uefi-pxe-host-proof.md`) and
/// [`BARE_PXE_INITRD_NAME`] as its initrd, then boot. `console=ttyS0,115200`
/// carries the serial capture; `panic=0` is deliberately fail-closed — a
/// panicked BARE kernel halts rather than silently rebooting into a second,
/// evidence-polluting PXE attempt. `imgfree` discards any image a prior boot
/// of the same BVE left registered before loading this boot's payload. Pure
/// — writes nothing.
pub fn bare_boot_ipxe_script() -> String {
    let base = winpe_http_base_url();
    format!(
        "#!ipxe\n\
         echo Bamep BVE Issue 73 BARE UEFI-PXE proof\n\
         imgfree\n\
         kernel {base}/{BARE_PXE_KERNEL_NAME} console=ttyS0,115200 panic=0\n\
         initrd {base}/{BARE_PXE_INITRD_NAME}\n\
         boot\n"
    )
}

/// The exact `dnsmasq` argv for the WinPE fixture (Issue #71). Pure.
///
/// Adds to the #70 DHCP fixture: TFTP serving from [`winpe_tftp_root`], an
/// architecture match (`option:client-arch,7` → EFI x86-64), an iPXE match
/// (encapsulated option 175 **and** the `iPXE` user-class), and a **two-stage**
/// `dhcp-boot`:
///
/// - a firmware EFI-x64 PXE client that is *not* iPXE is sent
///   [`WINPE_TFTP_BOOTFILE`] over TFTP;
/// - an iPXE client is sent `boot.ipxe` over HTTP.
///
/// The two tag sets (`!ipxe` vs `ipxe`) are mutually exclusive, so there is no
/// PXE→iPXE loop regardless of `dhcp-boot` order. `--log-queries` records the
/// TFTP request/transfer for the stage evidence.
pub fn winpe_fixture_dnsmasq_argv(plan: &BveNetworkPlan) -> Vec<String> {
    vec![
        "--keep-in-foreground".to_string(),
        "--log-facility=-".to_string(),
        "--log-dhcp".to_string(),
        "--log-queries".to_string(),
        "--no-resolv".to_string(),
        "--no-hosts".to_string(),
        "--port=0".to_string(),
        "--no-ping".to_string(),
        "--bind-interfaces".to_string(),
        format!("--interface={}", plan.veth_peer()),
        format!("--listen-address={FIXTURE_PEER_IP}"),
        "--dhcp-authoritative".to_string(),
        format!("--dhcp-range={FIXTURE_DHCP_FIRST},{FIXTURE_DHCP_LAST},255.255.255.0,5m"),
        "--enable-tftp".to_string(),
        format!("--tftp-root={}", winpe_tftp_root(plan).display()),
        "--tftp-no-fail".to_string(),
        "--dhcp-match=set:efi-x64,option:client-arch,7".to_string(),
        "--dhcp-match=set:ipxe,175".to_string(),
        "--dhcp-userclass=set:ipxe,iPXE".to_string(),
        format!("--dhcp-boot=tag:efi-x64,tag:!ipxe,{WINPE_TFTP_BOOTFILE}"),
        format!("--dhcp-boot=tag:ipxe,{}/boot.ipxe", winpe_http_base_url()),
        format!("--dhcp-leasefile={}", fixture_lease_file(plan).display()),
        format!("--pid-file={}", fixture_pid_file(plan).display()),
    ]
}

/// The `(program, args)` to run the WinPE HTTP fixture inside its netns:
/// `ip netns exec <ns> python3 -m http.server <port> --bind <peer>
/// --directory <http_root>`. Privileged (run as an explicit step). Pure.
pub fn winpe_http_fixture_command(plan: &BveNetworkPlan) -> (String, Vec<String>) {
    (
        IP_BINARY.to_string(),
        vec![
            "netns".to_string(),
            "exec".to_string(),
            plan.netns().to_string(),
            PYTHON3_BINARY.to_string(),
            "-m".to_string(),
            "http.server".to_string(),
            WINPE_FIXTURE_HTTP_PORT.to_string(),
            "--bind".to_string(),
            FIXTURE_PEER_IP.to_string(),
            "--directory".to_string(),
            winpe_http_root(plan).display().to_string(),
        ],
    )
}

// ---------------------------------------------------------------------------
// Host-proof DHCP-forward accommodation.
//
// NOT part of the ADR-0024 topology. On a host whose `br_netfilter` is active
// and whose iptables `FORWARD` policy is restrictive (evidenced: WSL2 + Docker,
// Issue #70 host proof — bridged IPv4 `68→67` reaches the TAP but is dropped
// before the fixture veth, while IPv6 from the same MAC crosses), the bridged
// DHCP exchange needs two `FORWARD` ACCEPT rules. They are:
//
// - `physdev`-scoped to *exactly* this BVE's TAP and fixture veth — the private
//   bridge has no other port and both interfaces only exist between `prepare`
//   and `teardown`, so the rules can match nothing else;
// - limited to UDP DHCP (client→server dport 67, server→client sport 67);
// - runtime-only: never written to a sysctl, never persisted, removed by
//   [`remove_dhcp_forward_accommodation`] and swept again by [`teardown`].
//
// A host that does not filter bridged IPv4 does not need this; an inserted
// ACCEPT rule is then simply inert.
// ---------------------------------------------------------------------------

/// The two `FORWARD` rules (each as the argv that follows `iptables -I`/`-D`)
/// for `plan`'s DHCP-forward accommodation. Pure.
pub fn dhcp_forward_accommodation_rules(plan: &BveNetworkPlan) -> [Vec<String>; 2] {
    let tap = plan.tap.as_str().to_string();
    let veth = plan.veth_host.as_str().to_string();
    let base = |physdev_in: &str, physdev_out: &str, port_flag: &str| {
        vec![
            "FORWARD".to_string(),
            "-m".to_string(),
            "physdev".to_string(),
            "--physdev-in".to_string(),
            physdev_in.to_string(),
            "--physdev-out".to_string(),
            physdev_out.to_string(),
            "-p".to_string(),
            "udp".to_string(),
            port_flag.to_string(),
            "67".to_string(),
            "-j".to_string(),
            "ACCEPT".to_string(),
        ]
    };
    [base(&tap, &veth, "--dport"), base(&veth, &tap, "--sport")]
}

/// **PRIVILEGED, optional.** Inserts `plan`'s DHCP-forward accommodation rules,
/// removing a stale duplicate of each first so repeated proof cycles never
/// accumulate rules. Returns [`BveNetworkError::ToolUnavailable`] if `iptables`
/// is absent (a host that needs no accommodation) — the caller may treat that
/// as "nothing to do".
pub fn apply_dhcp_forward_accommodation(plan: &BveNetworkPlan) -> Result<(), BveNetworkError> {
    apply_forward_rules(&dhcp_forward_accommodation_rules(plan))
}

/// **PRIVILEGED, idempotent.** Removes every copy of `plan`'s DHCP-forward
/// accommodation rules. A rule that is already absent is not an error; a rule
/// that exists but cannot be removed is [`BveNetworkError::CleanupFailed`].
/// `iptables` being absent means there is nothing to remove.
pub fn remove_dhcp_forward_accommodation(plan: &BveNetworkPlan) -> Result<(), BveNetworkError> {
    remove_forward_rules(plan, &dhcp_forward_accommodation_rules(plan))
}

/// The two **bridged-forward accommodation** rules for `plan` (Issue #71). Pure.
///
/// A generalisation of [`dhcp_forward_accommodation_rules`] from "UDP/67 only"
/// to **all** traffic between exactly this BVE's TAP and its fixture veth, both
/// directions. The WinPE UEFI-PXE proof has to carry DHCP **and** TFTP (UDP/69
/// plus a dynamic data port) **and** HTTP (TCP), so per-port rules do not
/// suffice on a `br_netfilter`-filtering host.
///
/// Still tightly bounded and reversible (ADR-0024 amendment): the private
/// bridge has exactly two ports, both `physdev`-named here, both existing only
/// between `prepare` and `teardown`; no uplink, no route, no NAT, no sysctl
/// change; removed by [`remove_bridged_forward_accommodation`] / [`teardown`].
/// Not applied by [`prepare`] — the #71 harness applies it explicitly, and the
/// #70 proof never does.
pub fn bridged_forward_accommodation_rules(plan: &BveNetworkPlan) -> [Vec<String>; 2] {
    let tap = plan.tap.as_str().to_string();
    let veth = plan.veth_host.as_str().to_string();
    let rule = |physdev_in: &str, physdev_out: &str| {
        vec![
            "FORWARD".to_string(),
            "-m".to_string(),
            "physdev".to_string(),
            "--physdev-in".to_string(),
            physdev_in.to_string(),
            "--physdev-out".to_string(),
            physdev_out.to_string(),
            "-j".to_string(),
            "ACCEPT".to_string(),
        ]
    };
    [rule(&tap, &veth), rule(&veth, &tap)]
}

/// **PRIVILEGED, optional (Issue #71).** Inserts `plan`'s
/// [`bridged_forward_accommodation_rules`], pre-deleting a stale copy of each.
pub fn apply_bridged_forward_accommodation(plan: &BveNetworkPlan) -> Result<(), BveNetworkError> {
    apply_forward_rules(&bridged_forward_accommodation_rules(plan))
}

/// **PRIVILEGED, idempotent (Issue #71).** Removes every copy of `plan`'s
/// [`bridged_forward_accommodation_rules`].
pub fn remove_bridged_forward_accommodation(plan: &BveNetworkPlan) -> Result<(), BveNetworkError> {
    remove_forward_rules(plan, &bridged_forward_accommodation_rules(plan))
}

fn apply_forward_rules(rules: &[Vec<String>]) -> Result<(), BveNetworkError> {
    check_iptables()?;
    for rule in rules {
        let mut predelete = vec!["-w".to_string(), "-D".to_string()];
        predelete.extend(rule.iter().cloned());
        let _ = run_iptables("accommodation-predelete", &predelete);

        let mut insert = vec!["-w".to_string(), "-I".to_string()];
        insert.extend(rule.iter().cloned());
        run_iptables("accommodation-insert", &insert)?;
    }
    Ok(())
}

fn remove_forward_rules(
    plan: &BveNetworkPlan,
    rules: &[Vec<String>],
) -> Result<(), BveNetworkError> {
    if check_iptables().is_err() {
        return Ok(());
    }
    let mut first_error: Option<BveNetworkError> = None;
    for rule in rules {
        loop {
            let mut delete = vec!["-w".to_string(), "-D".to_string()];
            delete.extend(rule.iter().cloned());
            match run_iptables("accommodation-delete", &delete) {
                Ok(_) => continue, // deleted one copy; try again in case -I ran twice
                Err(BveNetworkError::SetupFailed { stderr, .. })
                    if stderr.to_lowercase().contains("bad rule")
                        || stderr.to_lowercase().contains("matching rule exist")
                        || stderr.to_lowercase().contains("no chain/target/match") =>
                {
                    break
                }
                Err(e) => {
                    first_error.get_or_insert(BveNetworkError::CleanupFailed {
                        resource: format!("iptables FORWARD accommodation for {}", plan.netns()),
                        detail: e.to_string(),
                    });
                    break;
                }
            }
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Privileged host operations. Written for the owner-run host proof
// (`examples/bve_isolated_net`); an ordinary `cargo test` never reaches them.
// ---------------------------------------------------------------------------

/// **PRIVILEGED.** Creates the isolated network for `plan`: a private bridge
/// (no IP, no uplink), a TAP owned by `tap_owner` and enslaved to the bridge,
/// a veth pair (host end on the bridge), and a dedicated netns holding the
/// peer end with a TEST-NET-1 address, `lo` up, and nothing else — no default
/// route, no NAT.
///
/// Fails closed:
///
/// - any pre-existing target name → [`BveNetworkError::ResourceAlreadyExists`]
///   (no adoption, no deletion);
/// - missing privilege → [`BveNetworkError::PrivilegeRequired`];
/// - a failure part-way through rolls back **only** what this attempt created,
///   in reverse order;
/// - the L2-isolation invariant is asserted before returning.
pub fn prepare(
    plan: BveNetworkPlan,
    tap_owner: &str,
) -> Result<PreparedBveNetwork, BveNetworkError> {
    check_network_prerequisites()?;
    ensure_absent(&plan)?;

    let mut created: Vec<NetResource> = Vec::new();
    match prepare_inner(&plan, tap_owner, &mut created) {
        Ok(()) => Ok(PreparedBveNetwork { plan, created }),
        Err(setup_err) => {
            // Roll back exactly what we made, newest first.
            rollback(&created);
            Err(setup_err)
        }
    }
}

fn prepare_inner(
    plan: &BveNetworkPlan,
    tap_owner: &str,
    created: &mut Vec<NetResource>,
) -> Result<(), BveNetworkError> {
    let br = plan.bridge.as_str();
    let tap = plan.tap.as_str();
    let vh = plan.veth_host.as_str();
    let vp = plan.veth_peer.as_str();
    let ns = plan.netns();

    // 1. private bridge, no IP.
    run_ip(
        "create-bridge",
        &["link", "add", "name", br, "type", "bridge"],
    )?;
    created.push(NetResource::new(NetResourceKind::Bridge, br));
    run_ip("bridge-up", &["link", "set", br, "up"])?;

    // 2. TAP owned by the invoking user, enslaved to the bridge.
    run_ip(
        "create-tap",
        &[
            "tuntap", "add", "dev", tap, "mode", "tap", "user", tap_owner,
        ],
    )?;
    created.push(NetResource::new(NetResourceKind::Tap, tap));
    run_ip("tap-master", &["link", "set", tap, "master", br])?;
    run_ip("tap-up", &["link", "set", tap, "up"])?;

    // 3. veth pair; host end on the bridge.
    run_ip(
        "create-veth",
        &["link", "add", vh, "type", "veth", "peer", "name", vp],
    )?;
    created.push(NetResource::new(NetResourceKind::Veth, vh));
    run_ip("veth-master", &["link", "set", vh, "master", br])?;
    run_ip("veth-host-up", &["link", "set", vh, "up"])?;

    // 4. dedicated netns; move the peer into it.
    run_ip("create-netns", &["netns", "add", ns])?;
    created.push(NetResource::new(NetResourceKind::Netns, ns));
    run_ip("veth-peer-netns", &["link", "set", vp, "netns", ns])?;
    run_ip("netns-lo-up", &["-n", ns, "link", "set", "lo", "up"])?;
    run_ip("veth-peer-up", &["-n", ns, "link", "set", vp, "up"])?;
    run_ip(
        "veth-peer-addr",
        &["-n", ns, "addr", "add", FIXTURE_PEER_CIDR, "dev", vp],
    )?;

    // 5. L2-isolation invariant.
    let members = bridge_members(&plan.bridge)?;
    assert_only_expected_members(plan, &members)?;

    Ok(())
}

/// Asserts the private bridge carries **only** this BVE's TAP and fixture
/// veth — no physical interface, no host uplink. **Read-only.** Returns the
/// observed member list for the proof log.
pub fn assert_l2_isolation(plan: &BveNetworkPlan) -> Result<Vec<String>, BveNetworkError> {
    let members = bridge_members(&plan.bridge)?;
    assert_only_expected_members(plan, &members)?;
    Ok(members)
}

fn assert_only_expected_members(
    plan: &BveNetworkPlan,
    members: &[String],
) -> Result<(), BveNetworkError> {
    let expected = [plan.tap.as_str(), plan.veth_host.as_str()];
    for member in members {
        if PHYSICAL_PREFIXES
            .iter()
            .any(|p| member.starts_with(p) && member.as_str() != plan.bridge.as_str())
        {
            return Err(BveNetworkError::L2IsolationViolated {
                bridge: plan.bridge.to_string(),
                detail: format!("physical/uplink-looking port {member:?} is enslaved"),
            });
        }
        if !expected.contains(&member.as_str()) {
            return Err(BveNetworkError::L2IsolationViolated {
                bridge: plan.bridge.to_string(),
                detail: format!("unexpected port {member:?} (expected only {expected:?})"),
            });
        }
    }
    Ok(())
}

/// **PRIVILEGED.** Removes exactly the resources recorded in `prepared`, in
/// reverse creation order.
///
/// Fail-closed ordering (Issue #70 host-proof lesson):
///
/// 1. refuse if the DHCP/PXE fixture is still running in the netns
///    ([`BveNetworkError::FixtureStillRunning`]) — removing the interface under
///    a live `dnsmasq` is what caused `error binding DHCP socket to device`;
/// 2. refuse if the TAP still has a client
///    ([`BveNetworkError::NetworkStillInUse`]);
/// 3. sweep `prepared`'s DHCP-forward accommodation rules (idempotent), so a
///    proof never leaves an `iptables` rule behind even if it was applied
///    out-of-band;
/// 4. remove the bridge/TAP/veth/netns, newest-first.
///
/// A resource that should be gone but cannot be removed is a
/// [`BveNetworkError::CleanupFailed`]. Never scans for `bv*` names.
pub fn teardown(prepared: PreparedBveNetwork) -> Result<(), BveNetworkError> {
    let pids = netns_processes(prepared.plan.netns());
    if !pids.is_empty() {
        return Err(BveNetworkError::FixtureStillRunning {
            netns: prepared.plan.netns().to_string(),
            pids,
        });
    }
    if tap_has_client(prepared.plan.tap.as_str()) {
        return Err(BveNetworkError::NetworkStillInUse {
            tap: prepared.plan.tap.to_string(),
        });
    }

    let mut first_error: Option<BveNetworkError> = None;
    // Sweep both accommodations idempotently — the narrow #70 DHCP-only one and
    // the broad #71 bridged one — so a proof never leaves an `iptables` rule
    // behind regardless of which harness applied one.
    if let Err(e) = remove_dhcp_forward_accommodation(&prepared.plan) {
        first_error.get_or_insert(e);
    }
    if let Err(e) = remove_bridged_forward_accommodation(&prepared.plan) {
        first_error.get_or_insert(e);
    }
    for resource in prepared.teardown_order() {
        if let Err(e) = remove_resource(&resource) {
            first_error.get_or_insert(e);
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Best-effort rollback of a partial [`prepare`]: remove newest-first, ignore
/// "already gone", never surface a second error over the setup failure.
fn rollback(created: &[NetResource]) {
    for resource in created.iter().rev() {
        let _ = remove_resource(resource);
    }
}

fn remove_resource(resource: &NetResource) -> Result<(), BveNetworkError> {
    let (op, args): (&str, Vec<&str>) = match resource.kind {
        NetResourceKind::Netns => ("del-netns", vec!["netns", "del", &resource.name]),
        NetResourceKind::Veth => ("del-veth", vec!["link", "del", &resource.name]),
        NetResourceKind::Tap => (
            "del-tap",
            vec!["tuntap", "del", "dev", &resource.name, "mode", "tap"],
        ),
        NetResourceKind::Bridge => ("del-bridge", vec!["link", "del", &resource.name]),
    };
    match run_ip(op, &args) {
        Ok(_) => Ok(()),
        // Idempotent: something already removed it (rollback overlap, netns
        // delete taking its veth peer with it).
        Err(BveNetworkError::SetupFailed { stderr, .. })
            if stderr.contains("Cannot find")
                || stderr.contains("does not exist")
                || stderr.contains("No such file") =>
        {
            Ok(())
        }
        Err(BveNetworkError::PrivilegeRequired { .. }) => Err(BveNetworkError::CleanupFailed {
            resource: format!("{:?} {}", resource.kind, resource.name),
            detail: "missing CAP_NET_ADMIN".to_string(),
        }),
        Err(e) => Err(BveNetworkError::CleanupFailed {
            resource: format!("{:?} {}", resource.kind, resource.name),
            detail: e.to_string(),
        }),
    }
}

/// Whether a persistent TAP currently has a client (QEMU) attached. Heuristic:
/// an unattached persistent TAP has no carrier. The authoritative guarantee is
/// caller sequencing (destroy the BVE before tearing down its network); this
/// is the fail-closed backstop.
fn tap_has_client(tap: &str) -> bool {
    std::fs::read_to_string(format!("/sys/class/net/{tap}/carrier"))
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

fn ensure_absent(plan: &BveNetworkPlan) -> Result<(), BveNetworkError> {
    for candidate in plan.collision_candidates() {
        let exists = match candidate.kind {
            NetResourceKind::Netns => Path::new("/run/netns").join(&candidate.name).exists(),
            _ => interface_exists(&candidate.name)?,
        };
        if exists {
            return Err(BveNetworkError::ResourceAlreadyExists {
                kind: candidate.kind,
                name: candidate.name,
            });
        }
    }
    Ok(())
}

fn interface_exists(name: &str) -> Result<bool, BveNetworkError> {
    let output = Command::new(IP_BINARY)
        .args(["link", "show", name])
        .output()
        .map_err(|source| BveNetworkError::ToolUnavailable {
            tool: IP_BINARY.to_string(),
            source,
        })?;
    Ok(output.status.success())
}

/// The interface names enslaved to `bridge`, via `ip -j link show master
/// <bridge>`.
fn bridge_members(bridge: &IfName) -> Result<Vec<String>, BveNetworkError> {
    let stdout = run_ip(
        "list-bridge-members",
        &["-j", "link", "show", "master", bridge.as_str()],
    )?;
    let value: Value =
        serde_json::from_str(&stdout).map_err(|e| BveNetworkError::UnparseableToolOutput {
            tool: IP_BINARY.to_string(),
            detail: e.to_string(),
        })?;
    let array = value
        .as_array()
        .ok_or_else(|| BveNetworkError::UnparseableToolOutput {
            tool: IP_BINARY.to_string(),
            detail: "expected a JSON array from `ip -j link`".to_string(),
        })?;
    Ok(array
        .iter()
        .filter_map(|entry| entry.get("ifname").and_then(Value::as_str))
        .map(str::to_string)
        .collect())
}

fn run_ip(operation: &str, args: &[&str]) -> Result<String, BveNetworkError> {
    run_tool(IP_BINARY, operation, args)
}

fn run_iptables(operation: &str, args: &[String]) -> Result<String, BveNetworkError> {
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    run_tool(IPTABLES_BINARY, operation, &borrowed)
}

fn run_tool(binary: &str, operation: &str, args: &[&str]) -> Result<String, BveNetworkError> {
    let output = Command::new(binary).args(args).output().map_err(|source| {
        BveNetworkError::ToolUnavailable {
            tool: binary.to_string(),
            source,
        }
    })?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let lower = stderr.to_lowercase();
    if lower.contains("not permitted")
        || lower.contains("permission denied")
        || lower.contains("must be root")
    {
        return Err(BveNetworkError::PrivilegeRequired {
            operation: operation.to_string(),
        });
    }
    Err(BveNetworkError::SetupFailed {
        operation: operation.to_string(),
        stderr,
    })
}

fn check_iptables() -> Result<(), BveNetworkError> {
    Command::new(IPTABLES_BINARY)
        .arg("--version")
        .output()
        .map_err(|source| BveNetworkError::ToolUnavailable {
            tool: IPTABLES_BINARY.to_string(),
            source,
        })?;
    Ok(())
}

/// PIDs currently running inside the named netns (`ip netns pids <ns>`). Empty
/// when the netns does not exist. Privileged in practice (it reads other
/// processes' `/proc/*/ns/net`).
fn netns_processes(netns: &str) -> Vec<String> {
    if !Path::new("/run/netns").join(netns).exists() {
        return Vec::new();
    }
    run_ip("netns-pids", &["netns", "pids", netns])
        .map(|out| {
            out.split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

/// Which of `plan`'s host resources currently exist. **Read-only.** Empty means
/// a previous teardown left nothing behind — the reproducibility check.
pub fn residual_resources(plan: &BveNetworkPlan) -> Vec<NetResource> {
    let mut present = Vec::new();
    for candidate in plan.collision_candidates() {
        let exists = match candidate.kind {
            NetResourceKind::Netns => Path::new("/run/netns").join(&candidate.name).exists(),
            _ => interface_exists(&candidate.name).unwrap_or(false),
        };
        if exists {
            present.push(candidate);
        }
    }
    present
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> BveId {
        BveId::new(s).unwrap()
    }

    #[test]
    fn names_are_deterministic_stable_and_within_ifnamsiz() {
        let a = BveNetworkPlan::for_bve(&id("lab-03"));
        let a2 = BveNetworkPlan::for_bve(&id("lab-03"));
        let b = BveNetworkPlan::for_bve(&id("lab-04"));

        assert_eq!(a, a2, "same id -> same plan");
        assert_ne!(a.bridge(), b.bridge());
        assert_ne!(a.tap(), b.tap());
        assert_ne!(a.veth_host(), b.veth_host());
        assert_ne!(a.veth_peer(), b.veth_peer());
        assert_ne!(a.netns(), b.netns());

        for name in [
            a.bridge().as_str(),
            a.tap().as_str(),
            a.veth_host().as_str(),
            a.veth_peer().as_str(),
        ] {
            assert!(
                name.len() <= crate::definition::MAX_IFNAME_LEN,
                "{name:?} exceeds IFNAMSIZ"
            );
            // Re-validating proves the name is argv-safe.
            assert!(IfName::new(name).is_ok());
        }
    }

    #[test]
    fn names_are_built_only_from_the_id_hash_never_caller_input() {
        // Even a hostile id (already rejected by BveId, but belt-and-braces):
        // the plan can only be built from a validated BveId, and every name is
        // `<fixed prefix><hex(hash)>`.
        let plan = BveNetworkPlan::for_bve(&id("x1-y2_z3"));
        let h = plan.short_hash().to_string();
        assert_eq!(plan.bridge().as_str(), format!("bvbr{h}"));
        assert_eq!(plan.tap().as_str(), format!("bvtap{h}"));
        assert_eq!(plan.veth_host().as_str(), format!("bvh{h}"));
        assert_eq!(plan.veth_peer().as_str(), format!("bvp{h}"));
        assert_eq!(plan.netns(), format!("bve-{h}"));
        assert_eq!(h.len(), 8);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn creation_order_and_teardown_order_are_exact_reverses() {
        let plan = BveNetworkPlan::for_bve(&id("bve-order"));
        let created = plan.creation_order();
        assert_eq!(
            created.iter().map(|r| r.kind).collect::<Vec<_>>(),
            vec![
                NetResourceKind::Bridge,
                NetResourceKind::Tap,
                NetResourceKind::Veth,
                NetResourceKind::Netns,
            ]
        );

        let prepared = PreparedBveNetwork::from_prepared_plan(plan);
        let mut expected_reverse = created.clone();
        expected_reverse.reverse();
        assert_eq!(prepared.teardown_order(), expected_reverse);
    }

    #[test]
    fn collision_candidates_cover_both_veth_ends_and_the_netns() {
        let plan = BveNetworkPlan::for_bve(&id("bve-collide"));
        let names: Vec<String> = plan
            .collision_candidates()
            .into_iter()
            .map(|r| r.name)
            .collect();
        for expected in [
            plan.bridge().as_str(),
            plan.tap().as_str(),
            plan.veth_host().as_str(),
            plan.veth_peer().as_str(),
            plan.netns(),
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "collision candidates must include {expected:?}"
            );
        }
    }

    #[test]
    fn teardown_order_after_a_partial_setup_contains_only_what_was_created() {
        let plan = BveNetworkPlan::for_bve(&id("bve-partial"));
        // Simulate: bridge + tap created, veth creation failed.
        let created = vec![
            NetResource::new(NetResourceKind::Bridge, plan.bridge().as_str()),
            NetResource::new(NetResourceKind::Tap, plan.tap().as_str()),
        ];
        let prepared = PreparedBveNetwork {
            plan: plan.clone(),
            created,
        };
        assert_eq!(
            prepared.teardown_order(),
            vec![
                NetResource::new(NetResourceKind::Tap, plan.tap().as_str()),
                NetResource::new(NetResourceKind::Bridge, plan.bridge().as_str()),
            ],
            "rollback touches only the two resources that were created, newest first"
        );
    }

    #[test]
    fn attach_puts_exactly_the_prepared_tap_on_the_definition() {
        use crate::definition::{BveDefinition, DiskAttachment, DiskFormat, Firmware};

        let plan = BveNetworkPlan::for_bve(&id("bve-attach"));
        let prepared = PreparedBveNetwork::from_prepared_plan(plan.clone());
        let def = BveDefinition::new(
            id("bve-attach"),
            1,
            256,
            Firmware::Default,
            DiskAttachment::system("/s.qcow2", DiskFormat::Qcow2).unwrap(),
        )
        .unwrap();

        let attached = prepared.attach(def);
        assert_eq!(
            attached.network(),
            &NetworkAttachment::IsolatedTap {
                ifname: plan.tap().clone()
            }
        );
        assert_eq!(prepared.tap_ifname(), plan.tap());
        assert_eq!(prepared.attachment(), attached.network().clone());
    }

    #[test]
    fn fixture_dnsmasq_argv_binds_only_the_peer_and_serves_test_net_1() {
        let plan = BveNetworkPlan::for_bve(&id("bve-fixture"));
        let argv = fixture_dnsmasq_argv(&plan);

        assert!(argv.contains(&format!("--interface={}", plan.veth_peer())));
        assert!(argv.iter().any(|a| a == "--bind-interfaces"));
        assert!(argv.iter().any(|a| a == "--port=0"), "DNS disabled");
        assert!(argv.iter().any(|a| a == "--log-dhcp"));
        let range = argv
            .iter()
            .find(|a| a.starts_with("--dhcp-range="))
            .unwrap();
        assert!(range.contains("192.0.2.50") && range.contains("192.0.2.100"));

        // No physical interface is ever named.
        for arg in &argv {
            for physical in ["eth0", "wlan0", "docker0", "en", "bond0"] {
                assert!(
                    !arg.split(['=', ',']).any(|tok| tok == physical),
                    "fixture argv must not name a physical interface: {arg:?}"
                );
            }
        }
    }

    #[test]
    fn fixture_command_runs_dnsmasq_inside_the_netns() {
        let plan = BveNetworkPlan::for_bve(&id("bve-fixcmd"));
        let argv = fixture_dnsmasq_argv(&plan);
        let (program, args) = fixture_command(&plan, &argv);
        assert_eq!(program, "ip");
        assert_eq!(&args[..4], &["netns", "exec", plan.netns(), "dnsmasq"]);
        assert_eq!(&args[4..], &argv[..]);
    }

    #[test]
    fn l2_isolation_check_rejects_a_physical_or_unexpected_port() {
        let plan = BveNetworkPlan::for_bve(&id("bve-l2"));
        let ok = [plan.tap().to_string(), plan.veth_host().to_string()];
        assert!(assert_only_expected_members(&plan, &ok).is_ok());

        let with_eth = [plan.tap().to_string(), "eth0".to_string()];
        assert!(matches!(
            assert_only_expected_members(&plan, &with_eth),
            Err(BveNetworkError::L2IsolationViolated { .. })
        ));

        let with_stray = [plan.tap().to_string(), "tap-somethingelse".to_string()];
        assert!(matches!(
            assert_only_expected_members(&plan, &with_stray),
            Err(BveNetworkError::L2IsolationViolated { .. })
        ));
    }

    #[test]
    fn from_prepared_plan_owns_the_full_creation_set() {
        let plan = BveNetworkPlan::for_bve(&id("bve-full"));
        let prepared = PreparedBveNetwork::from_prepared_plan(plan.clone());
        assert_eq!(prepared.created, plan.creation_order());
    }

    #[test]
    fn fixture_and_bve_run_dirs_never_share_an_ownership_sensitive_directory() {
        let plan = BveNetworkPlan::for_bve(&id("bve-domains"));
        let fixture = fixture_run_dir(&plan);
        let bve = bve_run_dir(&plan);
        let temp = std::env::temp_dir();

        // Neither tree contains the other, so a privileged fixture that
        // creates and owns `fixture` is never an ancestor of anything the
        // non-privileged BVE run writes under `bve` (and vice versa).
        assert!(!bve.starts_with(&fixture));
        assert!(!fixture.starts_with(&bve));
        assert_ne!(fixture, bve);
        assert_ne!(fixture.file_name(), bve.file_name());

        // Their only common ancestor is the OS temp root itself — a directory
        // that already exists and that neither `setup`/`start-fixture` nor
        // `run-bve` creates or chowns. There is no intermediate writable dir
        // one side makes and the other side must write into.
        assert_eq!(fixture.parent(), Some(temp.as_path()));
        assert_eq!(bve.parent(), Some(temp.as_path()));

        // The fixture dir is stable (teardown removes exactly it); the BVE run
        // dir is process-specific so a run never inherits another run's — or
        // the privileged fixture's — directory.
        assert_eq!(
            fixture,
            fixture_run_dir(&plan),
            "fixture dir is deterministic"
        );
        assert!(bve
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(&std::process::id().to_string()));
    }

    #[test]
    fn fixture_argv_writes_only_under_the_fixture_run_dir() {
        let plan = BveNetworkPlan::for_bve(&id("bve-fixpaths"));
        let dir = fixture_run_dir(&plan);
        assert_eq!(fixture_pid_file(&plan), dir.join("dnsmasq.pid"));
        assert_eq!(fixture_lease_file(&plan), dir.join("leases"));
        for arg in fixture_dnsmasq_argv(&plan) {
            for flag in ["--dhcp-leasefile=", "--pid-file="] {
                if let Some(path) = arg.strip_prefix(flag) {
                    assert!(
                        Path::new(path).starts_with(&dir),
                        "{flag} path {path:?} escapes the fixture run dir {dir:?}"
                    );
                }
            }
        }
        assert!(fixture_dnsmasq_argv(&plan)
            .contains(&format!("--pid-file={}", fixture_pid_file(&plan).display())));
    }

    #[test]
    fn dhcp_accommodation_rules_are_scoped_to_this_bve_and_to_dhcp_only() {
        let plan = BveNetworkPlan::for_bve(&id("bve-nf"));
        let other = BveNetworkPlan::for_bve(&id("bve-nf-other"));
        let [to_server, to_client] = dhcp_forward_accommodation_rules(&plan);

        for rule in [&to_server, &to_client] {
            assert_eq!(rule[0], "FORWARD");
            assert!(
                rule.contains(&"physdev".to_string()),
                "must be physdev-scoped"
            );
            assert!(rule.contains(&"udp".to_string()));
            assert!(rule.contains(&"67".to_string()), "DHCP port only");
            assert_eq!(rule.last().unwrap(), "ACCEPT");
            // Only this BVE's own interfaces are named — never a physical one,
            // never another BVE's, never the bridge itself.
            let names: Vec<&String> = rule
                .iter()
                .filter(|t| {
                    t.as_str() == plan.tap().as_str() || t.as_str() == plan.veth_host().as_str()
                })
                .collect();
            assert_eq!(names.len(), 2, "exactly the TAP and the fixture veth");
            for forbidden in [
                plan.bridge().as_str(),
                plan.veth_peer().as_str(),
                other.tap().as_str(),
                "eth0",
                "docker0",
            ] {
                assert!(
                    !rule.iter().any(|t| t == forbidden),
                    "rule must not mention {forbidden:?}: {rule:?}"
                );
            }
        }
        // Direction: client->server matches dest 67, server->client source 67.
        assert!(to_server.contains(&"--dport".to_string()));
        assert!(to_server
            .windows(2)
            .any(|w| w[0] == "--physdev-in" && w[1] == plan.tap().as_str()));
        assert!(to_client.contains(&"--sport".to_string()));
        assert!(to_client
            .windows(2)
            .any(|w| w[0] == "--physdev-in" && w[1] == plan.veth_host().as_str()));

        // Deterministic across calls (repeated proof cycles get identical rules).
        assert_eq!(
            dhcp_forward_accommodation_rules(&plan),
            dhcp_forward_accommodation_rules(&BveNetworkPlan::for_bve(&id("bve-nf")))
        );
    }

    // ---- Issue #71 WinPE UEFI-PXE fixture --------------------------------

    #[test]
    fn winpe_dnsmasq_argv_adds_tftp_arch_and_ipxe_tags_with_a_loop_free_two_stage_boot() {
        let plan = BveNetworkPlan::for_bve(&id("bve-winpe"));
        let argv = winpe_fixture_dnsmasq_argv(&plan);

        // Still bound only to the peer; DNS off; DHCP range unchanged from #70.
        assert!(argv.contains(&format!("--interface={}", plan.veth_peer())));
        assert!(argv.iter().any(|a| a == "--port=0"));
        assert!(argv
            .iter()
            .any(|a| a.starts_with("--dhcp-range=192.0.2.50,192.0.2.100")));

        // TFTP from the plan's own root, holding the bootstrap NBP.
        assert!(argv.iter().any(|a| a == "--enable-tftp"));
        assert!(argv.contains(&format!("--tftp-root={}", winpe_tftp_root(&plan).display())));

        // Architecture (EFI x86-64 = option 93 value 7) and iPXE detection.
        assert!(argv.contains(&"--dhcp-match=set:efi-x64,option:client-arch,7".to_string()));
        assert!(argv.contains(&"--dhcp-match=set:ipxe,175".to_string()));
        assert!(argv.contains(&"--dhcp-userclass=set:ipxe,iPXE".to_string()));

        // Two-stage, mutually exclusive tags -> no PXE/iPXE loop.
        let firmware_stage = argv
            .iter()
            .find(|a| a.starts_with("--dhcp-boot=tag:efi-x64,tag:!ipxe,"))
            .expect("firmware EFI-x64 non-iPXE client gets the TFTP bootstrap");
        assert!(firmware_stage.ends_with("snponly.efi"));
        assert!(!firmware_stage.contains("http"));
        let ipxe_stage = argv
            .iter()
            .find(|a| a.starts_with("--dhcp-boot=tag:ipxe,"))
            .expect("an iPXE client gets the HTTP script");
        assert!(ipxe_stage.contains("http://192.0.2.1:8080/boot.ipxe"));

        // TFTP evidence for the stage parser.
        assert!(argv.iter().any(|a| a == "--log-queries"));
        // No physical interface, ever.
        for arg in &argv {
            for physical in ["eth0", "wlan0", "docker0", "bond0"] {
                assert!(!arg.split(['=', ',', '/']).any(|t| t == physical));
            }
        }
    }

    #[test]
    fn winpe_http_fixture_runs_python_in_the_netns_bound_to_the_peer_only() {
        let plan = BveNetworkPlan::for_bve(&id("bve-winpe-http"));
        let (program, args) = winpe_http_fixture_command(&plan);
        assert_eq!(program, "ip");
        assert_eq!(&args[..3], &["netns", "exec", plan.netns()]);
        assert!(args.contains(&"python3".to_string()));
        assert!(args.contains(&"http.server".to_string()));
        assert!(args.contains(&"8080".to_string()));
        // Bound to the isolated peer address, serving the plan's own http root.
        let bind = args.iter().position(|a| a == "--bind").unwrap();
        assert_eq!(args[bind + 1], "192.0.2.1");
        let dir = args.iter().position(|a| a == "--directory").unwrap();
        assert_eq!(args[dir + 1], winpe_http_root(&plan).display().to_string());
    }

    #[test]
    fn winpe_boot_ipxe_script_is_the_wimboot_chain_over_http() {
        let s = winpe_boot_ipxe_script();
        assert!(s.starts_with("#!ipxe\n"));
        assert!(s.contains("kernel http://192.0.2.1:8080/wimboot"));
        assert!(s.contains("initrd http://192.0.2.1:8080/BCD BCD"));
        assert!(s.contains("initrd http://192.0.2.1:8080/boot.sdi boot.sdi"));
        assert!(s.contains("initrd http://192.0.2.1:8080/boot.wim boot.wim"));
        assert!(s.trim_end().ends_with("boot"));
        // The Secure-Boot wrapper is out of scope: no shim anywhere.
        assert!(!s.contains("shim"));
    }

    // ---- Issue #73 BARE UEFI-PXE fixture (reuses the #71 TFTP/HTTP roots and
    // dnsmasq argv unchanged; only the served boot.ipxe payload differs) -----

    #[test]
    fn bare_boot_ipxe_script_is_the_kernel_initrd_chain_over_http() {
        let s = bare_boot_ipxe_script();
        assert!(s.starts_with("#!ipxe\n"));
        // Clears any image a previous chain (or a previous boot of the same
        // BVE) left loaded — never boot a stale payload.
        assert!(s.contains("imgfree\n"));
        assert!(s.contains(&format!(
            "kernel http://192.0.2.1:8080/{BARE_PXE_KERNEL_NAME} console=ttyS0,115200 panic=0"
        )));
        assert!(s.contains(&format!(
            "initrd http://192.0.2.1:8080/{BARE_PXE_INITRD_NAME}"
        )));
        assert!(s.trim_end().ends_with("boot"));
        // Never the WinPE payload.
        assert!(!s.contains("wimboot"));
        assert!(!s.contains(".wim"));
    }

    #[test]
    fn bare_pxe_kernel_and_initrd_names_are_stable_and_distinct() {
        assert_eq!(BARE_PXE_KERNEL_NAME, "bzImage");
        assert_eq!(BARE_PXE_INITRD_NAME, "rootfs.cpio.gz");
        assert_ne!(BARE_PXE_KERNEL_NAME, BARE_PXE_INITRD_NAME);
    }

    #[test]
    fn winpe_fixture_roots_are_disjoint_children_of_the_privileged_fixture_dir() {
        let plan = BveNetworkPlan::for_bve(&id("bve-winpe-roots"));
        let tftp = winpe_tftp_root(&plan);
        let http = winpe_http_root(&plan);
        assert!(tftp.starts_with(fixture_run_dir(&plan)));
        assert!(http.starts_with(fixture_run_dir(&plan)));
        assert_ne!(tftp, http);
        // Never the non-privileged BVE run dir.
        assert!(!tftp.starts_with(bve_run_dir(&plan)));
    }

    #[test]
    fn bridged_forward_accommodation_is_all_traffic_between_exactly_this_bves_two_ports() {
        let plan = BveNetworkPlan::for_bve(&id("bve-brnf"));
        let other = BveNetworkPlan::for_bve(&id("bve-brnf-other"));
        let [fwd, rev] = bridged_forward_accommodation_rules(&plan);

        for rule in [&fwd, &rev] {
            assert_eq!(rule[0], "FORWARD");
            assert!(rule.contains(&"physdev".to_string()));
            assert_eq!(rule.last().unwrap(), "ACCEPT");
            // No -p / port restriction: DHCP + TFTP (69 + dynamic) + HTTP all pass.
            assert!(!rule.contains(&"-p".to_string()));
            assert!(!rule
                .iter()
                .any(|t| t == "67" || t == "69" || t == "udp" || t == "tcp"));
            // Only this BVE's own TAP and fixture veth are named.
            for forbidden in [
                plan.bridge().as_str(),
                plan.veth_peer().as_str(),
                other.tap().as_str(),
                other.veth_host().as_str(),
                "eth0",
                "docker0",
            ] {
                assert!(
                    !rule.iter().any(|t| t == forbidden),
                    "rule names {forbidden:?}: {rule:?}"
                );
            }
        }
        // Exactly the two directions between tap and veth_host.
        assert!(fwd
            .windows(2)
            .any(|w| w[0] == "--physdev-in" && w[1] == plan.tap().as_str()));
        assert!(fwd
            .windows(2)
            .any(|w| w[0] == "--physdev-out" && w[1] == plan.veth_host().as_str()));
        assert!(rev
            .windows(2)
            .any(|w| w[0] == "--physdev-in" && w[1] == plan.veth_host().as_str()));
        assert_eq!(
            bridged_forward_accommodation_rules(&plan),
            bridged_forward_accommodation_rules(&BveNetworkPlan::for_bve(&id("bve-brnf")))
        );
    }

    #[test]
    fn winpe_fixture_leaves_the_70_dhcp_only_fixture_untouched() {
        // Regression guard: #70's argv must not gain TFTP / arch tags.
        let plan = BveNetworkPlan::for_bve(&id("bve-70-intact"));
        let seventy = fixture_dnsmasq_argv(&plan);
        assert!(seventy.iter().any(|a| a == "--dhcp-boot=bootx64.efi"));
        assert!(!seventy.iter().any(|a| a == "--enable-tftp"));
        assert!(!seventy.iter().any(|a| a.contains("client-arch")));
    }
}
