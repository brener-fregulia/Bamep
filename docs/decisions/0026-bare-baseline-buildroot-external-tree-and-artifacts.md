# ADR-0026: BARE baseline — Buildroot external tree and kernel/initramfs artifacts

Status: Accepted

## Context

Issues #67–#71 built the Bamep Virtual Endpoint (BVE) substrate: QEMU/KVM
lifecycle (ADR-0022), deterministic storage (ADR-0023), an isolated
provisioning network (ADR-0024), and virtual UEFI PXE of the existing WinPE
path (ADR-0025). `docs/specifications/m0-bamep-virtual-endpoint-contract.md`
already anticipates that software inside a BVE "may later be a Buildroot-hosted
participant or production Agent", while BVE itself ends before Agent
Protocol/product semantics.

Issue #72 needs the first Bamep-owned minimal Linux runtime to boot directly in
one BVE — the substrate a future Bamep Agent will be hosted on — instead of
continuing Alpine-specific integration inherited from the FORGE PoC
(`docs/reference/poc-lessons.md`: "diskless Alpine boot into RAM was viable as a
maintenance environment"; "updating the Agent runtime independently from the
initramfs significantly improved development iteration speed").

This introduces a named component and a build-system dependency with long-term
maintenance cost, and it constrains #73 (network delivery of the same image).
Those are durable choices, so they need an ADR rather than being fixed silently
in a Work Package.

Read-only discovery (WSL2 Ubuntu 24.04, the reference build host): `make`, `gcc`
13.3, `binutils` 2.42, `perl`, `python3`, `cpio`, `rsync`, `bc`, `wget` and
`build-essential` are present; `unzip` is absent (a Buildroot mandatory
dependency). Buildroot's release cadence is quarterly `YYYY.MM`; the current
Stable is **2026.08** (2026‑09‑04) and the current LTS series is 2025.02.x
(supported to 2028‑03).

## Decision

### 1. BARE is a named Bamep component

**BARE — Bamep Agent Runtime Environment** is *the minimal bootable runtime
environment that hosts the Bamep Agent. It is not a general-purpose operating
system.*

```text
Buildroot     = the build system used to produce BARE
BARE          = Bamep's minimal bootable Agent Runtime Environment
Bamep Agent   = a separate future executable/component that BARE hosts
```

Buildroot is how BARE is built; it is not the component's product name. #72
establishes BARE V1 and does **not** implement the production Agent.

### 2. Buildroot is the build substrate; BARE config is repository-owned as a `BR2_EXTERNAL` tree

The authoritative BARE build inputs live in a top-level **`bare/`** directory
used as a Buildroot `BR2_EXTERNAL` tree (`external.desc` name `BAMEP_BARE`):

```text
bare/
├── external.desc / Config.in / external.mk   BR2_EXTERNAL wiring (no BARE packages in V1)
├── configs/bamep_bare_x86_64_defconfig
├── board/bamep/bare/linux.fragment           forced-builtin virtio / 8250-serial / initramfs symbols
├── board/bamep/bare/rootfs-overlay/etc/init.d/S50bare   the BARE startup hook
└── buildroot.lock                            the pinned upstream release + SHA-256
```

`bare/` is **not** a Rust crate and there is no `crates/bare`. It has no
`BR2_EXTERNAL` packages in V1; a future Work Package that adds the Bamep Agent
adds its package menu there.

### 3. Buildroot itself is consumed as an external, pinned source — never vendored

`scripts/build-bare.sh` resolves Buildroot only from the exact pinned archive
named in `bare/buildroot.lock`: the archive is downloaded (or reused from the
cache), verified against the lock, and extracted once into a persistent tree
under the cache root. There is no source-tree override — the compiled Buildroot
always originates from the verified archive. Buildroot source, its `BR2_DL_DIR`
package-source cache, and the `O=` build tree all live **outside** the
repository (default `${XDG_CACHE_HOME:-$HOME/.cache}/bamep-bare/`), so no heavy
tree lands in Git or on a `/mnt/*` DrvFs path. Generated images are not
committed.

The first pin is **authenticated**, not merely hashed: `--pin` verifies the
official Buildroot release PGP signature (`buildroot-<v>.tar.xz.sign`) against a
signing-key fingerprint pinned in `bare/buildroot.lock`
(`AB07D806D2CE741FB886EE50B025BA8B59C36319` — Peter Korsgaard, the Buildroot
maintainer), and records the SHA-256 taken from the *signed* text. Rebuilds
then compare the archive to that pinned SHA-256 with no network trust. `gpg` is
a `--pin`-only prerequisite.

Buildroot is invoked under an explicit whitespace-free Linux PATH
(`/usr/local/sbin:…:/bin`), because Buildroot aborts on a PATH containing
spaces/TABs and the WSL-inherited PATH carries Windows `/mnt/c/Program Files/…`
entries. The user's global environment is never modified; `--preflight` checks
that the effective PATH is clean and finds every mandatory Buildroot tool.

- Vendoring Buildroot into the repo, or a Git submodule, are rejected: both add
  a large second-project checkout with recurring update cost and no benefit
  over a pinned, PGP-authenticated archive.
- Recording the SHA-256 of the just-downloaded archive without checking the
  upstream signature is rejected: it protects later builds against drift but
  does not authenticate the origin of that first archive.

### 4. The baseline is Buildroot **Stable 2026.08**

`bare/buildroot.lock` pins `buildroot_version = 2026.08` plus the archive name,
URL, and SHA-256. The build refuses `latest`, refuses any other version or
series, and never falls back.

Stable, not LTS, is the initial baseline **on purpose**: BARE is still being
established (kernel, VirtIO, initramfs and minimal userspace are still being
validated), there is no installed production base that would make an older LTS
worth its trade-offs, and starting from a recent upstream reduces friction now.
This is the *current baseline*, not a policy of tracking every quarterly
release. A future BARE revision may adopt a specific Stable or an LTS series —
that is a deliberate update to this ADR, not a side effect.

The x86_64 profile also sets `BR2_LINUX_KERNEL_NEEDS_HOST_LIBELF=y` in the
defconfig: the arch default kernel config enables `CONFIG_UNWINDER_ORC`, whose
`objtool` host tool needs libelf, and Buildroot supplies it via `host-elfutils`
(not a host `libelf-dev` package, and not by disabling ORC/objtool). The
empirical build/boot result is owned by
`docs/reference/bve-bare-direct-boot-host-proof.md`.

### 5. BARE artifacts are a separate kernel and initramfs

BARE produces exactly:

```text
<O>/images/bzImage           the Linux kernel
<O>/images/rootfs.cpio.gz    a standalone gzip-compressed initramfs (cpio)
```

The initramfs is **not** embedded in the kernel. Keeping them separate lets #73
load them with iPXE `kernel` / `initrd` unchanged, and lets the kernel and the
(future Agent-carrying) rootfs evolve independently. No bootable disk image and
no ISO are produced.

### 6. Validation mechanism (not the subject of this ADR)

#72 proves BARE by **direct kernel boot**: QEMU/KVM loads `bzImage` +
`rootfs.cpio.gz` via `-kernel`/`-initrd`/`-append`, with no firmware boot
device, no bootloader and no ISO. `bamep-ve` gains a narrow
`DirectKernelBoot { kernel, initrd, command_line }` (orthogonal to
`Firmware`/`NicModel`/`BootMode`; conflicts only with `BootMode::NetworkFirst`)
and an opt-in append-mode serial capture for machine-readable boot evidence.
This is a consequence of choosing a direct-boot artifact shape, not a general
`BootProvider`/`BootBackend` abstraction (ADR-0022 still defers those). BVE
understands only "boot this kernel and initrd", never "this is BARE".

## Alternatives considered

- **Continue Alpine-specific integration.** Rejected. #72 exists to replace
  that with a Bamep-owned, reproducible substrate; the FORGE Alpine work is
  historical evidence, not an inherited constraint.
- **Buildroot LTS (2025.02.x) as the initial baseline.** Not selected now.
  Long-term maintenance matters, but no operational base yet depends on BARE;
  a recent Stable is the better starting point and an LTS can be adopted later
  by updating this ADR.
- **Vendor or submodule Buildroot.** Rejected — large second-project tree,
  recurring update cost, no benefit over a pinned archive + SHA-256.
- **`bare/output/` inside the repo (gitignored) as the default build tree.**
  Rejected. The repo lives on `/mnt/d` (DrvFs); Buildroot does heavy filesystem
  work. The default output lives on the native Linux filesystem under a cache
  root; `BAMEP_BARE_OUTPUT` can override it, and `build-bare.sh clean` validates
  the target (fail-closed) before removing it.
- **Kernel with embedded initramfs (one artifact).** Rejected. It couples
  kernel and rootfs evolution and does not fit iPXE `kernel`/`initrd` reuse in
  #73.
- **Bootable disk image or ISO through firmware.** Rejected for #72 — adds
  partition/filesystem/bootloader/UEFI concerns the runtime proof does not
  need, and an ISO fallback is exactly the silent-boot-path risk #71 guarded
  against.
- **A generic `BootProvider`/`BootBackend` in `bamep-ve`.** Rejected — ADR-0022
  defers backend abstraction; there is one direct-boot need, represented as one
  narrow typed field.
- **Pre-installing `bison`/`flex`/`pkg-config` as host prerequisites.**
  Rejected. Buildroot's current manual does not list them as mandatory and
  Buildroot builds host packages for such tools when needed. `build-bare.sh`
  preflights the actual mandatory set; `unzip` is the one confirmed gap and the
  owner installs it. A dependency the real build later proves missing is
  handled on that evidence, not pre-empted.

## Consequences

- New top-level `bare/` `BR2_EXTERNAL` tree; new `scripts/build-bare.sh`
  (build / `--pin` / `clean` / `--preflight`) with `scripts/lib/bare-build-env.sh`
  (controlled PATH + tool checks, tested by `scripts/build-bare-env-test.sh`),
  `scripts/bve-bare-direct-proof.sh`, `scripts/lib/bare-serial-evidence.sh`
  (+ its parser test), and `crates/ve/examples/bve_bare.rs`.
- `bamep-ve` gains `DirectKernelBoot` + `BveDefinition::with_direct_kernel` /
  `ensure_direct_kernel_ready`, a 4th `serial: Option<&Path>` argument to
  `QemuCommand::for_bve`, and `BveRuntime::with_serial_capture` /
  `serial_log()` / `SERIAL_LOG_FILENAME`. Every path that does not opt in emits
  byte-identical argv (`-serial none`, no `-kernel`). `SimulatorBve` is
  unchanged.
- A generated BARE image bundles third-party software under its own licenses
  (Linux kernel, BusyBox — GPL-2.0; the C library; the toolchain runtime). A
  BARE image must not be described as "Apache-2.0 only". Buildroot
  `make legal-info` is retained as auxiliary evidence; a short summary from the
  first build goes in `docs/reference/`. #72 is not a release-compliance
  pipeline.
- #73 must reuse the **same** `bzImage` + `rootfs.cpio.gz` over the isolated
  provisioning network and must not rebuild BARE around a second boot model.
- Reproducibility claim: **build inputs/configuration are pinned** (Buildroot
  version + SHA-256, defconfig, kernel fragment, rootfs overlay), not
  bit-for-bit output. Artifact hashes are recorded as empirical evidence, not
  architecture pins.
- A future change of Buildroot baseline (another Stable, or an LTS series), of
  the consumption model, or of the artifact shape is a new or updated ADR.

## Related architecture

- `docs/architecture/README.md` — "Bamep Virtual Endpoint host runtime
  (`bamep-ve`)" records the implemented `DirectKernelBoot`, serial capture, and
  the `bare/` tree and its build/proof scripts.

## Related work

- ADR-0022 — Linux/QEMU/KVM BVE backend (defers any boot-backend abstraction).
- ADR-0023 — deterministic BVE storage (the blank disposable overlay BARE's
  block-device proof uses).
- ADR-0025 — BVE virtual UEFI firmware (records that a future UEFI BVE such as
  BARE may use `VirtioNetPci`; #72 uses `Firmware::Default` + direct kernel).
- `docs/specifications/m0-bamep-virtual-endpoint-contract.md` — BVE
  responsibility/lifecycle/fidelity boundary; anticipates a Buildroot-hosted
  guest participant.
- `docs/reference/bve-bare-direct-boot-host-proof.md` — the #72 empirical
  result and its fidelity limits.
- `docs/reference/poc-lessons.md` — FORGE Alpine/initramfs lessons this
  supersedes for Bamep.
- Issue #72 — the Work Package that produced this decision.
- Issue #73 — network delivery of the same BARE artifacts (hard boundary: #72
  does not implement PXE/DHCP/TFTP/HTTP/iPXE/Secure Boot for BARE).
  Implemented reusing `bzImage`/`rootfs.cpio.gz` unchanged over the isolated
  #70 network and the Issue #71 UEFI/iPXE bootstrap; a pre-implementation
  spike confirmed BARE's kernel already builds `CONFIG_EFI`/`CONFIG_EFI_STUB`,
  so no `linux.fragment`/defconfig/Buildroot change was made. See
  `docs/reference/bve-bare-uefi-pxe-host-proof.md`.
