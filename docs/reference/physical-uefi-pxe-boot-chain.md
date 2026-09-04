# Physical UEFI PXE Boot Chain — Fedora / Mini-PC Evidence

## Status and scope

**Completed empirical reference**, produced by physical Integration Environment work
in Issues #50 and #52.

This document preserves reusable physical evidence only. It does not select Bamep's
production network-delivery mechanism, does not turn the observed shim/GRUB behavior
into a normative requirement, and does not replace `docs/discovery/architecture-redesign.md`
for the still-open network-delivered WinPE question.

## Physical environment

### Server

- Fedora 44 Server;
- direct provisioning interface `enp8s0`;
- Realtek RTL8168H/8111H onboard NIC;
- Linux driver `r8169`;
- Server MAC `3c:7c:3f:7b:23:b8`;
- Server management remained on Wi-Fi/Tailscale throughout, on separate interfaces;
- direct point-to-point Ethernet cable to the Endpoint (no switch in this path).

### Endpoint

- mini PC;
- PXE NIC MAC `e8:ff:1e:d6:2e:f5`;
- UEFI x86-64 PXE firmware.

### Network

- direct, isolated point-to-point link between `enp8s0` and the Endpoint;
- negotiated **1000 Mb/s Full Duplex** after cleaning the RJ45 connector/contacts; an
  earlier session on the same cable had negotiated only 100 Mb/s. The evidence
  establishes only that cleaning the contacts restored gigabit negotiation — no more
  specific root cause (e.g. which pair, which connector) was isolated;
- no IPv4 assigned to `enp8s0` outside the throwaway Spike runs, no route through it,
  no DHCP/TFTP/PXE service running outside an active Spike run.

The Intel X520-DA2 / MikroTik CRS326 path was **not** part of this validated topology.
Both X520 PHYs and DAC links were separately observed healthy at 10 Gb/s during #50,
but the MikroTik bridge/VLAN/DHCP configuration was intentionally left unaudited and
is out of scope for the evidence in this document.

## #50 — physical discovery evidence

- physical UEFI PXE IPv4 `DHCPDISCOVER` observed on `enp8s0` from the Endpoint;
- Vendor-Class `PXEClient:Arch:00007:UNDI:003016`;
- DHCP option 93 (client architecture) = `7`, EFI x86-64;
- DHCP option 94 (NDI) = UNDI `1.3.16`;
- DHCP option 97 (client GUID) = `00020003-0004-0005-0006-000700080009`;
- that GUID behaved as a placeholder/default value, not a unique per-machine
  identifier. It **must not** be promoted as trustworthy unique Endpoint identity;
- standard PXE bootstrap-related DHCP options were present in the client's parameter
  request list (TFTP server, bootfile name, vendor-encapsulated options, and the
  PXE-specific option range);
- Wake-on-LAN from the Fedora Server (`enp8s0`) to the Endpoint (`e8:ff:1e:d6:2e:f5`)
  was empirically successful using the already-installed `ether-wake`.

## #52 — DHCP and executable network-boot evidence

### DHCP

- complete physical DORA (`DHCPDISCOVER` → `DHCPOFFER` → `DHCPREQUEST` → `DHCPACK`)
  for the Endpoint's PXE firmware, isolated to `enp8s0`;
- temporary Server address `192.168.99.1/24`, removed after each run;
- Endpoint lease `192.168.99.66` in the observed runs;
- DHCP option 93 = `7` correctly selected the EFI x86-64 boot offer;
- offered boot path: `EFI/fedora/shimx64.efi`.

### Tested Fedora artifacts

shim:
- Fedora `shim-x64` **16.1-5**;
- `shimx64.efi` SHA-256 `571ea56b855dcf73bec6acb63c5ded44c2a191138bca0d8cfa5aa93f60f46fff`.

GRUB:
- Fedora `grub2-efi-x64` **2.12-64.fc44**;
- `grubx64.efi` SHA-256 `db283a408682e92dabec2c2098576c2a6e374e714320124a0161136c5b326095`.

`mmx64.efi` (shim MOK Manager) was present in the harness alongside `shimx64.efi`,
SHA-256 `f8af592759c8ab33b69c4b0e772da5a8e2aa6d09c7dbd5e24c62c89fa5fdbd05`. Its execution
was not evidenced; its presence did not block or alter the observed sequence.

### Run 1 boundary

- `shimx64.efi` transferred over TFTP;
- shim then requested, at the TFTP root (not under `EFI/fedora/`, where the harness
  had placed the file):
  - `revocations_sku.efi`
  - `revocations_sbat.efi`
  - `shim_certificate_0.efi`
  - `grubx64.efi`
