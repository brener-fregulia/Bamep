# ADR-0024: BVE isolated provisioning network — private bridge + TAP + fixture network namespace

Status: Accepted

## Context

ADR-0022 fixed the initial BVE backend (Linux/QEMU/KVM, driven directly) and
`docs/specifications/m0-bamep-virtual-endpoint-contract.md` deferred "disk
attachment details, TAP/bridge/macvtap, and DHCP" and "PXE implementation and
PXE service topology". Issues #67–#69 deliberately used QEMU user-mode (SLIRP)
networking: enough for VM lifecycle and storage proofs, but SLIRP gives no
Layer-2 fidelity, so guest firmware PXE / DHCP discovery cannot be observed
crossing the NIC boundary.

Issue #70 needs one BVE to reach an isolated, PXE-capable virtual network so a
controlled peer can see the BVE's deterministic MAC and answer it, without
exposing a test DHCP server to the developer's normal LAN, and with a
scoped, fail-closed lifecycle for the host resources involved. This constrains
later UEFI PXE (#71) and Buildroot (#72/#73) work, so the topology is a
durable architectural choice rather than an implementation detail.

Read-only evidence was gathered on the reference environment (WSL2, Ubuntu
24.04, kernel 6.6, iproute2 6.1, QEMU/`qemu-img` 8.2.2, `dnsmasq` 2.90,
`ipxe-qemu` ROMs present):

- creating a host-visible bridge / TAP / veth requires `CAP_NET_ADMIN`;
  unprivileged attempts fail with `EPERM`. `sudo` on this host requires a
  password (no `NOPASSWD`);
- `/dev/net/tun` is world-accessible, so once a TAP exists it can be opened by
  a normal user;
- with a blank system disk the guest firmware runs its virtio PXE option ROM
  and emits `DHCPDISCOVER` from the deterministic MAC (confirmed
  non-destructively via `-object filter-dump` over SLIRP); `-boot order=n`
  makes this explicit;
- `br_netfilter` is loaded and `net.bridge.bridge-nf-call-iptables=1` with a
  Docker-managed `FORWARD` policy present — bridged DHCP frames on a host-ns
  bridge may be filtered.

The **authorised host proof was then executed on this environment** and is
recorded in `docs/reference/bve-isolated-network-host-proof.md`. It confirmed
the topology below works end to end (full DHCP `DISCOVER/OFFER/REQUEST/ACK`
from the deterministic MAC `52:54:00:e5:37:5d`, vendor class
`PXEClient:Arch:00000`, user class `iPXE`), and confirmed that on this host the
bridged IPv4 exchange **does not cross** without a `FORWARD` accommodation
(the frame reaches the TAP but is dropped before the fixture veth; IPv6 from
the same MAC crosses). Two scoped, reversible `FORWARD` ACCEPT rules made the
exchange work; removing them and tearing down left no residual state.

## Decision

The reference BVE provisioning-network topology is **"Design B"**:

```text
QEMU (host network namespace, the same owned std::process::Child as #67)
  └─ -netdev tap → bvtap<h>  ─┐
                              ├─ private bridge bvbr<h>   (no IP, no physical uplink)
               bvh<h> (veth) ─┘        │
                                       └─ bvp<h> (veth peer) → netns bve-<h>
                                                                 ├─ 192.0.2.1/24 (RFC 5737), lo up
                                                                 ├─ no default route, no NAT
                                                                 └─ dnsmasq DHCP/PXE fixture, bound to bvp<h> only
```

- **QEMU stays in the host network namespace.** Process ownership and the QMP
  control socket are exactly as ADR-0022 / Issue #67 established; the BVE
  lifecycle (`start`/`observe`/`reset`/`stop`/`destroy`) is unchanged and
  unprivileged.
- **The private bridge has no physical uplink and no IP.** Its only ports are
  this BVE's TAP and the fixture veth. Setup asserts this invariant and fails
  closed otherwise.
- **The DHCP/PXE fixture lives only inside the dedicated network namespace**,
  bound to the namespace-side veth. It is a disposable validation fixture
  (`dnsmasq`), never production DHCP/PXE architecture, and is not installed
  automatically.
- **Privilege is confined to network preparation, the fixture, and teardown.**
  These are explicit owner-run steps. `bamep-ve` invokes `ip` via argv (never
  a shell string), **never calls `sudo`**, never prompts for a password, never
  edits `/etc/qemu/bridge.conf`, and does not use `qemu-bridge-helper`. Lack
  of `CAP_NET_ADMIN` is an actionable `PrivilegeRequired` error. Once the TAP
  exists (created with `user <uid>`), `qemu-system-x86_64` opens it as the
  normal user.
- **The topology itself performs no firewall / routing / `bridge-nf` /
  sysctl change.** `prepare` never touches `iptables`. On a host where
  `br_netfilter` drops the bridged DHCP exchange (evidenced above), a
  **separate, opt-out DHCP-forward accommodation** applies two `iptables`
  `FORWARD` ACCEPT rules that are:
  - `physdev`-scoped to exactly this BVE's TAP and fixture veth (the private
    bridge has no other port; both interfaces exist only between `prepare` and
    `teardown`, so the rules can match nothing else);
  - limited to UDP DHCP (`--dport 67` client→server, `--sport 67`
    server→client);
  - runtime-only: **never** written to a sysctl, **never** persisted;
  - removed by `remove_dhcp_forward_accommodation` and swept again by
    `teardown` (idempotent), and re-inserted cleanly on a repeat cycle
    (pre-delete before insert).
  A host that does not filter bridged IPv4 does not need this; an inserted
  ACCEPT rule is then inert. It is host-proof scaffolding, not a topology
  element, and it is opt-out (`--no-netfilter-accommodation`).
