# BVE Isolated Provisioning Network — Host-Proof Evidence (Issue #70)

Status: **Completed empirical reference.**

This document preserves the validated evidence from the Issue #70 host proof of
the BVE isolated PXE-capable provisioning network. It does not define normative
behavior: `docs/specifications/m0-bamep-virtual-endpoint-contract.md` owns the
BVE contract, ADR-0024 owns the topology and privilege decision, and
`docs/architecture/README.md` records the implemented `bamep-ve` `network`
module. It also does **not** establish physical PXE/DHCP/NIC/firmware
behavior — that remains the physical Integration Environment's authority
(`docs/reference/physical-uefi-pxe-boot-chain.md`, ADR-0021).

## Environment

- WSL2, Ubuntu 24.04, kernel `6.6.87.2-microsoft-standard-WSL2`.
- `qemu-system-x86_64` / `qemu-img` 8.2.2; `ipxe-qemu` option ROMs present
  (`/usr/lib/ipxe/qemu/pxe-virtio.rom` in QEMU's ROM search path).
- iproute2 6.1.0; `dnsmasq` 2.90; Docker running (`docker0`, veth, and a
  restrictive `FORWARD` policy present).
- `br_netfilter` loaded; `net.bridge.bridge-nf-call-iptables = 1` (Docker
  default — **not** changed by this work).
- `sudo` requires a password (no `NOPASSWD`); privileged steps are owner-run.

## Topology exercised

The ADR-0024 "Design B" layout, for BVE id `bve-net-proof`
(`short hash a6e5375d`):

```text
QEMU (host netns, normal user)  ─ -netdev tap,ifname=bvtapa6e5375d,script=no,downscript=no
  bvtapa6e5375d ─┐
                 ├─ bridge bvbra6e5375d   (no IP, no uplink; L2-isolation assert: ports = {bvtapa6e5375d, bvha6e5375d})
   bvha6e5375d ──┘        │
                          └─ bvpa6e5375d → netns bve-a6e5375d  (192.0.2.1/24, lo up; no default route, no NAT)
                                                └─ dnsmasq, bound to bvpa6e5375d only
```

BVE definition: `BootMode::NetworkFirst` (`-boot order=n`), deterministic MAC
`52:54:00:e5:37:5d`.

## DHCP/PXE exchange observed (fixture log, `--log-dhcp`)

```text
DHCPDISCOVER(bvpa6e5375d) 52:54:00:e5:37:5d
DHCPOFFER(bvpa6e5375d)    192.0.2.59 52:54:00:e5:37:5d
DHCPREQUEST(bvpa6e5375d)  192.0.2.59 52:54:00:e5:37:5d
DHCPACK(bvpa6e5375d)      192.0.2.59 52:54:00:e5:37:5d

vendor class : PXEClient:Arch:00000:UNDI:002001
user class   : iPXE
next server  : 192.0.2.1
bootfile-name: bootx64.efi
```

This establishes, host-internally:

- SeaBIOS ran the virtio PXE option ROM and performed network boot;
- the virtio NIC used the deterministic MAC `52:54:00:e5:37:5d`;
- DHCPv4 left the BVE and crossed, in order: the virtio NIC boundary → the TAP
  → the private bridge → the veth pair → into the isolated netns;
- the fixture observed that exact MAC and completed a full
  `DISCOVER / OFFER / REQUEST / ACK`.

`Arch:00000` = BIOS/x86 (SeaBIOS), as expected — UEFI (`Arch:00007`) is Issue
#71, not exercised here.

## `br_netfilter` interaction and the scoped accommodation

Diagnosis on this host:

- **Without** any `FORWARD` accommodation: the client's DHCPv4 `68→67`
  broadcast is visible on `bvtapa6e5375d` but **does not reach**
  `bvpa6e5375d`; IPv6 (link-local, same MAC) *does* traverse to the peer.
  Cause: `br_netfilter` + `bridge-nf-call-iptables=1` sends the bridged IPv4
  frame through the iptables `FORWARD` chain, where the host's restrictive
  (Docker-managed) policy drops it.
- **With** two `FORWARD` ACCEPT rules scoped by `physdev` to exactly
  `bvtapa6e5375d ⇄ bvha6e5375d` and to UDP port 67, the full handshake above
  works.

Adopted mechanism (ADR-0024, implemented as
`network::apply_dhcp_forward_accommodation` /
`remove_dhcp_forward_accommodation`):

```text
iptables -w -I FORWARD -m physdev --physdev-in bvtapa6e5375d --physdev-out bvha6e5375d -p udp --dport 67 -j ACCEPT
iptables -w -I FORWARD -m physdev --physdev-in bvha6e5375d  --physdev-out bvtapa6e5375d -p udp --sport 67 -j ACCEPT
```

Properties: `physdev`-scoped to the two BVE-owned ports (which exist only
between `setup` and `teardown`); UDP-67 only; runtime-only (no sysctl, not
persisted); removed by `teardown` (idempotent, with a pre-delete before insert
so repeat cycles do not accumulate). A host that does not filter bridged IPv4
does not need it (an inserted ACCEPT rule is then inert); it is opt-out with
`--no-netfilter-accommodation`.

This does **not** change the topology: still no uplink, no route, no NAT, and
`net.bridge.bridge-nf-call-iptables` is left exactly as found.

## Shutdown-ordering finding

Removing `bvpa6e5375d` / the netns while `dnsmasq` was still running produced:

```text
dnsmasq: error binding DHCP socket to device bvpa6e5375d
```

Fix: `network::teardown` now refuses while any process runs inside the netns
(`FixtureStillRunning`) — the required order is **stop fixture → wait for exit
→ remove accommodation → teardown network**. The `bve_isolated_net` example
enforces and documents this.

## Cleanup / reproducibility

Teardown removed exactly:

```text
Netns  bve-a6e5375d
Veth   bvha6e5375d
Tap    bvtapa6e5375d
Bridge bvbra6e5375d
```

plus both accommodation rules; `ip link` / `ip netns list` / `iptables -S
FORWARD` showed no residual BVE state afterward.

A first manual attempt failed to stop the fixture because
`sudo … start-fixture … &` captured the `sudo` wrapper PID (36137), not the
real `dnsmasq` (36141) inside the netns; `teardown` then correctly refused
(`FixtureStillRunning`). The fix is the versioned harness
`scripts/bve-network-proof.sh`, which drives the whole cycle, locates the real
`dnsmasq` via `bamep_ve::fixture_pid_file` validated against
`ip netns pids <netns>`, `SIGTERM`s it, waits for the netns to be
process-free, then tears down — with a `trap` that best-effort restores the
host on Ctrl-C / mid-failure, scoped to the proof's BVE id only. Its expected
short output:

```text
check clean ........ ok
setup .............. ok
fixture ............ ok
BVE PXE ............ PASS
  DHCPDISCOVER
  DHCPOFFER
  DHCPREQUEST
  DHCPACK
fixture stop ....... ok
teardown ........... ok
host clean .......... yes
```

Repeated runs reproduce with no accumulated state (resource names are
deterministic; the accommodation insert pre-deletes; teardown is idempotent).
`bve_isolated_net verify-clean <id>` is the pass/fail check between cycles.

## What this does and does not establish

Establishes (host-internal, virtualized): virtio NIC PXE behavior, guest
firmware DHCP discovery across the NIC boundary, TAP, Linux bridge, veth, L2
broadcast into an isolated namespace, a controlled DHCP/PXE exchange, the BVE
deterministic MAC, and host-internal isolation with a bounded reversible
netfilter accommodation.

Does **not** establish: motherboard PXE firmware, physical NIC option
ROM/driver/PHY behavior, switch/VLAN behavior, real DHCP coexistence, Secure
Boot, UEFI PXE (`Arch:00007`), or throughput. Those remain the physical
Integration Environment's authority.

## Related

- ADR-0024 — isolated BVE provisioning network topology and privilege model.
- `docs/specifications/m0-bamep-virtual-endpoint-contract.md` — BVE contract.
- `docs/architecture/README.md` — implemented `bamep-ve` `network` module.
- `docs/reference/physical-uefi-pxe-boot-chain.md` — physical PXE/DHCP evidence.
- Issue #70 — the Work Package this proof belongs to.
