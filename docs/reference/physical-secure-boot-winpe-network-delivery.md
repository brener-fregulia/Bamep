# Physical Secure Boot Network-Delivered WinPE — Mini-PC Evidence

## Status and scope

**Completed empirical physical Integration Environment reference**, produced by
Issue #53.

This document preserves reusable physical evidence only. It does not select a
production artifact-versioning or bundle-activation scheme, does not establish
trusted bootstrap, and does not claim portability beyond the one tested
Endpoint and firmware. Issue #53 remains the execution-history authority; this
Reference keeps only the findings that remain useful after the Issue is
closed.

Fixture, continued from `docs/reference/physical-uefi-pxe-boot-chain.md`
(Issues #50/#52):

- one physical UEFI x86-64 mini-PC Endpoint, PXE NIC MAC `e8:ff:1e:d6:2e:f5`;
- Endpoint physically diskless for this Spike (M.2 SATA SSD and HDD removed);
- direct, isolated `Fedora 44 Server enp8s0 ↔ Endpoint` link, no switch in the
  path;
- one tested physical firmware/NIC environment only.

## Physical Secure Boot state

Owner-confirmed physical firmware state for the fixture, immediately before
this Spike's runs:

- System Mode: **User**;
- Secure Boot: **Enabled**;
- Secure Boot status: **Active**;
- Secure Boot Mode: **Standard**;
- factory/default keys restored immediately before enabling Secure Boot.

Every successful/attempted chain below ran with iPXE's own `show
efi/SecureBoot` reporting `efi/SecureBoot:hex = 01` at the point each script
reached it, corroborating that Secure Boot remained enabled and active for
the whole physical run, from the firmware's own executing second-stage
loader, not only from pre-boot configuration intent.

No more specific firmware-security semantics (db/dbx contents, revocation
state, PK/KEK provenance beyond "factory/default") were observed or are
claimed here.

## Successful mechanism family

```text
UEFI PXE
→ official Secure Boot iPXE shim (snponly-shim.efi)
→ shim DEFAULT_LOADER fallback request for /ipxe.efi at the TFTP root
→ official SNP-only iPXE second stage (snponly.efi), served under /ipxe.efi
→ iPXE v2.0.0 (g12798), automatic ipxeboot/x86_64-sb/autoexec.ipxe
→ HTTP
→ wimboot v2.9.0
→ external pristine BCD + boot.sdi + stock boot.wim (same ADK WinPE media
  lineage)
→ Windows Boot Manager
→ WinPE
→ wpeinit
→ X:\Windows\System32\cmd.exe
```

This chain physically completed with Secure Boot enabled the whole time, on
the diskless Endpoint, over the isolated provisioning link.

The observed shim fallback to a root-level `/ipxe.efi` is preserved as
**compatibility evidence for this exact tested shim/iPXE build and this exact
physical firmware**, not as a universal Bamep TFTP-layout requirement. It
reproduces the interoperability class reported upstream in ipxe/ipxe#1684:
on firmware where the shim cannot read its own `LoadedImage.FilePath`, it
cannot derive its sibling second-stage filename and falls back to its
compiled `DEFAULT_LOADER` (`ipxe.efi`) at the TFTP root instead of the
directory it was itself loaded from.

## Material tested artifacts

All values below were independently re-verified against the retained
evidence (`sha256sum` of the served/captured files, and independent
reassembly of the HTTP payloads directly from the packet capture — see
Transfer evidence).

| Artifact | Version | Size (bytes) | SHA-256 |
| --- | --- | --- | --- |
| iPXE release | v2.0.0 (`g12798`) | 12,002,760 (`ipxeboot.tar.gz`) | — |
| `snponly-shim.efi` | from the v2.0.0 release bundle | 1,038,920 | `83ad71c7d4f2cf328b75b653d09bf3bea5f29bee2e67ca058f37d83c07133885` |
| `snponly.efi` | from the v2.0.0 release bundle | 295,784 | `b1e67c3e4a1e8708ddfd0079ad4505e3a02245acb55ee9a95437ab3c507be82a` |
| `wimboot` | v2.9.0 | 76,064 | `5f067ccdc4d084d5bf77b6c853bd0f8402dfc2b4cd1b103d358993ae97fae8e3` |
| `BCD` | from the same ADK WinPE media tree | 262,144 | `c0fd865ab0a1329d333ee6d3ab48c3030851a193a939d8b382522d40c81eea41` |
| `boot.sdi` | from the same ADK WinPE media tree | 3,170,304 | `cd2c00ce027687ce4a8bdc967f26a8ab82f651c9becd703658ba282ec49702bd` |
| `boot.wim` | WinPE image, DISM-reported version `10.0.26100` | 340,134,390 | `fbcbdb1c6651ab3a69384e9d4f95f2c02321318603849453b252e21e827c8197` |

`snponly-shim.efi` chains Authenticode-wise to Microsoft Windows UEFI Driver
Publisher ← Microsoft Corporation UEFI CA 2011; `snponly.efi` is verified by
the shim itself against its own embedded iPXE vendor certificate, not
against firmware `db`. `wimboot` is dual Authenticode-signed by Microsoft,
one chain through UEFI CA 2011 and one through the newer UEFI CA 2023.

DISM metadata for `boot.wim` records index 1, "Microsoft Windows PE
(amd64)", x64, version `10.0.26100`, edition WindowsPE. This retained
provenance does not itself carry a fourth version component; do not
attribute a more specific servicing/source build to this exact WIM than
this DISM metadata proves. The owner-observed in-shell `ver` output (see
WinPE completion, below) is a separate, independently-observed fact.

## Transfer evidence

- `/wimboot`, `/BCD`, and `/boot.sdi`: independently reassembled from the raw
  packet capture's TCP payload stream (not merely read back from the
  serving directory) and re-hashed; each reassembled body matched its
  pinned SHA-256 exactly.
- `/boot.wim`: established complete through the HTTP response's
  `Content-Length: 340134390` header plus TCP sequence arithmetic — the
  final data segment's ending relative sequence number, minus the header
  length, equals exactly 340,134,390 bytes, with a clean bilateral
  FIN/ACK close and no RST or truncation. The full 340 MB body was not
  independently reassembled and re-hashed from the capture; completeness
  rests on the Content-Length/sequence-arithmetic proof above, not on an
  independent hash match.

## Important negative/reusable findings

- **Direct Windows Boot Manager / TFTP delivery did not reach WinPE.**
  Serving the stock `EFI/Boot/bootx64.efi` plus the exact stock `BCD` over
  plain TFTP progressed only to repeated requests for optional
  Secure-Boot/Code-Integrity policy files (`SbcpFlightToken.p7b`,
  `SecureBootPolicy.p7b`, `SiPolicy.p7b`, `SkuSiPolicy.p7b`,
  `WinSiPolicy.p7b`, `ATPSiPolicy.p7b`, `VbsSiPolicy.p7b`) and a font
  (`wgl4_boot.ttf`), all not found, then network silence and
  `Windows Boot Manager Status: 0xc0000225 — An unexpected error has
  occurred.` Neither `boot.sdi` nor `boot.wim` was ever requested on the
  wire in this family.
- **Explicit policy/BCD variations did not move that boundary.** Substituting
  the boot.wim-embedded `bootmgfw.efi` for the retained-media `bootx64.efi`,
  serving the specific `WinSiPolicy.p7b` the Boot Manager actually
  requested, and replacing the stock BCD with a PXE-authored BCD following
  the Microsoft-documented PXE WinPE BCD model all reached the same
  `0xc0000225` boundary without ever requesting `boot.sdi`/`boot.wim`.
- **iPXE shim interoperability required the root `/ipxe.efi` fallback** in
  this tested physical environment (see Successful mechanism family).
- **wimboot with `boot.wim` alone reached Windows Boot Manager, then failed**
  on missing `\EFI\Microsoft\Boot\BCD` with `0xc000000f` ("The Boot
  Configuration Data for your PC is missing or contains errors"). An
  intermediate attempt additionally observed the Secure-Boot-aware
  boot-manager selection reject `bootmgfw_EX.efi` at the firmware level
  with "Verification failed: Security Policy Violation"
  (`EFI_STATUS 0x800000000000001a`) before falling back to `bootmgfw.efi`.
- **Providing the pristine external `BCD` + `boot.sdi` from the retained
  WinPE media eliminated that boundary**, and WinPE boot completed.

## WinPE completion

- owner-visible usable shell: `X:\Windows\System32>`;
- `ver`: `Microsoft Windows [Version 10.0.26100.1]`;
- `%SystemRoot%`: `X:\Windows`;
- stock `X:\Windows\System32\startnet.cmd` contains `wpeinit`;
- the owner manually invoked `wpeinit` after shell arrival and it completed
  functionally (`wpeinit.log` progressed through its normal stages to a
  final `STATUS: SUCCESS`).

Because `wpeinit` was manually re-invoked after shell arrival, the retained
`wpeinit.log` instance does **not** by itself prove provenance from the
automatic initial `startnet.cmd`-driven invocation.

Independently of that manual re-run, the packet capture shows a second,
later DHCP DORA on the isolated link (`vendor class: MSFT 5.0`, `client
provides name: minint-c438f67`), roughly 40 seconds after the PXE DORA,
answered with a normal `DHCPACK` — i.e. genuine WinPE-driven network
activity did occur. The manually-triggered `wpeinit.log`'s later "no
adapters found" line must **not** be read as a categorical "WinPE has no
NIC" conclusion given that DHCP evidence; the discrepancy is left
unresolved here, plausibly because the automatic network bring-up happened
earlier (during the original automatic `startnet.cmd`-driven `wpeinit`)
than the state captured by the owner's manual re-run.

## Limits

This Reference does **not** establish:

- cross-vendor firmware portability, or universal OEM compatibility;
- MikroTik/X520 production network topology;
- trusted bootstrap or authenticated Server-fingerprint delivery;
- a production artifact-update lifecycle, atomic bundle activation, or
  version pinning;
- a Windows installation workflow;
- compatibility of future iPXE/wimboot/ADK versions without revalidation.

## Related

- Issue #53 — execution history for this evidence;
- `docs/reference/physical-uefi-pxe-boot-chain.md` — prior physical DHCP/PXE
  and Fedora shim/GRUB evidence (Issues #50/#52);
- `docs/reference/winpe-boot-mechanism-spike.md` — prior virtualized
  WinPE/iPXE/GRUB evidence;
- `docs/reference/secure-boot-hardened-chain-spike.md` — prior virtualized
  Secure Boot evidence;
- ADR-0010 — Secure Boot V1 baseline decision;
- ADR-0021 — network-delivered WinPE Boot Adapter baseline mechanism
  decision;
- `docs/specifications/m0-stack-and-boundaries-baseline.md` — Boot
  Port/Adapter boundary.
