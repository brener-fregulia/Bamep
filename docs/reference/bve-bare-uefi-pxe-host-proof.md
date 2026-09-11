# BVE BARE UEFI-PXE — Host-Proof Reference (Issue #73)

Status: **Validated — owner-run two-boot UEFI-PXE proof passed
(2026-09-11, WSL2).**

This document describes the implemented Issue #73 host-proof wiring and what a
successful run establishes and does not establish. The owner executed
`scripts/bve-bare-pxe-proof.sh` on 2026-09-11; both boots passed every required
provisioning and guest-readiness stage, and teardown left the host clean.
Only executed validation produces validation evidence
(`docs/development/testing.md`).

Normative ownership is unchanged: `docs/specifications/m0-bamep-virtual-endpoint-contract.md`
owns the BVE contract; ADR-0025 owns the virtual UEFI firmware decision;
ADR-0024 (with its Issue #71 amendment) owns the isolated network and the
bridged-forward accommodation; ADR-0026 owns the BARE baseline (Buildroot
external tree, kernel/initramfs artifact shape) and already anticipates this
proof reusing `bzImage`/`rootfs.cpio.gz` with iPXE `kernel`/`initrd`
unchanged. This document does not reopen any of those decisions and no new
ADR was created for #73 — it is a consequence of ADR-0024 + ADR-0025 +
ADR-0026, not a new architectural choice.

## Spike precedent

Before implementation, a minimal, non-versioned empirical spike (owner-run,
outside the repo's proof scripts) exercised the exact chain below by hand —
`Firmware::Uefi` + `NicModel::VirtioNetPci` + `BootMode::NetworkFirst` on the
isolated #70 network, the retained Issue #71 `snponly.efi` 2.0.0, and the
already-built BARE `bzImage`/`rootfs.cpio.gz` — and observed it reach
`BARE READY` / `BARE NET_READY` once. That spike is exploratory precedent for
this design, not a substitute for the formal two-boot proof recorded here: it
used ad-hoc tooling, held no repeatability guarantee, and its host-side HTTP
evidence collection had a known false-negative gap (see "Evidence model"
below) that this proof's `start-fixture` design specifically closes. The
spike confirmed one fact that shaped this implementation: **the BARE kernel
already built in this repository (`CONFIG_EFI=y`, `CONFIG_EFI_STUB=y`, and
every VirtIO/serial/initrd symbol `linux.fragment` already forces) crosses
iPXE's UEFI EFI-stub + EFI-initrd handoff unchanged — no `linux.fragment`,
defconfig, or Buildroot change was made or is needed for #73.**

## What #73 is

Boot the existing BARE `bzImage` + `rootfs.cpio.gz` (Issue #72 / ADR-0026)
through the same isolated BVE UEFI PXE mechanism already implemented for
WinPE (Issue #71 / ADR-0021 / ADR-0025), and observe BARE reach the same
readiness contract already defined by #72 (`BARE_READY` / `BARE_NET_READY`) —
this time delivered over the network instead of `DirectKernelBoot`. It does
**not** implement the production Agent, physical MiniPC PXE, Secure Boot, or
broad hardware compatibility (tracked separately by #75).

## Reference environment

- WSL2, Ubuntu 24.04; QEMU / `qemu-img` 8.2.2; KVM present; `ovmf` 2024.02
  (`/usr/share/OVMF/OVMF_CODE_4M.fd` + `OVMF_VARS_4M.fd`); `dnsmasq` 2.90;
  `python3` 3.12.

## Topology and profile

The ADR-0024 "Design B" isolated network (private bridge + user-owned TAP +
veth + dedicated netns), reused byte-for-byte from #70/#71, plus:

- **Firmware:** `Firmware::Uefi` — OVMF **non**-Secure-Boot, unchanged from
  #71 (ADR-0025): `OVMF_CODE_4M.fd` read-only `if=pflash,unit=0`; a per-BVE
  writable copy of `OVMF_VARS_4M.fd`, written once at `create`, reused across
  `stop`/`start`, removed at `destroy`.
- **NIC:** `NicModel::VirtioNetPci` — **not** #71's `E1000`. ADR-0025 already
  anticipated this ("a future UEFI BVE (e.g. BARE/Buildroot) may use
  `VirtioNetPci`"): OVMF's native `VirtioNetDxe` UEFI driver does the PXE
  DHCP/TFTP stage with no option ROM, and BARE's kernel only carries a
  built-in `virtio_net` driver (no `e1000` driver in `linux.fragment`) — using
  `E1000` here would bootstrap firmware-side but never reach
  `BARE_NET_READY`. This keeps the same virtual hardware profile #72 already
  validated.
- **Boot:** `BootMode::NetworkFirst` (`-boot order=n`). The system disk is a
  blank QCOW2 overlay over a blank base; **no** source disk, **no** `-cdrom`,
  **no** optical/El-Torito device, **no** `DirectKernelBoot` — the only path
  to BARE is the NIC (same anti-false-positive guard #71 used for WinPE).
- **Serial capture:** `BveRuntime::with_serial_capture` (Issue #72),
  append-mode `<control-dir>/serial.log` — new for a network-boot profile,
  reused unchanged from #72's mechanism.
- **Netfilter:** the same `apply_bridged_forward_accommodation` #71 already
  applies (unchanged) — DHCP + TFTP + HTTP across the bridge on a
  `br_netfilter`-filtering host.

## Boot chain under test

```text
OVMF UEFI (VARS pristine, -boot order=n)
→ VirtioNetDxe (OVMF's native UEFI PXE driver — no option ROM)
→ DHCP  (vendor class PXEClient:Arch:00007:UNDI:003016, option 93 = 7)
→ TFTP snponly.efi 2.0.0          (the exact Issue #71 artifact, reused)
→ iPXE second DHCP (user-class iPXE / option 175)  →  HTTP GET /boot.ipxe
→ HTTP  bzImage                    (BARE's Linux 7.1.13 kernel, EFI-stub)
→ HTTP  rootfs.cpio.gz             (BARE's standalone gzip initramfs)
→ Linux EFI stub → EFI initrd handoff → BusyBox init
→ BARE READY  nic=<iface> block=<dev> block_sectors=<n>
→ BARE NET_READY  nic=<iface> addr=<ipv4>
```

This reuses the ADR-0021/ADR-0025 UEFI/iPXE bootstrap mechanism unchanged —
only the served `boot.ipxe` payload and the two staged files differ from
#71's `wimboot` + WinPE asset chain (`bamep_ve::bare_boot_ipxe_script`,
`crates/ve/src/network.rs`). `imgfree` at the top of the script discards any
image a previous boot of the same BVE left registered; `panic=0` is
deliberately fail-closed — a panicked BARE kernel halts rather than silently
rebooting into a second, evidence-polluting PXE attempt.

## Readiness contract (unchanged from #72)

`BARE_READY` / `BARE_NET_READY` keep exactly the semantics ADR-0026 and
`docs/reference/bve-bare-direct-boot-host-proof.md` already define — #73
changes nothing about what the markers mean, only how BARE arrived at the
state that lets `S50bare` emit them.

## Evidence model

Two **separate** authorities, correlated only by per-boot line ranges — never
merged, never substituted for one another:

```text
host-side provisioning evidence          guest-side evidence
    dnsmasq (--log-dhcp --log-queries)        BVE serial.log (append mode,
    + python3 -m http.server access log       Issue #72's mechanism)
    — BOTH captured into ONE fixture log
    (start-fixture spawns both as children
    of one process; the orchestrating shell
    script's single redirection captures
    both — the same mechanism #71 already
    relies on for its wimboot/BCD/boot.sdi/
    boot.wim HTTP evidence)
```

The pre-implementation spike's ad-hoc harness trusted the BVE guest serial
console's own iPXE progress messages (`http://.../bzImage... ok`) as evidence
that the **host** had transferred the file — a category error: the guest's
own report of its download is not host-side proof, and the spike's separate
quick-check script that tried to grep the fixture log for those HTTP GETs
missed them because it captured `python3 -m http.server`'s access log into a
different, unconsolidated stream than the one it later read. This proof
closes that gap structurally: `start-fixture` spawns `dnsmasq` and
`python3 -m http.server` as children of the same process, inheriting its
stdout/stderr, so the **one** fixture-log redirection the orchestrating
`bve-bare-pxe-proof.sh` sets up captures both — the same pattern #71 already
uses successfully for `boot.wim`/`BCD`/`wimboot` HTTP evidence. The guest
serial console is never accepted as sufficient evidence of a host-side HTTP
transfer.

For **each** boot, independently, in its own line range:

```text
UEFI PXE attempt                    fixture log — DHCPDISCOVER tagged efi-x64
DHCP/bootstrap                      fixture log — DHCPACK for the BVE MAC
TFTP snponly.efi                    fixture log — dnsmasq TFTP send (or N/A if
                                     the NIC's firmware entered iPXE directly)
iPXE / boot.ipxe select             fixture log — user-class iPXE / GET /boot.ipxe
HTTP GET /boot.ipxe 200             fixture log — python http.server access log
HTTP GET /bzImage 200               fixture log — python http.server access log
HTTP GET /rootfs.cpio.gz 200        fixture log — python http.server access log
BARE_READY                          serial log
BARE_NET_READY                      serial log
```

`scripts/lib/bare-pxe-evidence.sh` implements the fixture-log helpers
(`uefi_pxe_attempt_in_range`, `dhcp_ack_in_range`, `snponly_tftp_in_range`,
`ipxe_boot_script_selected_in_range`, `http_get_200_in_range` — the `.` in a
path is matched literally, a 404 never counts, a request outside the given
range never counts). `scripts/lib/bare-serial-evidence.sh` (Issue #72) is
reused **unchanged** for `BARE_READY`/`BARE_NET_READY` — never
reimplemented. Every helper fails closed on a non-numeric or reversed/empty
range. `crates/ve/examples/bve_bare_pxe.rs`'s `run-bve` emits **two** pairs of
per-boot line ranges — `BVE_BOOT{1,2}_FIXTURE_RANGE` and
`BVE_BOOT{1,2}_SERIAL_RANGE` — from the two independent logs; it interprets
neither. `scripts/bve-bare-pxe-proof-parser-test.sh` unit-tests every helper
plus a local (test-only, never shipped into either library) composite that
ANDs both authorities together, with canonical two-boot fixtures — no sudo,
no QEMU, no network.

"BVE reboot" (repeat PXE path) is PASS iff every stage above is proven
**independently** inside **each** boot's own ranges — a boot #1 line can
never satisfy boot #2's proof, the same discipline #71/#72 already apply.

## Repeatability

Same `BveRuntime` / `BveDefinition` / storage / `bzImage` / `rootfs.cpio.gz` /
isolated network / staged PXE fixture: `start` → UEFI PXE → `BARE_READY` →
`BARE_NET_READY` → `stop`, twice, then `destroy` + `destroy_instance_storage`
+ fixture stop + network teardown + `verify-clean`. No hidden VM/storage/VARS
recreation between boots.

## Proof (`scripts/bve-bare-pxe-proof.sh`)

Normal user; calls `sudo` itself for `setup`/`start-fixture`/`teardown`.
Never builds or rebuilds BARE — a missing `bzImage`/`rootfs.cpio.gz` fails
clearly and names `scripts/build-bare.sh`. Never downloads `snponly.efi` — a
missing/mismatched artifact is an actionable error; it reuses the exact
Issue #71 artifact (see "snponly.efi authority" below).

```text
host + artifacts  →  host clean  →  setup (sudo)  →  start-fixture (sudo) →
run-bve x2 (serial capture on; emits fixture + serial ranges per boot)  →
per-boot stage parsing (both authorities, independently)  →
fixture stop  →  teardown (sudo)  →  verify-clean + zero residual iptables
```

## snponly.efi authority

The Issue #73 spike confirmed the same `snponly.efi` 2.0.0 (iPXE SNP-only
EFI, non-Secure-Boot) Issue #71 already qualified also bootstraps BARE — it
is generic UEFI/iPXE bootstrap, not WinPE-specific. Its identity (size,
SHA-256, provenance) keeps exactly **one** authoritative source:
`scripts/winpe-pxe-fixture.sha256` / `scripts/winpe-pxe-fixture.provenance.md`
(Issue #71) — #73 was deliberately **not** given a second, duplicate manifest
for the same fact. `crates/ve/examples/bve_bare_pxe.rs` reads that manifest
directly (`expected_snponly_sha256`) rather than re-pinning the hash, and
resolves the artifact's path from `--snponly` / `BAMEP_BVE_BARE_PXE_SNPONLY`,
falling back to an already-staged `BAMEP_BVE_WINPE_FIXTURE_ROOT` from #71 (the
same qualified file) — never a download.

A full extraction of a generic, shared `uefi-ipxe-bootstrap` fixture-artifact
authority (used by both #71 and #73) was considered and deliberately
**deferred**: it would require changing #71's own `bve_winpe_pxe.rs`
artifact-manifest handling (currently a single-manifest,
WinPE-fixture-rooted design) to accept a second, generic manifest — a
redesign of a proof #71 already validated, not a small addition. Reading
#71's existing manifest from #73 (above) captures the "one fact, one
authoritative source" requirement without that redesign. This is recorded
debt: if a third UEFI/iPXE bootstrap consumer appears, extracting a shared
`scripts/uefi-ipxe-bootstrap.sha256` (+ a small `bve_winpe_pxe.rs` /
`bve_bare_pxe.rs` change to read it instead) becomes worth its diff.

## Fidelity limits

**#73 proves BARE can boot through the BVE's isolated UEFI PXE path and reach
its defined readiness state** — nothing more.

- Virtual OVMF ≠ physical OEM UEFI firmware.
- `virtio-net`/`virtio-blk` under QEMU ≠ a physical NIC or storage controller,
  its option ROM, driver, or PHY.
- **Secure Boot is disabled** — no enforcement, `db`/`dbx`, shim, or
  revocation behavior is exercised or claimed.
- Host-internal isolated-virtual-network DHCP/TFTP/HTTP ≠ production
  DHCP/PXE service behavior or topology.
- Does **not** prove physical MiniPC compatibility, physical NIC/storage
  compatibility, broad hardware compatibility (tracked by
  `#75 — [Discovery] Define BARE production hardware compatibility and
  unsupported-endpoint handling`), or the production Agent (BARE starts no
  Agent; `S50bare` idles after emitting its markers).
- BVE virtual-firmware/network evidence is not physical OEM UEFI-firmware or
  physical-network evidence (`m0-bamep-virtual-endpoint-contract.md`
  fidelity boundary).

## Evidence

Owner-run validation completed on **2026-09-11** in the reference WSL2
environment documented above.

The proof used the retained Issue #71 `snponly.efi` 2.0.0 through the explicit
TFTP bootstrap path in **both** boots; the TFTP stage was therefore proven and
was not the option-ROM `N/A` case.

Observed result:

```text
host + artifacts ..... ok
host clean ........... ok
isolated network ..... ok
PXE fixture .......... ok
BVE x2 (UEFI PXE) .... ok

boot #1
  UEFI PXE attempt ........ PASS
  DHCP/bootstrap .......... PASS
  snponly.efi (TFTP) ...... PASS
  iPXE / boot.ipxe select . PASS
  boot.ipxe (HTTP 200) .... PASS
  bzImage (HTTP 200) ...... PASS
  rootfs.cpio.gz (HTTP 200) PASS
  BARE_READY .............. PASS
  BARE_NET_READY .......... PASS

boot #2
  UEFI PXE attempt ........ PASS
  DHCP/bootstrap .......... PASS
  snponly.efi (TFTP) ...... PASS
  iPXE / boot.ipxe select . PASS
  boot.ipxe (HTTP 200) .... PASS
  bzImage (HTTP 200) ...... PASS
  rootfs.cpio.gz (HTTP 200) PASS
  BARE_READY .............. PASS
  BARE_NET_READY .......... PASS

repeat PXE path (BVE reboot) . PASS
fixture stop ................. ok
teardown ..................... ok
host clean ................... yes
```

The host-side fixture evidence independently contained successful HTTP 200
requests for `boot.ipxe`, `bzImage`, and `rootfs.cpio.gz` inside each boot's
fixture range. The guest-side serial evidence independently contained
`BARE_READY` and `BARE_NET_READY` inside each corresponding serial range.

The second boot reused the same BVE runtime/definition, prepared storage,
per-BVE OVMF VARS, BARE artifacts, isolated provisioning network, and staged
PXE fixture. No VM, storage, firmware state, or BARE artifact was reconstructed
between boots.

After the second boot the fixture stopped successfully, network teardown
completed, `verify-clean` passed, and no BVE-scoped residual FORWARD rule
remained.

## Related

- ADR-0026 — BARE baseline (kernel/initramfs artifact shape this proof
  consumes unchanged; already anticipated #73's iPXE `kernel`/`initrd` reuse).
- ADR-0025 — BVE virtual UEFI firmware (OVMF non-Secure-Boot, per-BVE VARS;
  already anticipated `VirtioNetPci` for a future BARE/Buildroot UEFI BVE).
- ADR-0024 — isolated BVE provisioning network + the Issue #71 bridged-forward
  amendment.
- ADR-0021 — iPXE + wimboot network-delivered WinPE Boot Adapter baseline
  (the UEFI/iPXE bootstrap mechanism family this proof reuses, unchanged).
- `docs/reference/bve-winpe-uefi-pxe-host-proof.md` — the Issue #71 proof this
  reuses the bootstrap mechanism and `snponly.efi` authority from.
- `docs/reference/bve-bare-direct-boot-host-proof.md` — the Issue #72 proof
  this reuses the BARE artifacts and readiness contract from (direct-boot
  remains available as a diagnostic baseline; #73 does not change it).
- `scripts/winpe-pxe-fixture.provenance.md` — `snponly.efi` provenance and
  hash (the one authoritative source #73 reads, never duplicates).
- Issue #73 — the Work Package this proof belongs to.
- Issue #75 — broad BARE production hardware compatibility (explicitly out of
  scope here).
