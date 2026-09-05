# Issue #60 — Physical WinPE Agent Control-Plane / Capture-Source Spike Harness

Authored, **throwaway** Spike artifacts for Issue #60. They proved, checkpoint
by checkpoint, the first physical vertical slice:

```text
physical mini PC → UEFI PXE + Secure Boot → network-delivered WinPE
→ minimal WinPE-native Bamep-owned probe
→ TLS 1.3 + WSS (real AgentTransportAcceptor)
→ Agent Protocol v1 authentication (real AgentControlGateway + EnrollmentService)
→ InventoryReport (real InventoryService)
→ #59 capture-source projection
→ durable InventoryRevisionId on real PostgreSQL
→ stop
```

The Spike intentionally stops **before** `bamep.m2.endpoint-capture-transfer`,
any bulk disk read, chunk transfer, Artifact creation, restore, or Web/Admin
submission. Physical capture/transfer remains separate follow-up work.

## Authoritative status (read this first)

- **This is a preserved physical-integration harness/probe.** It exists to
  reproduce or extend physical evidence, not to ship.
- **It is NOT the production Agent architecture.** The probe defines no
  production Agent packaging, update, service, or crate model. Issue #60
  forbids turning it into a permanent `crates/agent`. A production Agent
  binary/crate is separate, owner-gated work (ADR-0003).
- **It is NOT the `bamepd` production composition root.** `harness/` is a
  *narrow* standalone-workspace binary that path-depends on `bamep-server`
  and composes the **existing** `AgentTransportAcceptor` +
  `AgentControlGateway` + `EnrollmentService` / `BootstrapEvidenceService` /
  `InventoryService` + real PostgreSQL, for the experiment only. It does not
  expand the deliberately-partial `bamepd` composition root and must not
  become it. Wiring the real Agent WSS listener into `bamepd` is a separate,
  owner-gated composition-root WP.
- **Storage enumeration performed by this Spike is read-only.** Every disk
  handle is opened with zero desired access (`CreateFileW(..., 0, ...)`);
  only read-only query IOCTLs are issued; the probe structurally cannot read
  or write disk contents.
- **Transient `\\.\PhysicalDriveN`, model, serial, bus type, and size are
  lab evidence only.** They are recorded solely so a run can be reproduced
  and reviewed; they are never a cross-boundary identity.
- **Cross-boundary source authority remains the #59 opaque
  source-observation model:** `capture_source_observation_id` (the
  `boot_nonce` idiom — 32 CSPRNG bytes, base64url, 43 chars) plus opaque
  `agent_source_id` values unique within that epoch. Nothing else about a
  source crosses the boundary.
- **Not authoritative for findings.** Attempt-by-attempt evidence and the
  final A/B/C/D classification (outcome **A**) live on Issue #60.

## Layout

```text
issue-60-winpe-agent-slice/
├── README.md
├── .gitignore
├── probe/    — bamep-winpe-probe  (WinPE-native x86-64 evidence probe)
│   ├── src/main.rs      NDJSON logging; CP2 exec, CP3 pinned TLS 1.3 + WSS,
│   │                    CP4 real AuthRequest, CP5/CP6 source epoch + InventoryReport
│   ├── src/pinned.rs    pinned Server-leaf-cert TLS 1.3 client verifier
│   └── src/sources.rs   read-only PhysicalDriveN enumeration (zero-access handles)
├── harness/  — bamep-physint-harness  (Server-side, standalone workspace)
│   ├── src/main.rs      `serve` (CP4 gateway+PG), `provision` (mint credential),
│   │                    `selftest` (CP3 transport-only)
│   ├── src/pinned.rs    selftest client verifier
│   └── src/evidence.rs  NDJSON logger
└── sink/     — bamep-probe-sink  (Fedora-side NDJSON collector, plain TCP)
```

Only authored source, the three `Cargo.lock` files, `README.md`, and
`.gitignore` are preserved. `target/`, `evidence/` (execution evidence),
`smb-share/` (generated CP-specific plumbing + credential), and any `*.exe` /
`*.der` / `*.pem` / `*.key` / `*.ndjson` are git-ignored runtime output — see
**Security** below.

## Known platform/tooling limitation (non-blocking)

`IOCTL_DISK_GET_LENGTH_INFO` returns failure / `size_bytes = 0` when the disk
handle is opened with zero desired access on this WinPE build, so the probe
records no disk size. This is a **platform/tooling limitation of the
zero-access read-only enumeration path**, not a defect and not an Issue #60
blocker: disk size is lab evidence only, is not part of the #59 cross-boundary
contract, and the `IOCTL_STORAGE_QUERY_PROPERTY` device descriptor
(vendor/product/serial/bus) is retrieved fine on the same handle. A future
production Agent that needs disk size can open with `GENERIC_READ` (still no
write) or use a size-capable query that does not require it.

## Build

Host: Fedora, no root required.

Probe (owner-approved toolchain: `cargo-xwin` + `x86_64-pc-windows-msvc`,
static CRT, no external Visual C++ Runtime dependency):

```bash
rustup target add x86_64-pc-windows-msvc
rustup component add llvm-tools-preview          # provides llvm-ar
ln -sf "$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-ar" ~/.local/bin/llvm-lib
ln -sf "$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-ar" ~/.local/bin/llvm-dlltool
cargo install cargo-xwin

cd probe
export PATH="$HOME/.local/bin:$PATH" XWIN_ACCEPT_LICENSE=1   # accepts Microsoft's CRT-headers licence
RUSTFLAGS="-C target-feature=+crt-static" \
  cargo xwin build --release --target x86_64-pc-windows-msvc
```

