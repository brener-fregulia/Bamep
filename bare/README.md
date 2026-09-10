# BARE — Bamep Agent Runtime Environment

BARE is **the minimal bootable runtime environment that hosts the Bamep Agent.
It is not a general-purpose operating system.**

```text
Buildroot      = the build system used to produce BARE
BARE           = Bamep's minimal bootable Agent Runtime Environment
Bamep Agent    = a separate future executable/component that BARE hosts
```

Issue #72 establishes the first minimal BARE image and proves it boots directly
in one Bamep Virtual Endpoint (BVE) — no PXE, no production Agent. Normative
ownership: ADR-0026 (BARE baseline), `docs/specifications/m0-bamep-virtual-endpoint-contract.md`
(BVE contract), Issue #72 (work history).

## This directory (a Buildroot `BR2_EXTERNAL` tree)

```text
bare/
├── external.desc      BR2_EXTERNAL name (BAMEP_BARE) + description
├── Config.in          BARE package menu (empty in V1 — no Agent yet)
├── external.mk        BARE package makefiles (none in V1)
├── configs/
│   └── bamep_bare_x86_64_defconfig
├── board/bamep/bare/
│   ├── linux.fragment           forced-builtin virtio / serial / initramfs symbols
│   └── rootfs-overlay/
│       └── etc/init.d/S50bare    the BARE startup hook (readiness markers)
├── buildroot.lock     the pinned upstream Buildroot release + checksum
└── README.md
```

Generated Buildroot source, downloads, and the build tree live **outside** this
directory (default `${XDG_CACHE_HOME:-$HOME/.cache}/bamep-bare/`), so nothing
heavy lands in the repo or on a `/mnt/*` DrvFs path.

## Building

```sh
scripts/build-bare.sh --preflight   # check the controlled build environment
scripts/build-bare.sh --pin         # first run: PGP-authenticate + record the SHA-256
scripts/build-bare.sh               # build (checks the archive against the pinned SHA-256)
scripts/build-bare.sh clean         # remove only the generated build tree
```

Buildroot is run under an explicit, whitespace-free Linux PATH
(`/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`, override
`BAMEP_BARE_BUILD_PATH`) — Buildroot aborts on a PATH containing spaces/TABs,
and the inherited WSL PATH carries Windows `/mnt/c/Program Files/...` entries.
The user's global environment is never modified. `--preflight` verifies that
PATH is clean and finds every mandatory Buildroot tool
(`scripts/lib/bare-build-env.sh`, tested by `scripts/build-bare-env-test.sh`).

`--pin` authenticates the first pin: it downloads `buildroot-2026.08.tar.xz`
and its official PGP signature (`.sign`), fetches the Buildroot signing key,
refuses a key served under any fingerprint other than the one in
`buildroot.lock`, verifies the signature, and records the SHA-256 **from the
signed text**. Rebuilds then only compare the archive to the pinned SHA-256.
`gpg` is required for `--pin` (already present on the reference host); nothing
is installed automatically, and a missing prerequisite is reported with the
exact command.

Artifacts:

```text
<cache>/output/bamep_bare_x86_64/images/bzImage
<cache>/output/bamep_bare_x86_64/images/rootfs.cpio.gz
```

## Booting / proving

```sh
scripts/bve-bare-direct-proof.sh   # boots the built artifacts in one BVE, twice
```

The proof never rebuilds. It creates one BVE with `Firmware::Default` +
`NicModel::VirtioNetPci` + a `DirectKernelBoot` payload, captures the serial
console, and requires `BARE_READY` and `BARE_NET_READY` in each of two boots.

## Readiness contract

```text
BARE_READY nic=<iface> block=<dev> block_sectors=<n>
    kernel + init reached; /proc /sys /dev usable; a virtio-net interface and a
    virtio-blk device are present and bound to their drivers.

BARE_NET_READY nic=<iface> addr=<ipv4>
    udhcpc obtained a lease over virtio-net + QEMU user-mode networking.
    Its absence never permits a "networking ready" claim.
```

## Licensing

BARE's Bamep-owned configuration in this directory is Apache-2.0 (repo default).
A **generated** BARE image bundles third-party software under its own licenses —
the Linux kernel and BusyBox (GPL-2.0), the C library, and the toolchain
runtime. Do not describe a generated BARE image as "Apache-2.0 only". Buildroot
`make legal-info` produces the full manifest; a short summary from the first
build is recorded in `docs/reference/`. Issue #72 is not a release-compliance
pipeline.

## Fidelity

A successful BARE boot here proves software-visible virtual behavior only
(QEMU/KVM directly loaded the kernel + initramfs; virtio NIC/block; user-mode
DHCP; repeated guest boot). It proves nothing about physical MiniPC boot,
physical UEFI/PXE/Secure Boot, real NIC/storage drivers, or the production
Agent.
