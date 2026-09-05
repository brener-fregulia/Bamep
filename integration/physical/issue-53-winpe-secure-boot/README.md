# Issue #53 — Physical Secure Boot / WinPE Network-Delivery Harness (Preserved)

## What this is

This directory preserves the historical physical-validation harness used
during the Issue #53 Technical Spike (Secure Boot + network-delivered WinPE
on the physical UEFI mini-PC Endpoint). The scripts and configuration here
were originally produced and run from temporary, non-versioned locations on
the operator's lab host and are preserved here only so the Spike does not
need to be reconstructed from scratch if similar physical validation is
needed again.

## What this is not

- **Not production configuration.** Nothing here is deployed, referenced, or
  loaded by Bamep Server, Agent, or any shipped component.
- **Not the official/final Bamep provisioning harness.** No attempt was made
  to generalize, harden, or promote these scripts into reusable tooling.
- **Not the authoritative source for technical findings.** Results,
  hashes, chain-of-boot evidence, and conclusions live in:
  - `docs/reference/physical-secure-boot-winpe-network-delivery.md`
  - `docs/reference/physical-uefi-pxe-boot-chain.md`
  - `docs/decisions/0021-ipxe-wimboot-network-delivered-winpe-baseline.md`

  This README does not repeat that narrative; treat the References/ADR above
  as authoritative for anything technical.

## Preservation, not refactor

These scripts are kept essentially verbatim from the recovered Spike
artifacts. They still contain the operator's lab-specific paths, IPs,
interface names, and internal comments — including throwaway markers such as
"THROWAWAY", "NOT production", or "DO NOT COMMIT" left over from when they
lived in a temporary directory. Those markers describe the scripts'
original, temporary working location; committing this preserved copy under
version control does not promote them to production status or contradict
those markers.

Do not "clean up," generalize, or modernize these files as part of
unrelated work. Any change here should have its own explicit reason.

## Layout

```text
issue-53-winpe-secure-boot/
├── README.md
├── .gitignore
├── phases/   — issue53-*-setup-run.sh / issue53-*-cleanup.sh per Spike phase
├── ipxe/     — phase-specific autoexec.ipxe variants (9c, 9d)
├── helpers/  — gate logic scripts and pcap/HTTP-body extraction helpers
└── final/    — autoexec.ipxe and dnsmasq.conf as actually deployed in Phase 9d
```

## Reuse

Any future reuse of these scripts (as a starting point for a new physical
Spike, or to distill a real reusable Integration Environment harness)
requires operator review first: paths, network interfaces, MAC addresses,
and assumptions are specific to the original Spike environment and are not
validated against any other setup.

Distilling a production/reusable physical-validation harness from this
material is out of scope for this preservation effort and may happen later
as its own explicit piece of work.

## What is deliberately excluded

Binary, proprietary, and third-party artifacts are not versioned here (see
`.gitignore`): `boot.wim`, `BCD`, `boot.sdi`, `wimboot`, `.efi` binaries,
upstream tarballs, packet captures, logs, DHCP leases, and other runtime
output. These remain only in the original recovery location outside the
repository.
