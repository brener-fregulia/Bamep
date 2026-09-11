# ADR-0025: BVE virtual UEFI firmware — OVMF non-Secure-Boot, per-BVE writable VARS

Status: Accepted

## Context

ADR-0022 fixed the BVE backend (Linux/QEMU/KVM, driven directly) and left
firmware as an explicit later decision. Issues #67–#70 used only
`Firmware::Default` (SeaBIOS): enough for VM lifecycle, deterministic storage,
and the isolated-network **BIOS-class** PXE proof (`Arch:00000`, virtio option
ROM), but `m0-bamep-virtual-endpoint-contract.md` and ADR-0024 both flagged
**UEFI PXE (#71)** as a distinct, still-open piece.

Issue #71 requires a BVE to boot the existing ADR-0021 Boot Adapter mechanism
(iPXE + wimboot + network-delivered stock WinPE) through **virtual UEFI**, so
the firmware representation is now a concrete, durable choice that also
constrains #72/#73 (BARE / Buildroot direct boot and its PXE delivery).

Read-only evidence was gathered on the reference environment (WSL2, Ubuntu
24.04, QEMU 8.2.2, KVM present):

- the `ovmf` 2024.02 package installs a matched 4 MB pflash pair —
  `/usr/share/OVMF/OVMF_CODE_4M.fd` (code) and `/usr/share/OVMF/OVMF_VARS_4M.fd`
  (variables template) — plus a Secure-Boot `OVMF_CODE_4M.secboot.fd`
  (`requires-smm`, `pc-q35` only) and a `.ms` VARS with Microsoft keys
  pre-enrolled;
- OVMF has a **built-in `virtio-net` UEFI driver** (`VirtioNetDxe`), so it can
  do UEFI PXE over `virtio-net-pci` with no option ROM. It has **no** built-in
  driver for `e1000`/`e1000e`/`rtl8139`: for those, QEMU attaches the matching
  iPXE **EFI option ROM** (`/usr/lib/ipxe/qemu/efi-*.rom`), which OVMF executes;
- a read-only inspection of the retained stock WinPE `boot.wim` (10.0.26100.1)
  found **no** NetKVM/VirtIO network driver and **no** `VEN_1AF4` hardware ID,
  but confirmed an inbox driver for the emulated Intel **82540EM**
  (`8086:100E`, `nete1g3e.inf`) — the exact NIC the earlier virtualized WinPE
  spike (`docs/reference/winpe-boot-mechanism-spike.md`) saw stock WinPE
  recognise and DHCP with.

## Decision

Add `Firmware::Uefi` to the BVE definition. It means **OVMF without Secure
Boot**:

- the **code** pflash is `OVMF_CODE_4M.fd`, attached
  `-drive if=pflash,unit=0,format=raw,readonly=on` — shared, never written,
  never in any deletion set. **Not** `OVMF_CODE_4M.secboot.fd`; no `-machine
  smm=on`;
- the **variables** pflash is a **per-BVE writable copy** of
  `OVMF_VARS_4M.fd`, attached `-drive if=pflash,unit=1,format=raw` (writable).
  Default paths are the Debian/Ubuntu `ovmf` locations, overridable with
  `BAMEP_VE_OVMF_CODE` / `BAMEP_VE_OVMF_VARS_TEMPLATE` (the Fedora lab installs
  OVMF elsewhere);
- **VARS lifecycle:** the copy is made **once, at `create`**, from the
  immutable template into the per-instance control directory
  (`<runtime-root>/<bve-id>/OVMF_VARS.fd`). `start` reuses the existing copy;
  `stop` preserves it; `start` again reuses it; `reset` (QMP `system_reset`)
  does not touch it; `destroy` removes it explicitly (before the non-recursive
  instance-dir cleanup). `reset_system_storage` never touches firmware. The
  copy is **not** refreshed on every `start` — a hidden per-boot NVRAM reset
  would weaken the #71 repeatability proof (PXE must repeat with the *same*
  firmware state).
- A read-only `check_uefi_firmware()` (separate from
  `detect_host_prerequisites`, mirroring the `qemu-img` storage split) fails
  closed with an actionable error if OVMF is not installed. This crate **never
  downloads** a firmware image and **never falls back** to SeaBIOS.
- **Secure Boot is out of scope for this variant and this Issue.** It remains
  independently owned by ADR-0010. A future Secure-Boot BVE variant is a new or
  updated ADR, not a flag added here.
- `Firmware::Uefi` **does not imply a NIC model.** `NicModel` (`VirtioNetPci`
  default, `E1000`) is an independent field; the deterministic MAC is identical
  for every model. The **Issue #71 WinPE validation profile** explicitly
  selects `NicModel::E1000` because the retained stock WinPE image has an inbox
  driver for the Intel 82540EM while it has no NetKVM/VirtIO network driver — a
  future UEFI BVE (e.g. BARE/Buildroot) may use `VirtioNetPci`. With `E1000`,
  UEFI network boot is initiated by the **e1000 EFI option ROM (iPXE) executed
  by OVMF**, not OVMF's own `VirtioNetDxe` PXE/SNP path — a fidelity
  distinction to record, not a blocker for #71.

