# Secure Boot and Hardened Boot Chain — Local Virtualized Evidence

Status: **Completed empirical reference.**

This document preserves the validated virtualized-firmware evidence produced by the Secure Boot spike. It does not define current Bamep security policy. ADR-0010 owns the Secure Boot decision; the trusted-bootstrap Specification owns the normative bootstrap/fingerprint contract.

## Question

Determine whether a practical Secure-Boot-enforced UEFI x86-64 chain was viable for Bamep and identify relevant limitations before relying on it as a production baseline.

## Environment and limits

The experiment reused the WinPE tooling and VirtualBox VM prepared by `docs/reference/winpe-boot-mechanism-spike.md`.

Environment:
- VirtualBox **7.2.14**;
- UEFI x86-64 VM;
- VirtualBox built-in Microsoft-trusting Secure Boot defaults;
- Oracle/VirtualBox Platform Key;
- headless execution with timed screenshots and keyboard injection for observation.

Secure Boot configuration:

```text
VBoxManage modifynvram "BamepSpike-WinPE-UEFI" inituefivarstore
VBoxManage modifynvram "BamepSpike-WinPE-UEFI" enrollmssignatures
VBoxManage modifynvram "BamepSpike-WinPE-UEFI" enrollorclpk
VBoxManage modifynvram "BamepSpike-WinPE-UEFI" secureboot --enable
```

Active state was confirmed through:

```text
VBoxManage showvminfo --machinereadable | grep -i secureboot
```

This is **virtualized-firmware evidence only**. VirtualBox's PK, KEK, db, dbx, revocation state, and firmware behavior are not evidence of any particular OEM implementation. Physical Integration Environment validation remains necessary for production hardware claims.

## Scenario 1 — Stock Microsoft-signed WinPE

Target:
- ISO: `BamepWinPE-amd64.iso`;
- ADK: **10.1.26100.2454**;
- WinPE: **10.0.26100.1**;
- `EFI\Boot\bootx64.efi`: unmodified Windows Boot Manager supplied by the Windows ADK.

Observed:
- boot accepted under Secure Boot;
- `wpeinit` at approximately 15 seconds;
- elevated `X:\windows\system32\cmd.exe` shell at approximately 30 seconds;
- no Secure Boot rejection or warning.

This established compatibility of that exact stock ADK WinPE artifact with the tested Microsoft-trusting Secure Boot configuration. It did not establish compatibility with every OEM db/dbx/revocation state.

## Scenario 2 — Unsigned EFI executables

Targets:
- iPXE v2.0.0 `ipxe.efi`, SHA-256 `868aa34057ff416ebf2fdfb5781de035e2c540477c04039198a9f8a9c6130034`;
- self-built GRUB 2.12-1ubuntu7.3 `grubx64.efi`, SHA-256 `dc3f7377f86d78318359224b4e1e55700be25cad7f25af290d6b7d4738c537e7`.

Neither binary was signed by an enrolled authority.

Both produced:

```text
BdsDxe: failed to load Boot0001 "UEFI VBOX CD-ROM ..." from
PciRoot(0x0)/Pci(0xD,0x0)/Sata(0x1,0xFFFF,0x0): Access Denied
```

Rejection was immediate and deterministic. Neither binary produced its own startup output and no partial execution was observed. The error was distinct from the non-Secure-Boot failures seen during the WinPE spike.

This established that Secure Boot enforcement was active and fail-closed for the unsigned EFI binaries tested.

## Scenario 3 — Microsoft-trusted shim + Canonical-signed GRUB

Artifacts:
- `shim-signed` **1.58+15.8-0ubuntu1**, `/usr/lib/shim/shimx64.efi.signed`, SHA-256 `6fe6e1bcbe6cf6baec8e056d40361ca1aa715cc04ddcc2855351de060b84350b`;
- `grub-efi-amd64-signed` **1.202.5+2.12-1ubuntu7.3**, GRUB **2.12**, `/usr/lib/grub/x86_64-efi-signed/grubx64.efi.signed`, SHA-256 `a831af01e4fb5e3c9457120e1d08ea13d98a0a47b62728c284b7f502d535965c`.

