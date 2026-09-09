# ADR-0023: BVE disk storage — sparse RAW base + per-instance QCOW2 overlay

Status: Accepted

## Context

ADR-0022 fixed the initial BVE virtualization backend (Linux/QEMU/KVM, driven
directly) and `docs/specifications/m0-bamep-virtual-endpoint-contract.md` deferred
"qcow2 vs. raw, backing images, overlays, and storage reset/disposal policy" to
Issue #69. Issues #67 and #68 implemented one real BVE lifecycle and its
Simulator-facing orchestration using a single throwaway RAW disk with no
reset semantics.

Issue #69 needs a reproducible disk model: a known system base state, a fresh
per-instance writable system disk derived from it, an explicit storage reset
that returns a fresh writable instance from the same base, and an independent
source-disk fixture — all without allocating the full logical
(Windows-oriented, initially 80 GiB) capacity on the host, and with
path-scoped fail-closed deletion. This model constrains later WinPE (#71),
Buildroot (#72/#73), and capture work, so the format/backing choice is a
durable architectural decision rather than an implementation detail.

Empirical evidence was gathered on the reference environment (WSL2, QEMU /
`qemu-img` / `qemu-io` 8.2.2, ext4): a RAW image created at 80 GiB logical
size allocates ~4 KiB on the host; a `qcow2` overlay created with an explicit
`-F raw -b <base>` records the backing filename and format; a write into the
offline overlay via `qemu-io` leaves the base's content hash unchanged;
removing and recreating the overlay discards that write and re-derives from
the same base; and a missing backing file makes `qemu-img create` fail
closed.

## Decision

- The BVE **system base** is a **sparse RAW** image of the full logical
  capacity, created once per storage root, marked read-only, and never
  rewritten or recreated by any per-instance operation.
- Each BVE's **writable system disk** is a per-instance **QCOW2 overlay**
  whose backing image is that base, always created with an explicit
  `-f qcow2 -F raw -b <base>` (no format auto-detection anywhere).
- **Storage reset** discards and recreates only that QCOW2 overlay from the
  same base. It is a distinct operation from the VM `reset` (QMP
  `system_reset`) and fails closed unless the BVE is stopped.
- The **source fixture** is an independent per-instance **sparse RAW** image,
  attached as a separate virtio-blk device with a stable identity, never as
  the system/boot disk. Storage reset does not touch it; only an explicit
  instance-storage disposal removes it.
- Image creation shells out to **`qemu-img`** via argv (never a shell
  string), capturing exit status and stderr. `qemu-img` is a **storage**
  prerequisite only — VM lifecycle execution still requires only
  `qemu-system-x86_64` and KVM.
- All disposable paths are derived from a validated storage root plus the
  validated `BveId` plus a fixed role filename. Per-instance reset/disposal
  removes only known files and then non-recursively removes the instance
  directory; the base is never in any instance deletion set.
- This decision does not populate any guest OS, filesystem, or partition
  table into the base, and does not define snapshot trees, image catalogs,
  deduplication, compression, or encryption.

## Alternatives considered

- **QCOW2 base + QCOW2 overlay.** Rejected. It adds image-format machinery
  (refcounts, cluster metadata, lazy refcounts) inside the layer that is
  supposed to be immutable and trivially verifiable, with no benefit while
  the base is opaque/zero-filled. Byte-level immutability is easier to prove
  for a RAW base.
- **Sparse RAW base with a `cp --reflink` / copy-on-write copy per instance
  instead of an overlay.** Rejected. Reflink support is filesystem-dependent
  (btrfs/XFS yes; the ext4 used by the WSL2 dev environment and the Linux lab
  no); "reset" would degrade to a full copy or vary by host, and it is not a
  QEMU-level CoW model. Not portable across the environments Bamep targets.
- **Preallocating the full 80 GiB base.** Rejected. It wastes host storage
  for an empty image; sparse allocation is a hard requirement of Issue #69.
- **Generating QCOW2 images without `qemu-img`.** Rejected. Hand-writing
  image containers is error-prone and unsafe; `qemu-img` is already present
  wherever `qemu-system` is, as a storage-only prerequisite.
- **Extending `BveRuntime::reset` / `BveRuntime::destroy` to cover storage.**
  Rejected. `reset` (VM reboot) and `destroy` (control resources) keep their
  approved meanings; storage reset and storage disposal are separate,
  explicit operations so a previously safe call never starts deleting large
  images.

## Consequences

- `bamep-ve` gains a `storage` responsibility (`BveStorageRoot`,
  `BveStorageLayout`, `PreparedInstanceStorage`, `ensure_system_base`,
  `prepare_instance`, `reset_system_storage`, `destroy_instance_storage`) and
  a `qemu-img` storage prerequisite check, separate from
  `detect_host_prerequisites`.
- `BveDefinition` identifies the disks actually attached (`DiskAttachment`
  with an explicit `DiskRole` and `DiskFormat`); the writable overlay, not
  the base, is the guest system disk.
- Later WinPE/Buildroot/capture work can put real content into the base
  without changing the conceptual model or the reset semantics.
- A future storage-model change (a different backing strategy, resettable
  source semantics, multi-disk profiles) is a new or updated ADR, not a
  silent code change.

## Related architecture

- `docs/architecture/README.md` — "Bamep Virtual Endpoint host runtime
  (`bamep-ve`)" records the implemented storage module, path layout, and
  reset implementation.

## Related work

- ADR-0022 — Linux/QEMU/KVM initial BVE backend this storage model runs on.
- `docs/specifications/m0-bamep-virtual-endpoint-contract.md` — backend-
  independent storage semantics this decision supplies a mechanism for.
- Issue #67 — one-BVE QEMU/KVM lifecycle.
- Issue #68 — Simulator-facing BVE orchestration.
- Issue #69 — Work Package that produced this decision.