- **Teardown ordering is fail-closed.** `teardown` refuses while any process
  (the `dnsmasq` fixture) is still running in the netns
  (`FixtureStillRunning`) and while the TAP still has a client
  (`NetworkStillInUse`) — removing an interface under a live `dnsmasq` is what
  produced `error binding DHCP socket to device` in the first proof. The
  required order is: stop the fixture → wait for it to exit → remove the
  netfilter accommodation → remove the network resources.
- **Resource names are derived, deterministic, and bounded.** `<fixed
  prefix><low-32-bits of FNV-1a(BveId) in hex>`, always within `IFNAMSIZ`.
  A name that already exists before setup is a `ResourceAlreadyExists` error —
  it is neither adopted nor deleted.
- **Setup rollback and teardown have exact ownership.** A partial setup rolls
  back only the resources that attempt created, newest-first. Teardown removes
  only the recorded resources (plus the idempotent accommodation sweep) and
  fails closed on an unexpected state. Never a `bv*` scan.
- **A minimal `BootMode { Default, NetworkFirst }`** on the BVE definition
  emits `-boot order=n` for `NetworkFirst` — the minimum to make the firmware
  attempt PXE. It is not a boot-order DSL and does not pull in UEFI/OVMF
  (Issue #71).
- Host-internal virtual-network evidence is **not** physical-network evidence
  (`m0-bamep-virtual-endpoint-contract.md` fidelity boundary).

## Alternatives considered

- **Bridge + TAP + `dnsmasq` directly in the host network namespace
  ("Design A").** Rejected. The DHCP process would run in the normal host
  namespace; isolation would depend entirely on correct interface binding,
  which is weak structural evidence that the fixture cannot reach the LAN.
- **Run QEMU itself inside the network namespace ("Design C").** Rejected as
  the reference. `ip netns exec` preserves the child PID, but every BVE
  `start` would then require privilege, breaking the ADR-0022/Issue #67
  separation of an unprivileged VM lifecycle. Kept as a documented fallback
  only if the host-ns bridge proves unworkable because of `br_netfilter`.
- **`qemu-bridge-helper` / `/etc/qemu/bridge.conf`.** Rejected. Requires a
  global setuid helper and a global allow-list file; the isolated network is
  created and destroyed per proof, not configured permanently.
- **macvtap instead of TAP + bridge.** Rejected for the initial topology. It
  removes the explicit bridge that makes the L2-isolation invariant checkable
  (exactly two named ports) and complicates attaching a second port for the
  fixture veth.
- **libvirt / a managed virtual network.** Rejected, consistent with
  ADR-0022: adds a daemon and abstraction with no second consumer.
- **A generic `NetworkBackend` trait / virtual-switch abstraction.** Rejected.
  There is one isolated network with two modes (user-mode, isolated TAP); a
  small explicit `NetworkAttachment` enum is sufficient.

## Consequences

- `bamep-ve` gains a `network` module (`BveNetworkPlan`, `PreparedBveNetwork`,
  `prepare`/`teardown`/`assert_l2_isolation`, the `dnsmasq` fixture argv
  builders, `apply`/`remove_dhcp_forward_accommodation`, `residual_resources`,
  `BveNetworkError`) and a read-only `check_network_prerequisites`.
- `BveDefinition` gains `NetworkAttachment { UserMode, IsolatedTap { ifname } }`
  (default `UserMode`, unchanged unprivileged path) and
  `BootMode { Default, NetworkFirst }`. A TAP name reaches a definition only
  through `PreparedBveNetwork::attach`.
- `BveRuntime::create_with_isolated_network` cross-checks the definition
  against the prepared network (same lesson as ADR-0023 storage/definition
  consistency); `BveRuntime::create` and every existing caller are unchanged.
- The privileged host proof is the owner-run `scripts/bve-network-proof.sh`
  harness (a versioned dev tool that may call `sudo`; it tracks the real
  `dnsmasq` via its pid-file validated against `ip netns pids`, asserts the
  full DHCP DORA for the deterministic MAC, and restores the host on
  failure/Ctrl-C), backed by the `bve_isolated_net` example's subcommands for
  manual debugging. Not `cargo test` as root — an ordinary `cargo test` never
  mutates host networking. `verify-clean <id>` reports residual resources so a
  reproducibility cycle is a crisp pass/fail.
- On a `br_netfilter`-filtering host, an isolated BVE network additionally
  carries the two `physdev`-scoped `FORWARD` ACCEPT rules described above for
  its lifetime. This does **not** reopen the topology decision (no uplink, no
  route, no NAT, no sysctl); it is a bounded, reversible, opt-out host
  accommodation whose need was confirmed by the authorised proof. Design C
  (QEMU inside the netns) remains the documented fallback only if a future
  host makes even the scoped accommodation unacceptable.
- Later UEFI PXE (#71) reuses this substrate and `BootMode`; it does not
  redefine the topology.

## Related architecture

- `docs/architecture/README.md` — "Bamep Virtual Endpoint host runtime
  (`bamep-ve`)" records the implemented `network` module, topology, resource
  ownership, privilege model, QEMU TAP args, and the host proof.

## Related work

- ADR-0022 — Linux/QEMU/KVM initial BVE backend this network runs on.
- ADR-0023 — deterministic BVE disk storage (the ownership / fail-closed
  pattern this decision mirrors).
- ADR-0021 — iPXE + wimboot network-delivered WinPE baseline (the production
  PXE mechanism this substrate is *not*).
- `docs/reference/bve-isolated-network-host-proof.md` — the executed host-proof
  evidence (DHCP DORA transcript, the `br_netfilter` diagnosis, the scoped
  accommodation, reproducibility).
- `docs/specifications/m0-bamep-virtual-endpoint-contract.md` — backend-
  independent BVE responsibility, lifecycle, and fidelity boundary.
- `docs/reference/physical-uefi-pxe-boot-chain.md` — physical PXE/DHCP
  evidence that remains the authority for real firmware/NIC/switch behavior.
- Issue #70 — Work Package that produced this decision.
- Issues #67–#69 — one-BVE lifecycle, Simulator orchestration, deterministic
  storage.