Test chain:

```text
\EFI\Boot\BOOTX64.EFI = shimx64.efi.signed
\EFI\Boot\grubx64.efi = grubx64.efi.signed
\EFI\Boot\mmx64.efi   = shim MOK Manager
```

The ISO used the same `xorriso`/`mtools` generic UEFI El Torito technique already validated by the WinPE spike.

Observed:
- firmware accepted and executed shim;
- shim accepted and chainloaded the signed GRUB;
- GRUB reached a genuine interactive `GNU GRUB version 2.12` `grub>` prompt;
- no `Access Denied` occurred.

No `grub.cfg` was supplied, so the interactive/rescue prompt was expected.

This established that a Microsoft-trusted shim -> distribution-signed GRUB chain passed Secure Boot enforcement end to end in this virtualized environment using off-the-shelf signed distribution components.

## Follow-up — Signed GRUB to WinPE

A follow-up attempted to access the stock WinPE ISO from the signed GRUB session.

Observed:
- `ls (cd1)/` and `ls (cd2)/` did not expose the WinPE contents;
- `ls (cd0)/`, corresponding to the WinPE ISO, returned `error: unknown filesystem`;
- `ls (hd0)/` returned the same error on the blank SATA test disk.

The experiment therefore **did not reach the chainload operation**. This differed from the earlier custom-GRUB experiment, where file resolution succeeded and the later `chainloader` operation returned `error: unknown error`.

Result: **inconclusive**. The exact cause was not diagnosed. A possible area was module availability/loading in the packaged signed GRUB versus the earlier `grub-mkstandalone` build, including UDF/ISO9660 support, but this was not experimentally established.

## Security observation

The spike demonstrated a distinction later consumed by Bamep's architecture:

- Secure Boot authenticated executable stages accepted by the configured trust chain;
- Secure Boot did **not**, by itself, authenticate arbitrary site-specific data read by those executables.

A signed boot chain is therefore an executable-integrity primitive; Server fingerprint/enrollment/bootstrap data still requires an authenticated binding to that trusted path.

This evidence informed ADR-0010 and `docs/specifications/m0-trusted-bootstrap-and-server-fingerprint-contract.md`; those documents own the resulting requirements.

## Result summary

| Scenario | Result |
| --- | --- |
| Stock Microsoft-signed WinPE | accepted; booted normally |
| Unsigned iPXE | rejected fail-closed with `Access Denied` |
| Unsigned self-built GRUB | rejected fail-closed with `Access Denied` |
| Microsoft-trusted shim -> Canonical-signed GRUB | accepted; GRUB prompt reached |
| Signed GRUB -> stock WinPE | inconclusive; GRUB could not recognize the WinPE filesystem |

The spike established practical Secure Boot viability for the tested virtualized UEFI x86-64 environment and supplied both positive and negative enforcement evidence.

## Limits and follow-up evidence

Not established by this spike:
- real OEM firmware compatibility or trust-store equivalence;
- signed GRUB -> WinPE viability;
- physical-platform Secure Boot behavior;
- db/dbx update/revocation behavior;
- a production Server-fingerprint delivery mechanism.

MOK enrollment was not exercised here; it was later investigated in `docs/reference/site-trust-anchor-provisioning-spike.md`.

## Related

- ADR-0010 — Secure Boot V1 baseline decision.
- `docs/reference/winpe-boot-mechanism-spike.md` — underlying WinPE/boot tooling and earlier GRUB observations.
- `docs/reference/site-trust-anchor-provisioning-spike.md` — later MOK and UEFI trust-anchor provisioning evidence.
- `docs/specifications/m0-trusted-bootstrap-and-server-fingerprint-contract.md` — normative trusted-bootstrap contract.
- `docs/specifications/m0-stack-and-boundaries-baseline.md` — Boot Port/Adapter boundary.