`Firmware::Default` (SeaBIOS) is unchanged and remains the BVE default: no
`-bios`/`-pflash`, no OVMF prerequisite, byte-identical argv to #67–#70.

## Alternatives considered

- **`Firmware::Uefi { code_path, vars_template }` (explicit paths in the
  definition).** Rejected. The BVE contract's responsibility list "is not a
  schema"; per-definition firmware paths are configuration-surface creep and
  make it a caller's job to police that VARS never points at the immutable
  template. Env overrides on two well-known constants cover the one real
  portability need (Fedora lab) without putting paths in every definition.
- **Refresh the VARS copy from the template on every `start`.** Rejected (owner
  decision). It makes `start` silently redefine firmware state, which does not
  fit the BVE lifecycle and weakens "PXE repeats with the same BVE and the same
  firmware state". If OVMF ever persists something that blocks the second PXE,
  that is evidence to observe and handle explicitly, not to hide.
- **Secure-Boot OVMF (`OVMF_CODE_4M.secboot.fd` + `.ms` VARS).** Rejected for
  #71. Physical Secure-Boot proof is explicitly out of scope; the retained
  Secure-Boot iPXE shim wrapper (`snponly-shim.efi`, the `/ipxe.efi` root
  fallback) is ADR-0021-documented *physical-firmware compatibility* detail,
  not part of the BVE mechanism proof. Adopting secboot OVMF just because the
  file exists would also force `pc-q35` + SMM.
- **Keep only `Firmware::Default` and assemble the pflash argv in the host-proof
  script.** Rejected. QEMU-specific argv stays inside `bamep-ve` (architecture).
- **A `FirmwareProvider` / multi-vendor firmware abstraction.** Rejected. One
  new variant, one file pair; no second firmware exists to design against.
- **Make `E1000` a property of `Firmware::Uefi`.** Rejected (owner decision).
  Firmware and NIC model are orthogonal; coupling them would block a future
  UEFI BVE from using `virtio-net-pci`.

## Consequences

- `BveDefinition` gains `Firmware::Uefi` and a `NicModel { VirtioNetPci,
  E1000 }` field (default `VirtioNetPci`, `with_nic_model` builder). Every
  existing caller and the #70 proof are unchanged.
- `QemuCommand::for_bve` takes an `Option<UefiPflash>` (resolved by the
  runtime) and emits the pflash pair for `Firmware::Uefi`; it emits
  `-device <nic_model>` for the NIC. It stays a pure function of its arguments
  and never reads the environment.
- `BveRuntime` gains `uefi_vars: Option<PathBuf>`: `create` runs
  `check_uefi_firmware()` and writes the pristine per-BVE VARS copy; `destroy`
  removes it. `qemu` gains `check_uefi_firmware`, `ovmf_code_path`,
  `ovmf_vars_template_path`, and the `OVMF_*`/`BAMEP_VE_OVMF_*` constants.
- The #71 host proof (`scripts/bve-winpe-pxe-proof.sh` +
  `examples/bve_winpe_pxe`) is the owner-run validation. Ordinary `cargo test`
  never needs OVMF: UEFI argv, the pflash pair, NIC model, the anti-optical
  guard, and the VARS copy/remove lifecycle are covered by pure/fixture unit
  tests.
- ADR-0021's mechanism is reused unchanged; the Secure-Boot wrapper is
  deliberately not reused. See
  `docs/reference/bve-winpe-uefi-pxe-host-proof.md` for the empirical result
  and its fidelity limits.
- BVE virtual-firmware evidence is **not** physical OEM UEFI-firmware evidence
  (`m0-bamep-virtual-endpoint-contract.md` fidelity boundary).

## Related architecture

- `docs/architecture/README.md` — "Bamep Virtual Endpoint host runtime
  (`bamep-ve`)" records the implemented `Firmware::Uefi` / `NicModel`, the
  pflash pair, and the WinPE UEFI-PXE host-proof wiring.

## Related work

- ADR-0022 — Linux/QEMU/KVM backend this firmware runs on.
- ADR-0023 — deterministic BVE disk storage (the per-instance
  ownership/fail-closed pattern the VARS lifecycle mirrors).
- ADR-0024 — isolated BVE provisioning network (#71 reuses this substrate; its
  bridged-forward accommodation is amended there for non-DHCP traffic).
- ADR-0021 — iPXE + wimboot network-delivered WinPE Boot Adapter baseline (the
  mechanism #71 exercises, unchanged).
- ADR-0010 — Secure Boot baseline (independently owned; not reopened here).
- `docs/reference/winpe-boot-mechanism-spike.md` — the prior virtualized WinPE
  evidence (stock WinPE recognising the emulated Intel 82540EM).
- `docs/reference/bve-winpe-uefi-pxe-host-proof.md` — the #71 empirical result.
- `docs/specifications/m0-bamep-virtual-endpoint-contract.md` — backend- and
  firmware-independent BVE responsibility, lifecycle, and fidelity boundary.
- Issue #71 — Work Package that produced this decision.
- Issue #73 — confirms this ADR's own prediction: BARE's UEFI BVE uses
  `NicModel::VirtioNetPci` (OVMF's native `VirtioNetDxe`), not #71's `E1000`.
  See `docs/reference/bve-bare-uefi-pxe-host-proof.md`.
