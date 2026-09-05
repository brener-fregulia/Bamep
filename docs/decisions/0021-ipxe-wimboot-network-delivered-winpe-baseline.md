# ADR-0021: iPXE + wimboot network-delivered WinPE Boot Adapter baseline

Status: Accepted

## Context

Bamep V1 already requires UEFI x86-64 and Secure Boot (ADR-0010), but the
concrete mechanism for network-delivering WinPE to the Boot Adapter remained
unresolved.

The evidence progressed in three stages:

- virtualized evidence (`docs/reference/winpe-boot-mechanism-spike.md`,
  `docs/reference/secure-boot-hardened-chain-spike.md`) established that
  stock WinPE boots under UEFI, and that a Microsoft-trusted shim chaining
  to distribution-signed GRUB passes Secure Boot enforcement, but did not
  establish a viable network-delivered WinPE path;
- physical evidence from Issues #50/#52
  (`docs/reference/physical-uefi-pxe-boot-chain.md`) established isolated
  PXE DHCP and a Fedora shim → GRUB chain reaching a visible inert GRUB
  menu on real UEFI x86-64 firmware, without exercising WinPE or physical
  Secure Boot;
- physical evidence from Issue #53
  (`docs/reference/physical-secure-boot-winpe-network-delivery.md`)
  established, on the same diskless physical Endpoint with Secure Boot
  actually enabled, an iPXE + wimboot chain reaching a functional stock
  WinPE shell.

## Decision

For Bamep V1, adopt the following Boot Adapter baseline mechanism family:

```text
UEFI PXE
→ Secure-Boot-capable iPXE bootstrap
→ SNP-based iPXE network stage
→ HTTP delivery of the WinPE boot assets
→ wimboot
→ stock WinPE
```

This is an Adapter/infrastructure decision. Domain and Application logic
must remain independent of iPXE, wimboot, concrete TFTP paths, or other
firmware/PXE implementation details; those stay behind the Boot Port/Adapter
boundary per `docs/specifications/m0-stack-and-boundaries-baseline.md`.

Secure Boot remains independently owned by ADR-0010; this decision does not
reopen it. Trusted bootstrap and Server-fingerprint authentication remain
independently owned by the trusted bootstrap contract and ADR-0011; this
decision does not establish trusted bootstrap by itself.

The exact versions and hashes validated in Issue #53 are empirical evidence,
not permanent architectural version pins. Updates or replacements of
boot-chain artifacts (iPXE, wimboot, ADK/WinPE media) require compatibility
and security requalification appropriate to their risk before being relied
upon in production.

Compatibility-specific behavior observed during validation — such as the
tested shim's `/ipxe.efi` root-level fallback request — is Adapter-level
compatibility detail, not a Domain or universal protocol requirement.

The WinPE assets used together must form a mutually compatible boot set.
The successful physical evidence used a pristine `BCD` + `boot.sdi` +
`boot.wim` drawn from the same retained ADK WinPE media lineage; wimboot's
automatic BCD/boot.sdi extraction from `boot.wim` alone did not suffice in
the tested WinPE deployment image.

## Alternatives considered

### Keep the mechanism unresolved
Rejected for the V1 baseline. Issue #53 now demonstrates an end-to-end
physical path meeting the required boundary (network-delivered WinPE under
Secure Boot), so the question no longer needs to stay open.

### Direct Windows Boot Manager-oriented network delivery
Not selected. The tested physical variants (stock BCD over TFTP,
`bootmgfw.efi` substitution, individual Secure-Boot/Code-Integrity policy
files, a PXE-authored BCD) repeatedly stopped at
`Windows Boot Manager Status: 0xc0000225` without ever requesting
`boot.sdi`/`boot.wim`.

### GRUB → Windows Boot Manager / WinPE
Not selected as the V1 baseline. Physical GRUB execution was proven only
through the inert-menu boundary in Issues #50/#52, and no physical
GRUB → WinPE completion exists; the corresponding virtualized experiment
also did not establish a working chainload.

### iPXE + wimboot
Accepted. It physically reached a functional stock WinPE shell
(`wpeinit` → `X:\Windows\System32\cmd.exe`) while Secure Boot remained
enabled and active on the diskless Endpoint throughout.

## Consequences

- Implementation work may target this Boot Adapter family for the
  production network-delivered WinPE mechanism.
- Compatibility testing (firmware, NIC, artifact versions) remains
  necessary before production reliance; this ADR does not establish
  cross-vendor portability.
- The mechanism may evolve later through a superseding ADR if evidence or
  requirements change.
- No Domain coupling is introduced: Domain/Application logic continues to
  depend only on the mechanism-independent Boot Port.
- This ADR does not decide boot-asset versioning, atomic bundle activation,
  or artifact-update lifecycle; those remain open for future work.
- This ADR does not by itself establish trusted bootstrap.

## Related architecture

- `docs/specifications/m0-stack-and-boundaries-baseline.md` — Boot
  Port/Adapter boundary and dependency constraints.

## Related

- ADR-0010 — trusted bootstrap and Secure Boot baseline.
- ADR-0011 — site trust-anchor operator-verified pairing.
- `docs/reference/physical-secure-boot-winpe-network-delivery.md` —
  empirical basis for this decision.
- `docs/reference/physical-uefi-pxe-boot-chain.md` — prior physical
  PXE/shim/GRUB evidence.
- `docs/reference/winpe-boot-mechanism-spike.md` — prior virtualized WinPE
  evidence.
- `docs/reference/secure-boot-hardened-chain-spike.md` — prior virtualized
  Secure Boot evidence.
