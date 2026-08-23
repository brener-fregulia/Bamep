# ADR-0010: Trusted bootstrap and Secure Boot baseline

Status: Accepted

## Context

Agent Protocol requires the Agent to authenticate the expected Bamep Server before a normal session. A controlled provisioning network alone is not a trust anchor, so the bootstrap path also needs an authenticated integrity basis.

The Secure Boot spike established that the tested UEFI x86-64 environment can enforce an executable trust chain fail-closed:
- Microsoft-signed WinPE was accepted;
- unsigned/untrusted EFI applications were rejected;
- a Microsoft-trusted shim chaining to distribution-signed GRUB was accepted.

The spike also exposed a separate problem: Secure Boot authenticates executable stages, but does not by itself authenticate arbitrary site-specific data such as the expected Server TLS fingerprint or enrollment/bootstrap context.

The empirical evidence is retained in `docs/reference/secure-boot-hardened-chain-spike.md`.

## Decision

Bamep V1 uses **Secure Boot as the production baseline for executable boot-chain integrity** on UEFI x86-64.

Higher layers consume the mechanism-independent property **`trusted bootstrap established`**, not a firmware-specific `SecureBootEnabled` fact. Firmware/Secure Boot mechanics remain behind the Boot Adapter boundary; Domain and Application logic must not depend on db/dbx, shim, GRUB, iPXE, or equivalent implementation details.

Secure Boot alone is insufficient to establish Bamep trusted bootstrap because it does not authenticate the site-specific bootstrap material consumed by the Agent. That material requires a separate authenticated binding to the trusted executable/bootstrap path.

The normative trusted-bootstrap, Server-fingerprint, freshness, evidence, trust-anchor, rotation, recovery, and failure semantics are owned by `docs/specifications/m0-trusted-bootstrap-and-server-fingerprint-contract.md`. ADR-0011 owns the site trust-anchor provisioning rationale.

A future hardened boot mechanism may replace Secure Boot only if explicitly specified, threat-modeled, and validated to provide the security properties required by the same mechanism-independent trusted-bootstrap contract.

## Alternatives considered

### Controlled LAN without Secure Boot
Rejected. Network position/reachability does not authenticate the boot path or Server and cannot protect against bootstrap substitution.

### Unsigned fingerprint/configuration delivery
Rejected. An attacker controlling bootstrap delivery could substitute both the Server destination and the expected fingerprint, defeating certificate pinning.

### Expose `SecureBootEnabled` as the Domain safety fact
Rejected. It would couple safety semantics to one firmware mechanism and unnecessarily block future equivalent hardened-bootstrap mechanisms.

### Secure Boot-backed executable trust plus a separate authenticated bootstrap-data contract
Accepted. It preserves a practical V1 executable-integrity mechanism while keeping Bamep's security invariant independent from firmware implementation details.

## Consequences

- Production Agent startup requires trusted bootstrap; Secure Boot supplies only the executable-integrity part of that property.
- Trusted bootstrap remains an independent destructive-operation safety condition; credential authentication does not imply it.
- Agent Protocol remains WSS with pinned Server authentication; this ADR does not reopen ADR-0005.
- No TOFU or unverified-Server fallback follows from this decision.
- The network-delivery/bootloader mechanism remains separate; selecting Secure Boot does not select GRUB, iPXE, wimboot, or another PXE delivery design.
- Simulator/non-production substitution semantics belong to their normative Specifications, not this ADR.

## Related

- `docs/reference/secure-boot-hardened-chain-spike.md` — empirical basis.
- `docs/specifications/m0-trusted-bootstrap-and-server-fingerprint-contract.md` — normative trusted-bootstrap contract.
- `docs/specifications/m0-endpoint-identity-lifecycle.md` — destructive-operation gate.
- `docs/specifications/m0-stack-and-boundaries-baseline.md` — Boot Port/Adapter boundary.
- ADR-0005 — Agent Protocol transport and Server authentication rationale.
- ADR-0011 — site trust-anchor provisioning rationale.
