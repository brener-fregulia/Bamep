# BVE WinPE UEFI-PXE — Host-Proof Reference (Issue #71)

Status: **Design/wiring reference — empirical transcript PENDING the owner-run
proof.**

This document describes the implemented Issue #71 host-proof wiring and what a
successful run establishes and does not establish. The pass/fail transcript,
observed DHCP/TFTP/HTTP evidence, and any host-specific findings are filled in
**after** `scripts/bve-winpe-pxe-proof.sh` is actually executed by the owner —
only executed validation produces validation evidence
(`docs/development/testing.md`).

Normative ownership is unchanged:
`docs/specifications/m0-bamep-virtual-endpoint-contract.md` owns the BVE
contract; ADR-0025 owns the virtual UEFI firmware decision; ADR-0024 (with its
Issue #71 amendment) owns the isolated network and the broadened bridged-forward
accommodation; ADR-0021 owns the iPXE + wimboot WinPE Boot Adapter mechanism.
Physical PXE/DHCP/NIC/firmware/Secure-Boot behavior remains the physical
Integration Environment's authority
(`docs/reference/physical-secure-boot-winpe-network-delivery.md`).

## Reference environment

- WSL2, Ubuntu 24.04; QEMU / `qemu-img` 8.2.2; KVM present; `ovmf` 2024.02
  (`/usr/share/OVMF/OVMF_CODE_4M.fd` + `OVMF_VARS_4M.fd`); `dnsmasq` 2.90;
  `python3` 3.12; `br_netfilter` loaded with a Docker-managed restrictive
  `FORWARD` policy.

## Topology and profile

The ADR-0024 "Design B" isolated network (private bridge + user-owned TAP +
veth + dedicated netns), plus:

- **Firmware:** `Firmware::Uefi` — OVMF **non**-Secure-Boot. `OVMF_CODE_4M.fd`
  read-only `if=pflash,unit=0`; a per-BVE writable copy of `OVMF_VARS_4M.fd` at
  `<control-dir>/OVMF_VARS.fd`, `if=pflash,unit=1`, written once at `create`,
  reused across `stop`/`start`, removed at `destroy` (ADR-0025).
- **NIC:** `NicModel::E1000` (Intel 82540EM, `8086:100E`). The retained stock
  WinPE `boot.wim` (10.0.26100) has an inbox driver for it (`nete1g3e.inf`) and
  **no** VirtIO/NetKVM driver (read-only inspection, Issue #71).
- **Boot:** `BootMode::NetworkFirst` (`-boot order=n`). The system disk is a
  blank QCOW2 overlay over a blank base; **no** source disk, **no** `-cdrom`,
  **no** optical/El-Torito device — the only path to WinPE is the NIC
  (anti-false-positive, Issue #71 / the VirtualBox spike's silent-ISO-fallback
  lesson).
- **Netfilter:** `apply_bridged_forward_accommodation` — two `physdev`-scoped
  `FORWARD` ACCEPT rules for **all** traffic between exactly this BVE's TAP and
  its fixture veth (both directions), because the chain carries DHCP + TFTP
  (UDP/69 + dynamic) + HTTP. Reversible; swept by `teardown`; opt-out on hosts
  that do not filter bridged traffic.

## Boot chain under test

```text
OVMF UEFI (VARS pristine, -boot order=n)
→ e1000 EFI option ROM (iPXE) executed by OVMF          ← see "Fidelity" below
→ DHCP  (vendor class PXEClient:Arch:00007, option 93 = 7)
→ [ TFTP snponly.efi ]   (only if the option ROM presents as non-iPXE)
→ iPXE second DHCP (user-class iPXE / option 175)  →  HTTP GET /boot.ipxe
→ HTTP  wimboot v2.9.0
→ HTTP  pristine BCD + boot.sdi + stock boot.wim (WinPE 10.0.26100)
→ Windows Boot Manager
→ WinPE  (startnet.cmd → wpeinit → NIC driver → DHCP client)
→ second DHCP DORA: vendor class "MSFT 5.0", hostname "MININT-*"   ← "WinPE ready"
```

This is the ADR-0021 mechanism family **minus** the physical Secure-Boot
wrapper — no `shim` / `snponly-shim.efi`, no root-level `/ipxe.efi` fallback,
non-Secure-Boot OVMF. The `wimboot → BCD/boot.sdi/boot.wim → WinPE` core is the
exact retained artifact set from the Issue #53 physical chain
(`scripts/winpe-pxe-fixture.provenance.md`).

## Acceptance signal — "WinPE ready" (approved for Issue #71)

> The network-delivered stock WinPE has progressed into its automatic
> Windows/WinPE initialization far enough to bring up its guest network stack
> and complete a distinct DHCP exchange identified as the Windows client
> (`MSFT 5.0` and/or the corresponding `MININT-*` evidence) from the
> deterministic BVE MAC.

This:

- proves functional headless initialization relevant to the future Agent (the
  WinPE participant will talk over this same network);
- does **not** prove an interactive `X:\Windows\System32>` shell is usable;
- does **not** prove a visual console (that is Issue #74);
- does **not** modify `boot.wim`.

The repeatability requirement is two accepted boots of the **same**
`BveRuntime` / definition / storage / OVMF VARS (`start` → WinPE ready →
`stop`, twice, then `destroy`), with no hidden recreate between boots. It is
proven by requiring `winpe_dhcp_ack_in_range` to hold **separately** in boot
#1's and boot #2's fixture-log line range.

## Boot-stage evidence

`scripts/bve-winpe-pxe-proof.sh` parses **only the real fixture log** (dnsmasq
`--log-dhcp --log-queries` + `python3 -m http.server`) — never `run-bve`'s
stdout — and reports which stage was last proven; a stage failure names the
stage, not a generic timeout:

```text
UEFI PXE · arch EFIx64 · TFTP bootstrap · iPXE · wimboot · BCD · boot.sdi ·
boot.wim (transfer started) · WinPE ready #1 · WinPE ready #2 · BVE reboot
```

- The pre-WinPE chain (UEFI PXE / arch / TFTP / iPXE / HTTP GETs) is a
  whole-log "did this happen" check. HTTP `200`/`206` matching is done by
  `http_200` / `http_transfer_started` in `scripts/lib/winpe-pxe-evidence.sh`
  (the `.` in `boot.sdi` is matched literally; `HTTP/1.0` and `HTTP/1.1` both
  count).
- `TFTP snponly.efi` is `N/A` (not a failure) when the e1000 EFI option ROM
  entered iPXE directly.
- `boot.wim` is *transfer started* on the HTTP `200`/`206`; a full 340 MB
  delivery is evidenced **downstream** by reaching `WinPE ready`, not by the
  status line.
- **`WinPE ready` is proven per boot.** `run-bve` (given `--evidence-log
  <fixture log>`) emits the fixture-log line ranges bounding boot #1 and
  boot #2 — it does not interpret DHCP. Inside each range,
  `winpe_dhcp_ack_in_range` requires a Windows-client DHCP transaction
  (vendor class `MSFT 5.0` / a `minint` client name) that **reaches DHCPACK for
  the BVE MAC**, at a log line at or after the identity line (the PXE/iPXE ACKs
  for the same MAC come first). `BVE reboot` PASS ⇔ both per-boot ranges prove
  it independently — a whole-log text count is not accepted, because one WinPE
  DORA produces several matching lines.

The parser helpers are unit-tested with canonical log fixtures by
`scripts/bve-winpe-pxe-proof-parser-test.sh` (no sudo / QEMU / network).

## Fidelity limits

- Virtual OVMF ≠ physical OEM UEFI firmware.
- **UEFI network boot is initiated through the e1000 EFI option ROM (iPXE)
  executed by OVMF — not OVMF's own `VirtioNetDxe` PXE/SNP path.** The observed
  network bootstrap implementation is iPXE.
- `e1000` emulation ≠ a physical NIC's option ROM / UNDI / PHY.
- **Secure Boot is disabled** — no enforcement, `db`/`dbx`, shim, or revocation
  behavior is exercised or claimed.
- "WinPE ready" = automatic initialization reached, not interactive-shell
  usability.
- 340 MB `boot.wim` over the isolated virtual network ≠ physical PXE throughput;
  no throughput is measured.
- Host-internal virtual-network evidence is not physical-network evidence.

## Evidence

**PENDING** — to be filled from the owner's `scripts/bve-winpe-pxe-proof.sh`
run: environment confirmation, the per-stage PASS/FAIL/N-A table for boot #1 and
boot #2, the observed DHCP/TFTP/HTTP transcript excerpts (deterministic MAC, the
`MSFT 5.0` / `MININT-*` DORA), the staged-artifact SHA-256 confirmation,
whether the e1000 option ROM entered iPXE directly, any `br_netfilter`-specific
finding, and the clean-teardown / reproducibility result.

## Related

- ADR-0025 — BVE virtual UEFI firmware (OVMF non-Secure-Boot, per-BVE VARS).
- ADR-0024 — isolated BVE provisioning network + the Issue #71 bridged-forward
  amendment.
- ADR-0021 — iPXE + wimboot network-delivered WinPE Boot Adapter baseline.
- `docs/reference/bve-isolated-network-host-proof.md` — the Issue #70 proof this
  builds on.
- `docs/reference/physical-secure-boot-winpe-network-delivery.md` — the physical
  chain whose retained artifacts this reuses.
- `docs/reference/winpe-boot-mechanism-spike.md` — prior virtualized WinPE
  evidence (stock WinPE recognising the emulated Intel 82540EM).
- `scripts/winpe-pxe-fixture.provenance.md` — artifact provenance and hashes.
- Issue #71 — the Work Package this proof belongs to.
