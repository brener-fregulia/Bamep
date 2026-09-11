# BVE Optional Visual Display (VNC) — Host-Proof Reference (Issue #74)

Status: **Validated — owner-run manual VNC observation proof passed
(2026-09-11, WSL2).**

This document records the implemented Issue #74 wiring and the empirical result of
the owner-run manual validation: connecting a VNC viewer to a running BVE, observing
BARE's boot output, and confirming visual-client disconnect/reconnect is independent
from BVE lifecycle. It is the authority for that empirical result;
`docs/architecture/README.md` references it rather than duplicating it.

Normative ownership: `docs/specifications/m0-bamep-virtual-endpoint-contract.md` (BVE
contract and fidelity boundary — "port allocation, console mechanism, and backend
process/control-supervision implementation details" are explicitly out of scope for
that Specification, so this capability is an implementation detail, not a new
normative contract); Issue #74 (work history). No new ADR was opened for #74: the
implementation is confined to `bamep-ve`'s existing QEMU-argv/runtime boundary and
introduces no durable architectural decision beyond what ADR-0022 already settled
(QEMU driven directly, no libvirt).

## What this proves

An **optional, local-only, opt-in** visual observation path for a BVE: connecting a
VNC viewer to a deterministic loopback endpoint and watching the guest's console,
without changing BVE lifecycle, storage, networking, firmware, QMP, or serial-capture
behavior. It does **not** prove a production remote console, a browser/Web console,
authentication, physical Endpoint behavior, GPU/display fidelity, or Agent behavior —
see "Fidelity limits" below.

## Implementation summary

- `bamep_ve::VncEndpoint` deterministically derives a `(display, port)` pair
  from `fnv1a_64(BveId)` — modulo the number of available displays
  (`port = 5900 + display`). This is a different derivation than
  `BveNetworkPlan`'s hashed host-resource names (which format the hash's low
  32 bits as hex), but the same guarantee: the same id always derives the
  same endpoint. No random-port race; no arbitrary QEMU display escape hatch.
- `BveRuntime::with_visual_display()` opts one BVE in (off by default, mirrors
  `with_serial_capture`). `BveRuntime::start` validates the derived port is free
  (`check_vnc_endpoint_available`, a bind-and-drop probe) **before** spawning QEMU,
  failing closed with `RuntimeError::VncEndpointUnavailable` on a collision.
- `QemuCommand::for_bve` emits exactly one additional `-vnc 127.0.0.1:<display>` when
  a visual display was requested; `-display none` is always present and unchanged
  otherwise. Bind address is always `127.0.0.1` — never `0.0.0.0`/`::`/a LAN address.
  No SPICE, no GTK/SDL window, no authentication/RBAC/proxy surface (none of that is
  in scope for #74).
- `crates/ve/examples/bve_bare` gained a `--visual` flag for the manual proof: one
  boot, VNC enabled, held running until the owner presses Enter (rather than the
  fixed two-boot timed hold used by the existing `#72` proof). Without an explicit
  `--append`, `--visual` automatically adds a local VGA console
  (`console=ttyS0,115200 panic=-1 console=tty0`) on top of the existing serial
  console, so BARE's boot output is visible over VNC with no hidden knowledge
  required; an explicit `--append` still wins unchanged in both modes.

## Reference environment

- WSL2 Ubuntu; QEMU/`qemu-system-x86_64` 8.2.2; the same BARE artifacts already
  validated by Issue #72/#73 (`docs/reference/bve-bare-direct-boot-host-proof.md`).
- Owner's VNC viewer: **TigerVNC Viewer**, run from Windows, connecting through
  WSL2's default `localhost` forwarding — no VNC server/viewer software is installed
  automatically by this crate or its examples.

## Manual proof procedure (owner-run)

```bash
cargo run -p bamep-ve --example bve_bare -- run-bve <bve-id> \
  --kernel <path to bzImage> --initrd <path to rootfs.cpio.gz> \
  --visual
```

No `--append` is needed — the local console is added automatically for `--visual`.

## Evidence (owner-run, 2026-09-11, WSL2 Ubuntu, QEMU/KVM 8.2.2)

The repository owner ran the command above against the existing BARE artifacts and
reported the following (recorded here as the owner's own report, per
`AGENTS.md` §"Validation integrity" — this was not run or observed by the assistant):

- Runtime output:

  ```text
  BVE_VNC_ENDPOINT=127.0.0.1:22018 (QEMU display :16118)
  BVE running with visual display enabled - press Enter here to stop it.
  ```

- The owner connected **TigerVNC Viewer** (Windows) to `127.0.0.1:22018` and
  observed the BARE/Linux boot on the visual console, reaching the same #72/#73
  readiness markers over the visible console:

  ```text
  BARE READY nic=eth0 block=vda block_sectors=131072
  BARE NET_READY nic=eth0 addr=10.0.2.15
  ```

- **Lifecycle-independence check**: the owner (1) closed TigerVNC Viewer, (2) left
  the BVE/QEMU process running, (3) reopened TigerVNC Viewer, (4) reconnected to the
  same `127.0.0.1:22018`, and (5) successfully observed the same still-running BVE.
  This demonstrates `visual client disconnect/reconnect != BVE lifecycle stop/start`
  — the key lifecycle-independence requirement of #74.
- The owner then stopped the BVE through the existing harness (pressing Enter in the
  `run-bve --visual` terminal), which issued the normal `stop`/`destroy` path —
  no separate teardown was needed for the visual display.

## Fidelity limits

**#74 proves only optional local visual observation of a BVE, plus visual-client
lifecycle independence** — nothing more.

It does **not** prove:

- a production remote console (no authentication/RBAC/proxy infrastructure exists —
  none was in scope);
- a browser/Web console (no noVNC/streaming component exists);
- physical Endpoint behavior (BARE under QEMU/KVM, exactly as #72/#73 already scope
  it — see that document's fidelity limits, unchanged by #74);
- GPU/display fidelity (the emulated VGA console proves BARE emits console output,
  not any graphics/GPU capability);
- Agent behavior (BARE's serial hook still starts no Agent; #74 changes nothing
  about that).

## Related

- `docs/specifications/m0-bamep-virtual-endpoint-contract.md` — BVE responsibility
  and fidelity boundary; "port allocation, console mechanism, ... implementation
  details" out of scope for that Specification.
- `docs/reference/bve-bare-direct-boot-host-proof.md` — the #72 direct-boot proof
  whose artifacts and readiness markers this proof reuses unchanged.
- `docs/architecture/README.md` — current implemented `QemuCommand`/`BveRuntime`
  visual-display wiring.
- Issue #74 — the Work Package this proof belongs to.