The `LNK4099` "cannot use debug info … .pdb" warnings are expected (the xwin
CRT ships no PDBs). Resulting `bamep-winpe-probe.exe` PE imports:
`kernel32 / ntdll / ws2_32 / advapi32 / bcrypt / bcryptprimitives /
api-ms-win-core-synch-l1-2-0` — all present in stock WinPE 10.0.26100; no
UCRT or `VCRUNTIME140.dll`.

Harness / sink (native):

```bash
cd harness && cargo build --release
cd ../sink  && cargo build --release
```

## Run (isolated #53 lab link, Fedora = lab Server, `enp8s0` = 192.168.99.1)

### 1. PostgreSQL

A non-superuser role that owns its own database is enough — `createuser -s`
is **not** required, and `pg_hba.conf` is not touched.

```bash
createdb bamep_physint_spike          # owned by the current OS user
```

The harness connects, with no env var, via a peer-auth Unix-socket DSN
derived for the current OS user
(`postgresql://$USER@%2Frun%2Fpostgresql/bamep_physint_spike`). Override with
`BAMEP_PHYSINT_DB_URL` for anything else (a value there may carry a password;
the harness only ever logs scheme/host/database).

### 2. Server-side services (Fedora)

```bash
cd integration/physical/issue-60-winpe-agent-slice
mkdir -p evidence                     # shell redirections below need it (git-ignored)

setsid nohup harness/target/release/bamep-physint-harness serve 192.168.99.1:8443 \
  > evidence/harness.serve.log 2>&1 < /dev/null & disown
setsid nohup sink/target/release/bamep-probe-sink 192.168.99.1:9099 \
  evidence/probe-events.ndjson > evidence/sink.stdout.log 2>&1 < /dev/null & disown

# The server prints its leaf-cert SHA-256 fingerprint on startup, and also
# writes it to evidence/harness-fingerprint.txt.
FP=$(cat evidence/harness-fingerprint.txt)

# Mint a disposable enrollment credential (24h TTL) into smb-share/ (git-ignored,
# owner-only mode). Re-run before each physical attempt.
harness/target/release/bamep-physint-harness provision physint-lab
```

### 3. Stage probe + credential for WinPE over a temporary read-only SMB share

Stock WinPE has no `curl` / `certutil` / `where`, so delivery is via SMB
(the Windows SMB redirector always uses TCP 445, a privileged port). Run the
share server as root — do **not** change `net.ipv4.ip_unprivileged_port_start`:

```bash
mkdir -p smb-share
cp probe/target/x86_64-pc-windows-msvc/release/bamep-winpe-probe.exe smb-share/
# smb-share/agent-credential.txt was written by `provision` above.

# impacket smbserver, run as root; PYTHONPATH points at the invoking user's
# --user site-packages (adjust the python version):
sudo env "PYTHONPATH=$HOME/.local/lib/python3.14/site-packages" \
  "$HOME/.local/bin/smbserver.py" -readonly -smb2support -ip 192.168.99.1 \
  PROBE "$PWD/smb-share"
```

Neither `smb-share/` nor its contents are ever tracked (`.gitignore`).

### 4. In WinPE — direct probe invocation (CP6: auth + InventoryReport)

```bat
net use \\192.168.99.1\PROBE
copy /Y \\192.168.99.1\PROBE\bamep-winpe-probe.exe X:\
copy /Y \\192.168.99.1\PROBE\agent-credential.txt X:\
X:\bamep-winpe-probe.exe --sink 192.168.99.1:9099 --wss 192.168.99.1:8443 --pin <FP> --auth-credential-file X:\agent-credential.txt --inventory-report
echo EXITCODE=%ERRORLEVEL%
```

`<FP>` is the fingerprint from step 2. Drop `--inventory-report` for CP4/CP5,
drop `--auth-credential-file` too for CP3, drop `--wss`/`--pin` for CP2.

Probe exit codes: `0` ok · `2` local-file sink failed · `3` TLS/WSS crossing
failed · `4` authentication not established · `5` inventory-report sequence
incomplete (including a #59 RF-4 duplicate-`agent_source_id` epoch, which is
never sent).

## Security

Never commit:

- `evidence/harness-cert.der`, `evidence/harness-key.pkcs8.der` — lab
  self-signed Server key material;
- `smb-share/agent-credential.txt` — a live (disposable) enrollment bearer
  credential;
- `*.exe` — generated binaries;
- WinPE WIM / EFI binaries / captured disk contents (never produced here).

The `.gitignore` already excludes all of the above. The probe opens every
disk handle with **zero desired access** (`CreateFileW(..., 0, ...)`) and
issues only read-only query IOCTLs; it structurally cannot read or write disk
contents.

## Reuse

Lab-specific: default addresses `192.168.99.1:{8443,9099,8080}`, the isolated
link, and the disposable disks all come from the #50/#52/#53 environment. Any
reuse needs owner review. Promotion of any of this into a durable Reference,
a production Agent WP, or a `bamepd` composition WP is a separate, owner-gated
decision recorded on Issue #60.
