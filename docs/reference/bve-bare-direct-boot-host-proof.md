# BVE BARE Direct-Boot — Host-Proof Reference (Issue #72)

Status: **Validated — owner-run build + direct BVE boot proof passed
(2026-09-10, WSL2).**

This document records the implemented Issue #72 wiring and the empirical result
of the owner-run validation: the Buildroot build, the direct BVE boot proof
(two accepted boots), the dedicated in-crate host test, `make legal-info`, and
the pure/static checks. It is the authority for that empirical result; ADR-0026
and `docs/architecture/` reference it rather than duplicating it.

Normative ownership: ADR-0026 (BARE baseline: Buildroot external tree +
kernel/initramfs artifacts); `docs/specifications/m0-bamep-virtual-endpoint-contract.md`
(BVE contract and fidelity boundary); Issue #72 (work history). Physical MiniPC
boot, physical UEFI/PXE/Secure Boot, and real NIC/storage behavior remain the
physical Integration Environment's authority.

## What BARE is

**BARE — Bamep Agent Runtime Environment** is the minimal bootable runtime
environment that hosts the Bamep Agent. It is not a general-purpose operating
system. Issue #72 builds BARE V1 and boots it; it does **not** implement the
production Agent, and it does **not** deliver BARE over PXE (that is #73).

## Reference environment

- WSL2, Ubuntu 24.04.1 LTS; kernel 6.6; 16 vCPU, ~15 GiB RAM, ~945 GiB free on
  the native Linux filesystem.
- QEMU / `qemu-img` 8.2.2; `/dev/kvm` present and usable; `gpg` (GnuPG) 2.4.4.
- All Buildroot mandatory host tools present under the controlled build PATH.
  `unzip` was absent at first discovery and was installed by the owner
  (`sudo apt-get install -y unzip`). `bison` / `flex` / `pkg-config` are **not**
  pre-installed (not Buildroot mandatory dependencies; Buildroot builds host
  packages when needed).
- The first real build attempt aborted in Buildroot's own PATH sanity check
  ("Your PATH contains spaces, TABs, and/or newline") — the inherited WSL PATH
  includes Windows `/mnt/c/Program Files/…` entries. Fixed in the build tooling
  (controlled PATH), not in the user environment.

## BARE configuration (`bare/`)

A Buildroot `BR2_EXTERNAL` tree, `external.desc` name `BAMEP_BARE`:

- **defconfig** `bamep_bare_x86_64_defconfig`: `BR2_x86_64`, musl toolchain,
  kernel from the x86_64 arch default config + the BARE fragment, standalone
  `rootfs.cpio.gz` (not embedded), BusyBox init, **no getty**, no systemd, no
  SSH, no compiler/package-manager in the image, `BR2_ROOTFS_OVERLAY` →
  `board/bamep/bare/rootfs-overlay`.
- **`linux.fragment`** forces built-in (never modules): `CONFIG_VIRTIO`,
  `CONFIG_VIRTIO_PCI`, `CONFIG_VIRTIO_NET`, `CONFIG_VIRTIO_BLK`;
  `CONFIG_SERIAL_8250(_CONSOLE|_PCI)`; `CONFIG_DEVTMPFS(_MOUNT)`,
  `CONFIG_PROC_FS`, `CONFIG_SYSFS`, `CONFIG_TMPFS`; `CONFIG_BLK_DEV_INITRD`,
  `CONFIG_RD_GZIP`; `CONFIG_NET`/`INET`/`PACKET`/`NETDEVICES` for udhcpc.
- **`board/bamep/bare/rootfs-overlay/etc/init.d/S50bare`** — the BARE startup
  hook (readiness markers, below).
- **`buildroot.lock`** pins `2026.08` + archive name/URL, the release signing
  fingerprint, and the PGP-signed SHA-256.
- **`BR2_LINUX_KERNEL_NEEDS_HOST_LIBELF=y`** — required on x86_64: the arch
  default config enables `CONFIG_UNWINDER_ORC`, whose `objtool` host tool needs
  libelf; Buildroot supplies it via `host-elfutils` (see "Evidence").

## Build (`scripts/build-bare.sh`)

```text
--preflight     controlled-build-environment check only (PATH + mandatory tools)
--pin           download archive + .sign → PGP-verify against the pinned signing
                key → record the signed SHA-256 into bare/buildroot.lock
(default)       preflight → archive == pinned SHA-256 → extract → make defconfig → make
clean           remove ONLY the generated output tree (validated, fail-closed)
```

- **Controlled PATH.** Buildroot aborts if `PATH` contains a space/TAB/newline;
  the WSL-inherited PATH carries Windows `/mnt/c/Program Files/…` entries. The
  script runs Buildroot (preflight, `make defconfig`, `make`, `legal-info`, and
  its own downloads/`gpg`/`tar`) under an explicit
  `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`
  (`BAMEP_BARE_BUILD_PATH` override, still whitespace-validated). The user's
  `~/.bashrc` / `/etc/wsl.conf` are never touched.
  `scripts/lib/bare-build-env.sh` + `scripts/build-bare-env-test.sh`.
- **Authenticated first pin.** `--pin` downloads `buildroot-2026.08.tar.xz.sign`
  (an official clearsigned message carrying the archive SHA-256), fetches the
  Buildroot signing key from `https://buildroot.org/~jacmet/pubkey.gpg`,
  **refuses** a key whose fingerprint is not
  `AB07D806D2CE741FB886EE50B025BA8B59C36319` (Peter Korsgaard), `gpg --verify`s
  the signature (require `GOODSIG` + `VALIDSIG` with that fingerprint), takes
  the SHA-256 from the *verified* text, and cross-checks the downloaded archive.
  Rebuilds only compare against the pinned SHA-256. `gpg` is a `--pin`-only
  prerequisite (present on the reference host).
- Source: `BAMEP_BARE_BUILDROOT_SRC=<pre-staged tree>` or the pinned archive
  download. Never `latest`, never a fallback version.
- Cache root `BAMEP_BARE_CACHE_ROOT` (default
  `${XDG_CACHE_HOME:-$HOME/.cache}/bamep-bare`): `buildroot-2026.08/`, `dl/`
  (`BR2_DL_DIR`, reusable offline after the first build), `output/bamep_bare_x86_64/`.
- `BAMEP_BARE_OUTPUT` overrides the output tree; `clean` still validates the
  target before removing it.
- A clean first build needs internet (Buildroot fetches package sources);
  repeat builds with a populated `dl/` do not. Nothing is downloaded during
  `cargo test`.
- Artifacts: `<O>/images/bzImage`, `<O>/images/rootfs.cpio.gz`.

## Direct-boot profile

```text
firmware  : Firmware::Default (SeaBIOS) — bypassed
boot      : DirectKernelBoot { kernel = bzImage, initrd = rootfs.cpio.gz,
            command_line = "console=ttyS0,115200 panic=-1" }
            → QEMU -kernel / -initrd / -append; no -boot, no -cdrom, no bootable disk
NIC       : NicModel::VirtioNetPci   net: QEMU user-mode / SLIRP (unprivileged)
disk      : one blank disposable QCOW2 overlay over a blank base (virtio-blk) —
            driver evidence only; never partitioned, formatted, mounted rw, or written
serial    : -display none; -chardev file,id=char0,path=<instance>/serial.log,append=on
            + -serial chardev:char0  (headless machine-readable capture; not #74)
```

> #72 proves **QEMU/KVM directly loaded the BARE Linux kernel + initramfs**. It
> does **not** prove SeaBIOS booted BARE as a firmware/disk payload, and —
> because direct kernel boot bypasses firmware and NVRAM — it does **not** test
> UEFI firmware.

## Readiness contract (BARE-owned, emitted on the serial console)

```text
BARE_READY nic=<iface> block=<dev> block_sectors=<n>
    kernel + BusyBox init reached; /proc /sys /dev usable; a virtio-net
    interface AND a virtio-blk device are present and bound to their drivers.
    Emitted only when all of that holds — otherwise BARE emits
    "BARE NOT_READY nic=… block=…" and no READY line.

BARE_NET_READY nic=<iface> addr=<ipv4>
    udhcpc obtained a lease over virtio-net + QEMU user-mode networking.
    A separate, honest claim: its absence never permits "networking is ready".
    DHCP failure emits "BARE NET_NOT_READY … reason=dhcp-failed" instead.
```

The hook starts no Agent; after the markers, BusyBox init idles.

## Proof (`scripts/bve-bare-direct-proof.sh`)

Normal user, no sudo, no TAP/bridge, no #70/#71 network. Never rebuilds BARE.

```text
resolve artifacts ($BAMEP_BARE_OUTPUT or --kernel/--initrd)
→ cargo build --example bve_bare
→ bve_bare check   (qemu + /dev/kvm + qemu-img + artifacts)
→ bve_bare run-bve <id> --serial-out <tmp> --boot-hold <secs>
     one BveRuntime / definition / storage / kernel / initrd:
       start → Running → hold → stop → Stopped        (boot #1)
       start → Running → hold → stop → Stopped        (boot #2)
     no rebuild, no recreate between boots; QEMU appends to serial.log
   emits BVE_BOOT{1,2}_LOG_RANGE (line ranges; it does not interpret markers)
→ parse serial.log per range with scripts/lib/bare-serial-evidence.sh:
     BARE_READY #1 · BARE_NET_READY #1 · BARE_READY #2 · BARE_NET_READY #2 · BVE reboot
→ destroy → destroy_instance_storage → scratch removed
```

`scripts/bve-bare-direct-proof-parser-test.sh` unit-tests the parser
(`bare_ready_in_range`, `bare_net_ready_in_range`, `bare_not_ready_in_range`)
with canonical serial fixtures — no QEMU, no build.

`crates/ve/tests/bare_direct_host.rs` (`BAMEP_BVE_BARE_HOST_TEST=1` +
`BAMEP_BVE_BARE_KERNEL` / `BAMEP_BVE_BARE_INITRD`) is the smallest in-crate
regression anchor: two direct boots of one BVE, `BARE READY` after each,
`destroy` removes the serial log. An ordinary `cargo test` skips it.

## Repeatability

Same `BveRuntime` / `BveDefinition` / storage / `bzImage` / `rootfs.cpio.gz`;
`start → BARE_READY → BARE_NET_READY → stop`, twice, then `destroy`. No manual
VM reconstruction, no hidden rebuild between boots. No firmware/NVRAM is
involved (direct kernel boot bypasses it).

## Fidelity limits

**#72 proves QEMU/KVM direct Linux kernel + initramfs boot** — nothing more. It
does **not** prove UEFI boot, PXE, Secure Boot, physical hardware compatibility,
physical NIC/storage compatibility, or the production Agent. BARE remains *the
minimal bootable runtime environment that hosts the future Bamep Agent; it is
not a general-purpose operating system.*

- QEMU/KVM direct kernel load ≠ physical firmware boot; no firmware/UEFI/NVRAM
  path is exercised.
- `virtio-net` / `virtio-blk` under QEMU ≠ a physical NIC or storage controller,
  its option ROM, driver, or PHY.
- BusyBox `udhcpc` over QEMU user-mode/SLIRP ≠ physical DHCP; **no internet
  access is claimed or required**.
- The block-device check is read-only presence + size; no filesystem, no
  partitioning, no write — #72 grants no destructive storage authorization.
- A generated BARE image bundles GPL-2.0 and other third-party components; it is
  not "Apache-2.0 only".
- BARE V1 is a minimal substrate, not a production-complete package set.

## Evidence (owner-run, 2026-09-10, WSL2 Ubuntu 24.04, QEMU/KVM 8.2.2)

### Build

- `build-bare.sh --preflight`: effective PATH
  `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin` (whitespace-free);
  all 20 mandatory Buildroot tools found under it. The inherited WSL PATH
  (Windows `/mnt/c/Program Files/…` entries) is not used — Buildroot's own PATH
  sanity check aborted the first attempt until this was fixed in the tooling.
- Buildroot **2026.08**, pinned. `buildroot_sha256`
  `87aaca4164ea9d5c8085854953018263f7963f07c22e73a2a2185cc98c581c34` — taken
  from the PGP-signed `.sign` (GOODSIG + VALIDSIG from key
  `AB07D806D2CE741FB886EE50B025BA8B59C36319`, Peter Korsgaard) and matching the
  downloaded archive.
- The first kernel build aborted at `objtool`
  (`fatal error: gelf.h: No such file or directory`): the x86_64 arch default
  config enables `CONFIG_UNWINDER_ORC`, which builds `objtool`, which needs
  libelf. Fixed with `BR2_LINUX_KERNEL_NEEDS_HOST_LIBELF=y` → Buildroot builds
  `host-elfutils 0.195` (+ `host-pkgconf`) and supplies `gelf.h`. **Not** a host
  `libelf-dev` package; **not** by disabling ORC/objtool or stack validation.
  Guarded by `scripts/bare-defconfig-test.sh`.
- The build then completed (incremental, **no `clean`** — the persistent
  output/cache was reused).
- **Artifacts** (empirical snapshot of this build, **not** an architecture pin —
  no bit-for-bit reproducibility is claimed):

  | artifact | size (bytes) | SHA-256 |
  | --- | --- | --- |
  | `bzImage` | 14 959 616 | `5db627be920603b8862c97101a695a20b024e3a64b8e7d5e38512dd8a51d7c04` |
  | `rootfs.cpio.gz` | 1 165 658 | `57ed483302a5b110eeaffe130cb232cf1ef3a0a329c33d721aef52fc08063eae` |

  - Kernel **Linux 7.1.13** (Buildroot 2026.08 `LATEST_VERSION`). `.config`
    confirms built-in: `CONFIG_VIRTIO_NET/BLK/PCI`, `CONFIG_SERIAL_8250_CONSOLE`,
    `CONFIG_DEVTMPFS_MOUNT`, `CONFIG_BLK_DEV_INITRD`, `CONFIG_RD_GZIP`,
    `CONFIG_UNWINDER_ORC`.
  - rootfs: BusyBox init, `/etc/init.d/S50bare` present and mode `0755`,
    `sbin/udhcpc` + `usr/share/udhcpc/default.script` present.
  - `bzImage` was byte-identical across two builds on this host; the
    `rootfs.cpio.gz` hash varied between builds (cpio/gzip embed per-build
    metadata) — consistent with "reproducible inputs, not bit-for-bit output".

- **Incremental rebuild** (one further `./scripts/build-bare.sh` with no
  relevant change): `real 0m13.436s` — a one-off measurement showing the
  persistent output/cache enables a far shorter cycle than a cold build. Not an
  SLA or a requirement.

### Direct BVE boot proof — `scripts/bve-bare-direct-proof.sh`

```text
host + artifacts ..... ok
BVE x2 (direct boot) . ok

BARE_READY #1 ........ PASS
BARE_NET_READY #1 .... PASS
BARE_READY #2 ........ PASS
BARE_NET_READY #2 .... PASS
BVE reboot ........... PASS
```

One `BveRuntime` / `BveDefinition` / storage / `bzImage` / `rootfs.cpio.gz`:
`start → BARE_READY → BARE_NET_READY → stop`, twice, then `destroy` +
`destroy_instance_storage`. **No rebuild and no recreate between the two boots.**
`BARE_READY` (kernel + init reached; `/proc` `/sys` `/dev`; virtio-net and
virtio-blk present and driver-bound) and `BARE_NET_READY` (BusyBox `udhcpc`
lease over QEMU user-mode networking) are **separate** markers and both appeared
in **each** boot's own serial-log line range.

### Dedicated in-crate host test

`BAMEP_BVE_BARE_HOST_TEST=1 … cargo test -p bamep-ve --test bare_direct_host`:
`bare_boots_directly_twice_in_one_bve … ok` (1 passed, 0 failed). An ordinary
`cargo test` skips it.

### Pure / static validation

`bare-build-env` 10/10, `bare-defconfig` 26/26, `bare-serial-evidence` 13/13,
and the WinPE parser regressions 16/16 — all with no QEMU, no build, no network.

### `make legal-info`

The owner ran Buildroot's official `make … legal-info`. It **completed
successfully** and produced the manifest tree at
`<O>/legal-info/` covering Buildroot / the toolchain / Linux 7.1.13 /
BusyBox 1.38.0 / musl 1.2.6 / GCC 15.3.0 / binutils 2.45.1 /
`host-elfutils 0.195` and the other components.

- Buildroot also emitted `WARNING: the Buildroot source code has not been
  saved` — the per-package **source archives** were not collected into
  `legal-info/`. This is a limitation to note, not a failure of the target.
- `legal-info` is **auxiliary evidence**, not a #72 acceptance criterion. #72
  does **not** claim commercial release/source-distribution compliance; that
  remains future release/compliance work. The `legal-info/` tree is not
  committed to Git.

## Related

- ADR-0026 — BARE baseline (this proof's normative decision).
- ADR-0022 / ADR-0023 / ADR-0025 — BVE backend, storage, virtual UEFI.
- `docs/specifications/m0-bamep-virtual-endpoint-contract.md` — BVE
  responsibility and fidelity boundary.
- `docs/reference/bve-winpe-uefi-pxe-host-proof.md` — the #71 proof (a
  different boot model: virtual UEFI PXE of stock WinPE).
- `docs/reference/poc-lessons.md` — FORGE Alpine/initramfs lessons.
- Issue #72 — the Work Package this proof belongs to.
- Issue #73 — network delivery of the same BARE artifacts.
