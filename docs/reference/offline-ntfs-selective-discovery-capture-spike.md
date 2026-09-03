# Offline NTFS Selective Discovery / Capture — Spike Evidence

Status: **Completed empirical reference.** Evidence gathering only. Covers a Linux-created
NTFS fixture; Windows-created NTFS behaviour remains an untested evidence gap (see
*Limitations*).

This document preserves reproducible local evidence for Issue #46. It does **not** define a
Bamep contract. `docs/specifications/m0-data-plane-and-storage-contracts.md` owns Artifact
integrity/completeness and `capture_consistency`; ADR-0008 owns data-plane rationale;
ADR-0020 (not reopened) owns the composed-service/intervention model. Anything here that
looks like it needs a normative change is listed under *Remaining decisions* for owner
review only.

Evidence is tagged **Observed** (direct experiment result), **Inferred** (technical
conclusion from an observation), **Not tested** (not exercised — not the same as
unsupported), and **Unsupported by candidate** (a demonstrated limitation).

## Question

From an offline NTFS source on the Linux maintenance side, what can Bamep truthfully
promise about: offline enumeration; mount-independent path identity; reparse points; NTFS
metadata preservation; arbitrary explicit operator selection; discovery-to-capture drift;
captured-result evidence; Selective Artifact granularity pressure; and the minimum
information a future restore contract would need?

## Environment

- **OS / kernel**: Fedora Linux 44 (Server Edition), `7.1.10-200.fc44.x86_64`.
- **NTFS tooling**: `ntfs-3g` / `libntfs-3g` / `mkntfs` / `ntfsinfo` / `ntfsls` / `ntfscat` /
  `ntfssecaudit` / `ntfsdecrypt`, all **v2026.2.25**, "integrated FUSE 28".
- Kernel **`ntfs3`** module is present (author K. Komarov; read-only lzx/xpress; POSIX ACL
  build) but was **not exercised** — see Limitations.
- **Supporting tools**: GNU `tar` 1.35, `rsync` 3.5.0, GNU coreutils, `attr`
  (`getfattr`/`setfattr`), `podman` 5.8.4 (rootless), Python 3.14.7.
- **Privilege**: uid 1000, `wheel` group, but `sudo` requires a password (non-interactive
  → unavailable). `losetup` / `mount(8)` for real filesystems require root → unavailable.
- **Mount path used**: `podman unshare` (rootless user namespace, backed by
  `newuidmap`/`newgidmap` and an `/etc/subuid` range) is the only context in which
  `ntfs-3g` FUSE mounts succeeded. A bare `unshare --map-root-user` failed
  (`setgroups`/priv-drop). The **libntfs-3g userspace tools need no namespace and no
  privilege** — they operate directly on the image.
- No existing Bamep test fixture provides disposable filesystem infrastructure
  (`crates/*/fixture.rs` are domain fakes, unrelated).

All experiment material was disposable, created under a session scratchpad, never inside
the repository. Repository HEAD `d990f77bd919e152a5ba53b2fb4f3748263434e4`, working tree
unchanged apart from this document.

## Fixture

**Linux-created NTFS only. No genuine Windows-created fixture was available or
constructible in this environment.**

- 96 MiB image **file** (not a block device): `mkntfs --fast --force --label BAMEPSPIKE`
  runs directly on a regular file — no loop device, no root. NTFS 3.1, 4096-byte clusters,
  512-byte sectors, volume serial `0x3e2e1ea174cd3810`.
- Populated through an `ntfs-3g` read-write FUSE mount inside `podman unshare`.
- Every name landed in the NTFS **POSIX namespace** (case-sensitive) and every object is
  owned by mapped-root. This **diverges from Windows-created NTFS**, which uses the
  Win32 / Win32+DOS namespace (case-insensitive, case-preserving, 8.3 aliases), real
  account SIDs, and real security descriptors.

Contents built:

- *Ordinary*: nested directories; normal / empty / 3 MiB files; a name with spaces; a
  Unicode name `relatório-café-ação-日本語.txt` (stored NFC); mixed-case siblings.
- *Operator-selected, heuristic-hostile*: an ELF binary as `\PortableApps\DefragTool\defrag.exe`
  (stand-in for an arbitrary portable executable); `save01.sav` under
  `\Games\SomeGame\_cache\usercontent`; `license.key` under `\Windows\Temp\vendorstash`.
- *NTFS-specific, attempted*: two alternate data streams; relative, volume-absolute, and
  deliberately-looping symlinks; an 8 MiB sparse file; a compression attempt; an ACL
  attempt.

Could **not** be constructed here (→ Not tested for creation/behaviour):

- NTFS transparent compression on write — `setfattr -n system.ntfs_compression` was a
  no-op (data stayed uncompressed; ntfs-3g created a junk `ntfs-3g.system.ntfs_compression`
  stream); `chattr +c` → `EINVAL` over FUSE.
- POSIX-ACL → NTFS-SD mapping — `setfacl` → `EOPNOTSUPP`. Only the default mapped-root SD
  exists. (SD *reading* does work — see Metadata.)
- Genuine reparse points — junctions (`IO_REPARSE_TAG_MOUNT_POINT`), Windows symlink
  reparse tag, dedup / WOF / WIM, cloud placeholders. ntfs-3g cannot create them.
- Hard links, NTFS object IDs, a populated `$UsnJrnl` / `$LogFile` (left at sequence 0),
  EFS-encrypted content.

## Methods

1. **Userspace, no mount, no root**: `ntfsls -R`, `ntfsinfo -F <path>`, `ntfscat` against
   the image file directly.
2. **`ntfs-3g` FUSE** mounted `ro` and `rw` inside `podman unshare`; enumeration with
   `find`; capture with `cp -a`, `rsync -aAHXS`, `tar --xattrs --sparse`.
3. **Round trip**: `ntfs-3g(ro)` → `tar`/`rsync` → plain dir, and → a second freshly
   `mkntfs`-formatted image via `ntfs-3g(rw)`, reopened with `ntfsinfo`.
4. SHA-256 for content identity throughout.

## Results

### Enumeration

- **Observed** — two offline read paths work, both without the kernel driver:
  1. **libntfs-3g userspace** — `ntfsls -R` recurses; `ntfscat` streams a file's unnamed
     `$DATA`; Unicode and space-containing paths handled; **no mount, no privilege, no
     FUSE**.
  2. **`ntfs-3g` FUSE `ro`** — standard POSIX tree; needed a user namespace here, would be
     a plain root mount in a privileged maintenance context.
- **Observed** — `ntfsinfo -F <path>` exposes per object: MFT record number + MFT record
  **sequence number**, hard-link count, parent-directory MFT reference, all four
  `$STANDARD_INFORMATION` timestamps plus the `$FILE_NAME` timestamps, DOS attribute flags
  (`ARCHIVE`/`SYSTEM`/`SPARSE_FILE`/…), Security ID (index into `$Secure`), and **every
  `$DATA` attribute — unnamed and named (ADS)** with resident/non-resident, data size,
  allocated size, compressed size, compression-unit.
- **Observed** — system metafiles (`$MFT`, `$Secure`, `$Bitmap`, `$UpCase`, `$Extend`, …)
  appear via `ntfsls -s`; a discovery pass must filter them.
- **Observed** — `ntfscat` on a directory fails cleanly (`Cannot find attribute type
  0x80`).
- **Observed** — a `ro` `ntfs-3g` mount plus full recursive content read plus full
  `getfattr` read left the image **byte-identical** (SHA-256 unchanged). A `rw` mount with
  no writes also left it byte-identical (clean volume).
- **Not tested** — kernel `ntfs3`; dirty / hibernated volumes (`ntfs-3g` refuses `rw`, `ro`
  needs `-o force`); corrupted volumes; large real volumes; enumeration performance and
  directory-count limits.
