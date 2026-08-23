# Transfer/Snapshot Resumability — Local Empirical Evidence

Status: **Completed empirical reference.**

This document preserves the validated local evidence from the resumability spike. It does not
define the current Bamep data-plane contract; ADR-0008 owns the design rationale and
`docs/specifications/m0-data-plane-and-storage-contracts.md` owns normative transfer,
Artifact, chunk, manifest, and source-consistency behavior.

## Question

Which transfer strategies provide honest resumability and integrity when large image/volume data
may need to resume after interruption, especially when a producer cannot reproduce an arbitrary
byte range identically after source mutation or transformation?

## Environment and scope

All experiments used disposable local files only.

Environment:

- Windows 11;
- MSYS2/git-bash (MINGW64);
- coreutils **8.32**;
- gzip **1.14**;
- OpenSSL **3.5.4**;
- POSIX-style tools including `dd`, `split`, `sha256sum`, and `gzip`.

The synthetic source was **32 MiB** of OpenSSL-generated pseudo-random data. It was intentionally
incompressible, so this spike produced no compression-ratio evidence.

No real disks, network transport, HTTP Range behavior, or production backup format were tested.

## Experiments

### A — Byte-offset resume from a static source

A partial copy of the unchanged source was resumed by reading the source again from the exact
byte offset.

Result: **success**.

The reconstructed file matched the source SHA-256 exactly.

Evidence: byte-offset resume is valid when the source can deterministically reproduce identical
bytes at the requested offset.

### B — Byte-offset resume from a regenerated transformed stream

The 32 MiB source was gzip-compressed. The first 60% of that compressed stream was treated as
already delivered. Four source bytes near the beginning were then changed, gzip was run again,
and the tail of the newly generated compressed stream was appended to the old head.

Measured original gzip size:

**33,559,581 bytes**

The mixed artifact:

- failed `gunzip -t` with CRC and length errors;
- still produced decompressed output when decompression was attempted without treating those
  integrity errors as authoritative;
- produced **33,554,434 bytes**, versus the correct **33,554,432 bytes**.

Result: **unsafe failure**.

This demonstrated that raw byte-offset resume across two generations of an unframed continuous
transformed stream can produce an approximately plausible but incorrect artifact.

The evidence applies to the tested plain gzip-style continuous stream. Explicitly seekable or
independently framed compression formats were not tested.

### C — Fixed-size content-addressed chunks

The source was split into:

- **8 chunks**;
- **4 MiB per chunk**;
- one SHA-256 manifest entry per chunk.

Simulated destination state:

- 5 correct chunks;
- 1 delivered chunk corrupted by overwriting 4 bytes;
- 2 missing chunks.

The resume pass hashed destination chunks against the original manifest.

Result:

- exactly the 1 corrupt + 2 missing chunks were fetched again;
- the 5 valid chunks were retained;
- the reassembled Artifact matched the source SHA-256 exactly.

Evidence: per-chunk hashes support corruption detection and selective retransmission while the
original chunk bytes remain reproducible or durably available.

### D — Independent per-chunk compression

Each 4 MiB logical chunk was independently gzip-compressed and the manifest hashed the compressed
chunk bytes.

The same missing/corrupt delivery pattern from Experiment C was applied.

Result:

- exactly the 3 affected compressed chunks were fetched again;
- every chunk independently passed `gunzip -t`;
- independently decompressing and concatenating the chunks reproduced the original source
  SHA-256 exactly.

Evidence: chunking and compression can preserve independent resume/integrity boundaries when
compression is applied per chunk, or equivalently when a format provides independently decodable
verified frames.

This experiment used an unchanged source and therefore did not establish source-mutation safety.

### E — Missing chunk regenerated after source mutation

The original 32 MiB source and 8-entry manifest were created. `chunk_006`, representing source
range:

```text
[25,165,824, 29,360,128)
```

was treated as never delivered.

Eight bytes were then changed at absolute source offset:

**26,165,824**

which lies inside `chunk_006`.

The missing chunk was regenerated from the changed source with:

```text
dd skip=24 count=4 bs=1M
```

Result: **the regenerated chunk did not match the original manifest hash**.

A separate split of the changed source produced the same regenerated `chunk_006` hash
(`61f73ca4...`) as the `dd` path, confirming the mismatch came from source mutation rather than
regeneration error.

Evidence:

- the manifest correctly detected that the original logical chunk could no longer be reproduced;
- chunking detected the inconsistency but could **not** complete the original Artifact from the
  changed source.

This separated two guarantees that must not be conflated:

```text
detect changed/corrupt bytes != reproduce the original capture
```

## Empirical conclusions

The experiments established:

1. Byte-offset resume requires byte-for-byte source reproducibility at the resumed range.
2. Raw offsets into a regenerated unframed transformed stream are not safe resume boundaries.
3. Per-chunk content hashes enable selective retransmission and early corruption detection.
4. Per-chunk compression preserves independent verified resume units in the tested model.
5. Chunking does not solve mutable-source consistency; a missing original chunk still requires
   reproducible or durably staged original bytes.
6. Detection of source mutation is not equivalent to successful completion of the original
   Artifact.

These findings informed ADR-0008 and the data-plane Specification. Those documents own the
resulting Bamep requirements.

## Limits

Not established by this spike:

- an optimal chunk size — **4 MiB was arbitrary experimental convenience**;
- a production digest choice — SHA-256 was used for readily available tooling;
- real-world compression ratios;
- actual live block-device behavior;
- frequency of source mutation on Windows endpoints;
- network reconnect/range-request mechanics;
- a specific snapshot/quiescing/staging mechanism;
- explicitly seekable/framed compression alternatives;
- per-file Selective-backup behavior.

Experiment E shows why some source-consistency mechanism is necessary but does not compare or
select stable snapshots, quiescing, durable chunk staging, or another mechanism.

## Related

- ADR-0008 — data-plane transport/chunking/resumability decision rationale.
- `docs/specifications/m0-data-plane-and-storage-contracts.md` — normative chunk, manifest,
  Artifact, resume, verification, and source-consistency contract.
- `docs/specifications/m0-simulator-contract-and-validation-strategy.md` — Simulator scenarios
  that consume the source-mutation evidence.