- the harness only provided `grubx64.efi` under `EFI/fedora/`, so the root-level
  request for `grubx64.efi` was not found and the chain stopped;
- owner-visible result: the Endpoint screen briefly showed **"No image found"**;
  no GRUB menu appeared.

This is useful negative evidence: it isolates the failure to a TFTP path-layout
mismatch, not to DHCP, PXE discovery, or shim's ability to execute.

### Run 2 — hypothesis and result

Only material boot-relevant change from Run 1: the identical `grubx64.efi` was
additionally exposed at the TFTP root, alongside the existing `EFI/fedora/grubx64.efi`
copy. DHCP behavior, the offered shim path, and the inert `grub.cfg` content/location
were unchanged.

Observed result:
- `shimx64.efi` transferred;
- root-level `grubx64.efi` transferred;
- GRUB then performed its own network configuration-file lookup, in this order:
  - `EFI/fedora/grub.cfg-01-e8-ff-1e-d6-2e-f5` (MAC-specific form) — not found;
  - `EFI/fedora/grub.cfg-C0A86342` (full hexadecimal-IP form) — not found;
  - progressively shorter hexadecimal-IP prefixes — not found;
  - `EFI/fedora/grub.cfg` (generic form) — **found and transferred**;
- optional `EFI/fedora/x86_64-efi/{command,fs,crypto,terminal}.lst` requests were not
  found, and did not prevent the inert menu from appearing;
- the owner visibly reached the GRUB menu: **"Bamep Spike #52 - inert PXE test
  payload"**.

## Reusable path-resolution finding

For **Fedora shim 16.1-5 + grub2-efi-x64 2.12-64.fc44**, in this physical PXE
environment:

- shim, although itself loaded from `EFI/fedora/shimx64.efi`, requested `grubx64.efi`
  and its observed revocation/certificate side files (`revocations_sku.efi`,
  `revocations_sbat.efi`, `shim_certificate_0.efi`) at the **TFTP root**, not in the
  directory it was loaded from;
- GRUB, once running, resolved its configuration relative to its own compiled-in
  `EFI/fedora` prefix, using its standard MAC → hexadecimal-IP (progressively
  shortened) → generic-`grub.cfg` fallback sequence.

In other words: **shim resolves its next stage relative to the TFTP root; GRUB
resolves its configuration relative to its own compiled prefix.** A TFTP layout using
this exact shim/GRUB pair needs to satisfy both resolution bases.

This is compatibility evidence for these exact tested artifact versions and this
exact physical environment. It is **not** a universal Bamep TFTP path-layout
requirement, and it must be revalidated if the boot artifacts, their versions, or the
delivery mechanism change.

## What the evidence establishes

For this Endpoint and this environment, physical evidence now establishes the chain:

```text
UEFI x86-64 PXE firmware
→ isolated DHCP DORA
→ architecture-specific boot offer (option 93 = 7)
→ TFTP delivery
→ Fedora shim execution path
→ Fedora GRUB execution path
→ network-delivered GRUB configuration
→ visible inert GRUB menu
```

## What it does NOT establish

- network-delivered WinPE viability;
- physical Secure Boot enforcement (Secure Boot state on this Endpoint was not
  determined by #50/#52);
- the final production GRUB/iPXE/wimboot/or other network-delivery mechanism;
- trusted bootstrap;
- authenticated Server fingerprint delivery;
- production DHCP architecture;
- MikroTik/X520 compatibility for this boot chain;
- future production NIC compatibility;
- multi-vendor physical firmware portability beyond the one tested Endpoint.

No absence-of-disk-read claim was proven. The #52 payload configured and
intentionally executed no disk-write, partition, format, install, or other
destructive storage action — but the Spike did not attempt to prove the absence of
incidental disk reads by firmware, shim, or GRUB.

## Related

- Issues #50 and #52 — execution history for this evidence;
- `docs/reference/winpe-boot-mechanism-spike.md` — virtualized WinPE/iPXE/GRUB
  evidence;
- `docs/reference/secure-boot-hardened-chain-spike.md` — virtualized Secure Boot
  evidence;
- `docs/discovery/architecture-redesign.md` — open network-delivered WinPE
  production-mechanism question;
- ADR-0010 — Secure Boot V1 baseline decision;
- `docs/specifications/m0-stack-and-boundaries-baseline.md` — Boot Port/Adapter
  boundary.