- **Inferred** — a privileged Linux maintenance context can enumerate an offline NTFS
  volume with either family; the userspace-tool path avoids the mount entirely and is the
  attractive base for a controlled capture component.

### Paths

- **Observed** — `stat` inode over `ntfs-3g` equals the MFT record number and is
  **identical across two simultaneous mounts of the same image at different mount
  points**. The MFT reference (record number + sequence number) is a volume-internal
  identity, independent of mount path and drive letter.
- **Observed** — FUSE reports `f_fsid = 0` for every mount → kernel `st_dev` is not a
  usable volume identity through `ntfs-3g`. The **NTFS volume serial** (64-bit, boot
  sector offset `0x48`; here `0x3e2e1ea174cd3810`) is readable offline and is
  mount-independent.
- **Observed** — name lookups are **exact-byte**. The NFC-stored Unicode file resolves for
  identical bytes / NFC and **fails** for the NFD form. Neither NTFS nor `ntfs-3g`
  normalises — a selection stored in a different normal form will not match.
- **Observed** — the fixture is case-sensitive (`CASETEST.TXT` ≠ `casetest.txt`), but only
  because the names are in the NTFS POSIX namespace. **Fixture artifact, not Windows
  behaviour.**
- **Not tested** — Win32-namespace case-insensitive lookup + case preservation; 8.3
  aliases; per-directory case-sensitivity flag; drive-letter / `\\?\` /
  `\Device\HarddiskVolumeN` resolution; long-path limits.
- **Inferred (for a future path contract)** — a durable selection identifier needs at
  least: (a) volume identity (volume serial, and filesystem GUID where present); (b) the
  path as an explicit **ordered sequence of name components** with exact code-unit encoding
  preserved (no normalisation, no case folding by Bamep); (c) optionally the MFT reference
  as a discovery-time corroborator — **not** a permanent identity (it is recycled — see
  Drift). A bare POSIX mount path or a `C:\` string is not sufficient identity.

### Reparse points

- **Observed** — the fixture symlinks are **`ntfs-3g`'s own representation**: a regular
  file with the `SYSTEM` bit whose unnamed `$DATA` is `IntxLNK\x01` + UTF-16LE target.
  They are **not** `$REPARSE_POINT` attributes; `ntfsinfo -F` shows no reparse attribute or
  flag.
- **Observed** — over the mount they present as ordinary POSIX symlinks (`readlink`
  works). `find` without `-L` does not traverse them. `cp -a --no-dereference` / `rsync
  -a` / `tar` archive them as links.
- **Observed** — a volume-absolute target is presented volume-relative
  (`/Users/Operator/Documents`), i.e. a dangling host-absolute path on the Linux mount; a
  capture that dereferences it would fail or escape the volume.
- **Observed** — a constructed directory loop did not hang `find` / `find -L` **only**
  because its target was a dangling host-absolute path. This is **not** evidence that loop
  protection exists.
- **Not tested (cannot construct here)** — genuine junctions, Windows symlink reparse
  tags, dedup / WOF / WIM-backed files, cloud placeholders, `\??\Volume{GUID}` targets,
  and `ntfs3` vs `ntfs-3g` differences.
- **Inferred — what Bamep must NOT assume**: that a "file" is plain content (a naive
  reader captures the `IntxLNK` blob as bytes); that link targets stay inside the volume;
  that recursive traversal across reparse points is safe (real junctions create genuine
  cycles and cross-volume escape); that `ntfs3` and `ntfs-3g` agree. A capture component
  must detect reparse/link objects explicitly and treat traversal as opt-in per object,
  never recursive-by-default.

### Metadata

Round trip = `ntfs-3g(ro)` → `tar --xattrs --sparse` / `rsync -aAHXS` → plain dir, and →
fresh `mkntfs` image.

| Property | Result |
|---|---|
| File content / SHA-256 | **content preserved** — all fixtures, both tools, including full NTFS → tar → fresh-NTFS round trip |
| Logical size | preserved |
| Modification time | **metadata preserved** to 100 ns (NTFS tick) |
| Access / MFT-changed time | observed via `system.ntfs_times*`; not round-tripped by generic tools (expected) |
| Creation time | **observed** via `getfattr system.ntfs_crtime[_be]`; **not** carried by cp/rsync/tar, **not** visible as `statx` btime over `ntfs-3g` — must be captured explicitly |
| DOS attribute bits | observed via `getfattr system.ntfs_attrib_be` (e.g. `0x20` ARCHIVE); not carried by generic tools |
| Alternate Data Streams | **preserved within the `ntfs-3g` toolchain**: each named `$DATA` is exposed as a `user.*` xattr; `tar --xattrs` / `rsync -X` carry it; extraction onto a fresh `ntfs-3g` image recreates the named `$DATA` (confirmed with `ntfsinfo`). Depends on every hop preserving `user.*` xattrs **and** on a stream-name mapping convention — fidelity was imperfect here (`ads:Zone.Identifier` prefixing). |
| Sparse allocation | **preserved** — 8 MiB logical / 4 KiB allocated survived `rsync -S`, `tar --sparse`, and the round trip; `ntfsinfo` shows sparse flag `0x8000` + compressed-size 4096 |
| NTFS security descriptor | **observed / capturable as an opaque blob** — `getfattr system.ntfs_acl` returns the raw self-relative `SECURITY_DESCRIPTOR`; per-file Security ID via `ntfsinfo`. `ntfssecaudit` **crashed** on this build (`free(): invalid pointer`). Remap onto a different install (different SIDs) not tested. |
| NTFS transparent compression | **Not tested for capture/restore** — the tested fixture-generation path (`ntfs-3g` FUSE, `setfattr system.ntfs_compression`, `chattr +c`) could not produce a genuinely NTFS-compressed file, so reading / capturing / restoring an already-compressed Windows-created file was never exercised |
| EFS-encrypted content | **Not tested** — could not be constructed without a Windows-created fixture and a user key; offline readability, preservation, and restore behaviour all unverified |
| Hard links | **Not tested** — not constructed |
| Object ID / `$UsnJrnl` / `$LogFile` | **Not tested / unavailable** — left empty by `mkntfs`/`ntfs-3g` |

Distinctions to carry: *content preserved* holds broadly; *metadata observed* (creation
time, DOS bits, SD blob, Security ID) requires **explicit** capture — no generic copy tool
records it; *metadata preserved through round trip* was shown only for mtime, sparseness,
and (within the `ntfs-3g` + tar/rsync toolchain) ADS; *metadata reconstructable* (e.g. SD
remapped to a new SID namespace) was **not** demonstrated for anything.

### Arbitrary selection

- **Observed** — a 4-entry operator selection mixing one directory
  (`\PortableApps\DefragTool`) and three files in deliberately heuristic-hostile locations
  (portable executable; game save under `_cache`; `license.key` under `\Windows\Temp`) was
  resolved (directory → its 2 files), captured object-by-object with `cp -a
  --no-dereference`, and every captured object's SHA-256 matched the source. No categories,
  profiles, or extension rules involved.
- **Observed** — resolution naturally yields three lists: *requested* (operator text),
  *resolved* (concrete objects, with a `MISSING` marker for entries that do not resolve),
  *captured* (per object: path, size, digest, or `FAILED`).
- **Inferred (scoped to the tested fixture and object classes)** — arbitrary *ordinary*
  file/directory selections in this Linux-created NTFS fixture were representable and
  capturable as `{volume identity} + {list of volume-relative path-component sequences} +
  {per-object observed facts}`, with no heuristic involved. This was **not** exercised for
  genuine Win32-namespace names, 8.3 aliases, real reparse points, Windows-created case
  behaviour, or other Windows-only object forms — those remain open (see Limitations).
- The separate, already-accepted product invariant — heuristics may *propose* but must not
  silently override an explicit operator inclusion — is not established or altered by this
  Spike; the evidence here only shows that an explicit resolved list is a workable
  representation for the tested cases.

### Discovery-to-capture drift

Sequence: discover + record facts → mutate via `rw` remount → re-observe. Mutations:
same-size content edit; in-place byte patch; delete; pathname reused for a new file; a
selected path that did not exist at discovery and appears later; metadata-only `mtime`
change; and a **forged-metadata** case (same-size content edit followed by `touch -m -a`).

| Signal | Verdict (this tooling) |
|---|---|
| Existence at path | **strong** for delete / late-create; necessary, not sufficient |
| MFT reference (record # + sequence #) | **strong** for "same path, different object" — the reused-name case moved MFT # 72 → 75. Useful but insufficient alone: unchanged on in-place edits, **and MFT numbers are recycled** |
| Size | useful, insufficient — misses same-size edits |
| mtime (`$STANDARD_INFORMATION`) | useful; oversensitive (fired on a content-identical `touch`), so safe-direction — **but forgeable**: after `touch -m -a`, size + mtime + creation time were all identical while SHA-256 differed → **cheap metadata gave a false "unchanged"** |
| Creation time | corroborating only — unchanged by content edits, changed on name reuse |
| `$STANDARD_INFORMATION` MFT-changed time | moved on the forged-mtime edit (not settable via `utimes()`), so tamper-evident against casual tooling — but reset by any later legitimate access, not authoritative |
| Full SHA-256 of the observed content | **detected every content mutation exercised** — same-size edit, in-place byte patch, and the forged-metadata case (identical size + mtime + creation time) |
| NTFS USN change journal (`$UsnJrnl`) | **unavailable** here (empty) — potentially the strongest cheap Windows signal, untested |

- **Inferred (scoped to the content-change question tested here)** — none of the cheap
  metadata tuples exercised (size, mtime, creation time, MFT reference, MFT-changed time)
  proved that a file's content was still the content seen at discovery; a same-size edit
  with forged timestamps defeated all of them together. A **full SHA-256 digest of the
  observed content** did detect every content mutation exercised. This Spike did **not**
  test a bounded / sampled digest — a sample can miss a mutation outside the sampled
  regions, so no sampled scheme is claimed here. Nor is a full content digest claimed to be
  sufficient for other staleness dimensions (directory membership, streams, metadata,
  reparse semantics) — those are separate and were not covered by this signal.
- **Inferred** — the cheap metadata comparisons are still useful as a *fast-fail* and as
  corroboration; they reduce, not eliminate, the content re-reading needed before
  destructive continuation. "Same path + same size + same mtime" must **not** be treated as
  "unchanged".

### Captured-result evidence

- **Observed** — the minimum facts that let a later contract separate the states:
  - *requested*: operator input text (path expression + required/optional intent),
    timestamped.
  - *resolved*: per requested entry — concrete objects each with volume identity,
    path-component sequence, MFT reference, type (file / dir / link / reparse / other),
    size; a `MISSING`/`AMBIGUOUS` marker where resolution fails; plus discovery timestamp
    and volume serial.
  - *encountered*: what the capture pass actually saw (may differ from *resolved* — see
    Drift), same per-object facts re-observed.
  - *captured*: per object — bytes + content digest + which streams/attributes were
    included, or a **typed** failure (unreadable / unsupported-type / vanished /
    changed-since-discovery).
  - *verified*: per object — recomputed digest matches captured digest; and, separately,
    the container/Artifact-level integrity result.
- **Observed (critical)** — verifying the **container** (archive/Artifact digest matches)
  proves the written bytes are intact; it does **not** prove the object set inside equals
  the resolved selection. In the granularity test a `tar` completed with exit 0 and a
  valid archive while silently containing only synthesised link objects and directories — a
  clean container digest would still "verify". Proving *"every intended object is
  represented"* needs an explicit per-object expected-vs-present reconciliation the
  container digest cannot provide.

### Artifact-granularity pressure

- **Observed** — a `tar` over a group mixing ordinary files with reparse/link objects and
  a directory loop **succeeds** (exit 0, valid archive); it neither fails the group nor
  flags the anomalous objects. A single "atomic Selective Artifact" spanning heterogeneous
  groups can therefore be cryptographically `Verified` while being semantically incomplete
  or containing objects that will not restore.
- **Observed** — splitting into independent per-group captures (group A ordinary docs;
  group B links / loop / game data) gave independent success/failure, per-group
  verification, and a natural retry/skip boundary — group A's evidence is unaffected by
  group B's problems.
- **Inferred (evidence only, not a rule)** — the atomic-integrity guarantee Bamep already
  requires for an Artifact is necessary but not sufficient for Selective; whatever the
  final mapping, a **per-object manifest reconciliation** layer is required, and grouping
  the capture so an unsupported/failed object isolates to the smallest useful unit
  materially improves failure isolation, retry, and restore correlation. The *number* of
  Artifacts is a separate decision; the need for sub-Artifact per-object accounting is the
  empirical finding.

### Restore prerequisites

- **Observed** — round trip into a fresh `mkntfs` image preserved: content, mtime,
  sparseness, ADS (recreated named `$DATA`), and symlink objects.
- **Observed / lost unless explicitly captured now**:
  - the **original NTFS path** (volume-relative component sequence) and *which volume* —
    the archive path is whatever the capture tool chose;
  - **creation time** and **DOS attribute bits** — not in cp/rsync/tar output;
  - the **security descriptor** and its owning-SID context — capturable only as an opaque
    blob, meaningless on a fresh install without an SID-remapping decision;
  - **reparse semantics** — the `IntxLNK`/target intent, and on real data the reparse tag
    and raw reparse buffer;
  - **provenance** — source volume serial, discovery + capture timestamps, endpoint
    context, and the operator's requested-vs-resolved intent.
- **Not tested** — restore onto a real Windows installation; profile / `%USERPROFILE%` /
  SID mapping; ACL reapplication; junction re-creation; restore of compression / EFS.
- **Inferred** — a restore contract needs, captured **at capture time** and stored with
  the bytes: (1) volume identity + volume-relative path component sequence per object,
  exact encoding; (2) object type + reparse/link intent; (3) the metadata not in generic
  archives (creation time, DOS bits, SD blob + owner SID) even if restore later chooses to
  drop it; (4) provenance; (5) a per-object digest set. What the source volume no longer
  holds cannot be captured later — under-capturing metadata now is irreversible.

## Candidate comparison

| | libntfs-3g userspace tools | `ntfs-3g` FUSE (`ro`) | kernel `ntfs3` |
|---|---|---|---|
| Root required | **no** | mount needs root / setuid / a userns | yes (mount) |
| Kernel driver | no | no (FUSE) | yes |
| Enumeration | yes (`ntfsls`) | yes (POSIX) | not tested |
| Content read | yes (`ntfscat`, unnamed stream) | yes | not tested |
| ADS | visible in `ntfsinfo`; extract needs `-i inode -n name` | clean via `user.*` xattr | not tested |
| Full metadata (SD, crtime, DOS bits) | `ntfsinfo` / `ntfssecaudit` (latter crashed) | `getfattr system.ntfs_*` | not tested |
| Standard tooling (`tar`, `rsync`, `find`) | no — must script around the tools | **yes** | yes |
| Write / round-trip target | `ntfscp` (limited) | yes | not tested |
| Read-only safety | inherent (no mount) | confirmed byte-identical | not tested |

Only two candidates were exercised, because they materially differ (mount vs no-mount,
privilege). They are the **same library family** — not independent implementations. Kernel
`ntfs3` is **untested** and is the most likely divergence for reparse points, compression,
case-insensitivity, and dirty volumes.

## Findings safe to carry forward

1. Offline NTFS enumeration + content capture is feasible on the Linux maintenance side
   with no kernel NTFS driver, and — via the userspace libntfs-3g tools — with no
   privilege at all. A privileged context additionally unlocks `ntfs-3g` FUSE plus
   standard `tar`/`rsync`.
2. A read-only `ntfs-3g` mount, and the userspace tools, did not modify the (clean) image.
3. Mount-path- and drive-letter-independent object identity is available: volume serial +
   exact path-component sequence, with the MFT reference as a discovery-time corroborator.
4. In the tested Linux-created fixture, arbitrary *ordinary* file/directory selections —
   including a portable executable, a game save under `_cache`, and a `license.key` under
   `\Windows\Temp` — were explicitly selectable and capturable with no heuristic, using
   volume-relative path-component sequences plus per-object observed facts. Not generalised
   to Windows-only object forms.
5. ADS and sparseness can be represented and round-tripped **within the `ntfs-3g` +
   tar/rsync toolchain**; creation time, DOS attribute bits, and the security descriptor
   are readable but are **not** captured by generic copy tools and must be captured
   explicitly.
6. For the content-change question tested: no cheap metadata tuple proved a file's content
   was still fresh — a same-size edit with forged timestamps defeated size + mtime +
   creation time together, while a full SHA-256 of the observed content caught every
   content mutation exercised. Sampled digests were not tested and are not claimed; other
   staleness dimensions (membership, streams, metadata, reparse) are separate.
7. A clean container/Artifact digest does not prove the captured object set equals the
   resolved selection — per-object manifest reconciliation is a distinct requirement.
8. Isolating an unsupported/failed object to the smallest useful capture unit materially
   improves failure isolation and retry; the Artifact count itself is undecided.

## Unsupported / negative evidence (silent-data-loss risks)

- `ntfs-3g` stores symlinks as `IntxLNK`-prefixed regular files; a naive content reader
  captures that blob **as file content**, and a naive recursive walker may follow
  volume-absolute targets out of the volume. Genuine junctions/reparse tags were not
  testable and must be assumed to create real traversal cycles and cross-volume escape.
- `cp` / `rsync` / `tar` silently drop NTFS **creation time**, **DOS attribute bits**, and
  the **security descriptor**. Capture built on a generic archiver loses them with no
  error.
- NTFS **transparent compression**: the tested fixture-generation path could not create a
  genuinely compressed file, so capture / restore of already-compressed Windows-created
  data is **Not tested** — its behaviour is unknown, in either direction.
- **EFS**: **Not tested** — no Windows-created fixture and no user key were available.
  Offline readability, preservation semantics, key requirements, and usable restore
  behaviour are all unverified and need Windows-created-fixture evidence. Whether the NTFS
  `ENCRYPTED` attribute is observable from the tested tooling was not established.
- The NTFS **USN change journal** and **object IDs** were empty/unavailable — the
  potentially strongest cheap staleness signal is unproven.
- ADS stream-name fidelity was imperfect (`ads:` prefixing) in the tested xattr
  convention; a lossy stream-name mapping risks collisions / omissions.
- `ntfssecaudit` crashed on this build — the one bundled SD-audit tool is unreliable here.

## Limitations

- **No Windows-created NTFS fixture.** Every Windows-specific behaviour — Win32 namespace,
  case-insensitivity + preservation, 8.3 names, real SIDs / ACLs, genuine reparse points,
  compression, EFS, USN journal, hibernation / dirty state — is **Not tested**. A
  Linux-created (`ntfs-3g`, POSIX-namespace, mapped-root) fixture cannot stand in for
  these. Follow-up empirical work on a genuine Windows-created (and dirty / hibernated)
  volume is tracked by Issue #47.
- **No root, no loop devices, no `mount(8)`** in the Spike environment. Kernel `ntfs3` was
  not exercised at all. `ntfs-3g` mounting required a `podman unshare` user namespace; a
  `podman unshare` that exits can leave the `ntfs-3g` FUSE server alive holding the image.
- No large / real volume — no evidence on enumeration performance, memory, directory-count
  limits, or real-world reparse / ADS / compression prevalence.
- Single host, single tool family, single day; versions as recorded above.
- No restore onto real Windows; SID / profile remapping unexamined.

## Remaining decisions (owner / Specification / ADR — not decided here)

- The Selective **path / identity contract**: exact fields (volume serial vs FS GUID vs
  partition identity), encoding / normalisation / case rules, whether the MFT reference is
  stored.
- Which **metadata classes are in scope** for Selective capture and restore (creation
  time, DOS bits, SD + SID-remapping policy, reparse, compression, EFS) and which are
  explicitly refused / flagged.
- The **staleness contract**: what must be re-verified (and how much content re-digested)
  between discovery and destructive continuation, and whether USN-journal evidence is
  required.
- **Reparse-point policy**: capture-as-object vs follow, and the recursion / escape guard.
- **Artifact granularity** for Selective and the **per-object manifest reconciliation**
  representation (requested / resolved / encountered / captured / verified).
- Which mechanism is sanctioned — kernel `ntfs3`, `ntfs-3g`, or the userspace libntfs-3g
  tools — pending a privileged-environment comparison that includes dirty volumes.
- Restore contract and SID / profile mapping onto a fresh Windows install.
- **A follow-up Spike on a genuine Windows-created (and a dirty / hibernated) NTFS volume**
  is required before a Selective Specification can promise reparse, case, ACL, compression,
  EFS, or USN behaviour. Tracked by Issue #47.

## Reproduction

Environment: Fedora 44, kernel 7.1.10, `ntfs-3g` / `libntfs-3g` 2026.2.25, `podman`
(rootless, `/etc/subuid` range present), GNU `tar` 1.35, `attr`. Spike scripts live in the
session scratchpad and are **not** committed: `build-fixture.sh`, `exp-readonly.sh`,
`exp-drift-restore.sh`, `exp-e6b.sh`, `ro-safety.sh`.

1. `truncate -s 96M fixture.img && mkntfs --fast --force --label X fixture.img`
   (works on a plain file — no loop device, no root).
2. Populate:
   `podman unshare bash -c 'ntfs-3g -o streams_interface=xattr fixture.img mnt && … && fusermount3 -u mnt'`.
3. Inspect without mount / root: `ntfsls -R -p / fixture.img`,
   `ntfsinfo -F /path fixture.img`, `ntfscat fixture.img /path`.
4. Read-only capture:
   `podman unshare bash -c 'ntfs-3g -o ro,streams_interface=xattr fixture.img mnt && tar -C mnt --xattrs --sparse -cf out.tar <selection> && fusermount3 -u mnt'`.
5. Cleanup: `fusermount3 -u`, then
   `ps -eo pid,comm | awk '$2=="ntfs-3g"{print $1}' | xargs -r kill`.

## Related

- `docs/discovery/m2-composite-service-workflow-and-operator-intervention.md` §B — the
  questions this Spike was scoped to.
- `docs/specifications/m0-data-plane-and-storage-contracts.md` — Artifact atomic
  integrity/completeness, `capture_consistency`, and the note that a future Selective
  workflow may use multiple independent Artifacts.
- ADR-0008 — data-plane transport / chunking / resumability rationale.
- ADR-0020 — planned-intervention checkpoint and execution-capacity separation (not
  reopened by this Spike).
- `docs/reference/transfer-resumability-spike.md` — companion evidence; the
  detect-change ≠ reproduce-capture distinction applies here too.
- Issue #47 — follow-up empirical work on Windows-created NTFS (namespace/case, genuine
  reparse points, real ACL/SID, compression, EFS, USN journal, dirty/hibernated volumes,
  `ntfs3` vs `ntfs-3g`); owns that work's scope, dependencies, and fixture procedure.
