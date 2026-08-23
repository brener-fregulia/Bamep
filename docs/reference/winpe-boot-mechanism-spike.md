# WinPE Boot Mechanism — Local Virtualized Evidence

Status: **Completed empirical reference.**

This document preserves the validated virtualized evidence from the WinPE boot-mechanism spike. It does not select Bamep's production boot mechanism. Current boot-boundary requirements belong to `docs/specifications/m0-stack-and-boundaries-baseline.md`; Secure Boot evidence is preserved separately in `docs/reference/secure-boot-hardened-chain-spike.md`.

## Question

Establish whether stock WinPE can boot under UEFI and whether either tested network-oriented path — iPXE + wimboot or GRUB EFI chainload — can deliver WinPE in the available local environment.

## Environment

Initial WinPE experiment:
- Windows ADK **10.1.26100.2454**;
- Windows PE add-on **10.1.26100.2454**;
- WinPE build **10.0.26100.1**;
- VirtualBox **7.2.14r174565**;
- firmware `EFI64`, chipset `piix3`;
- Secure Boot **off**.

This is virtualized-environment evidence. It does not establish behavior on physical OEM firmware, NICs, storage controllers, or real PXE infrastructure.

## Experiment 1 — Direct stock WinPE UEFI boot

### Media construction

`copype.cmd` could not run normally because DISM returned error 740 requiring elevation.

For this boot-viability test, media was assembled directly from ADK static content:
1. copied `Windows Preinstallation Environment\amd64\Media`;
2. copied stock unmodified `amd64\en-us\winpe.wim` to `media\sources\boot.wim`;
3. built a UEFI-only ISO with:

```text
oscdimg.exe -m -o -u2 -udfver102 -bootdata:1#pEF,e,b"efisys_noprompt.bin" <media_dir> <iso>
```

No WIM customization, driver injection, or non-default `startnet.cmd` change was used.

The VM used UEFI firmware, DVD boot, VirtualBox NAT, and later a blank **2048 MB** SATA/AHCI virtual disk. Observation used headless screenshots and synthetic keyboard input.

### Results

Stock WinPE booted successfully and reproducibly:
- ~15 s: `wpeinit` running at `X:\Windows\System32>`;
- ~25–30 s: initialized elevated command shell;
- second boot reproduced the same progression.

Networking:
- emulated `Intel(R) PRO/1000 MT Desktop Adapter` recognized by inbox WinPE driver;
- DHCP lease obtained automatically;
- IPv4 `10.0.2.15`, gateway `10.0.2.2`.

Storage:
- SATA/AHCI disk visible through `diskpart`;
- `Disk 0, Online, 2048 MB`;
- no storage-driver injection required for the tested controller.

DiskPart reported WinPE **10.0.26100.1**.

Local session artifacts were recorded under `C:\BamepSpike\vm\`, including `screenshot_*.png`, `BamepWinPE-amd64.iso`, and the VM definition.

**Evidence established:** the exact tested stock ADK/WinPE build boots directly under VirtualBox EFI64 and reaches a functional network- and disk-capable shell. This did not test PXE/network delivery, Secure Boot, or physical hardware.

## Experiment 2 — iPXE + wimboot + HTTP

### Artifacts

Self-built current-upstream iPXE:
- source commit `e6d0a97c05d238c17eeae5116cb6e9c0fc9fdb56` (2026-08-11);
- built in WSL2 Ubuntu 24.04.1 with `make bin-x86_64-efi/ipxe.efi`;
- plain and `EMBED=bamep-embed.ipxe` variants;
- embedded build SHA-256 `0f0475509a27406ee55be0c59c5d9bde5f034260b5c886b2a8ae06a76d148052`.

Intended embedded flow:

```text
dhcp
kernel http://.../wimboot
initrd http://.../boot.wim boot.wim
boot
```

wimboot:
- official release artifact;
- **76,064 bytes**;
- SHA-256 `5f067ccdc4d084d5bf77b6c853bd0f8402dfc2b4cd1b103d358993ae97fae8e3`.

HTTP serving used Python **3.13.14** `http.server`, reachable from the NAT VM at `10.0.2.2:8000`.

### Attempt A — VirtualBox NAT PXE/TFTP

VirtualBox NAT TFTP served `ipxe.efi` with network first in boot order.

Observed:
- PXE DHCP succeeded;
- `Station IP address is 10.0.2.15`;
- `>>Start PXE over IPv4` appeared;
- TFTP transfer did not complete;
- firmware returned to Boot Manager after ~10–15 s.

A critical false positive was found: with the original WinPE ISO still attached as fallback, failed PXE silently fell through to that ISO and booted WinPE directly. The absence of HTTP requests exposed this. Removing the fallback device made the failure unambiguous.

Result: **VirtualBox NAT TFTP could not bootstrap iPXE in this environment.** This is a harness-specific finding, not production PXE evidence.

### Attempt B — Boot iPXE as an EFI application

Two El Torito approaches were tested.

#### ADK `efisys_noprompt.bin`

A minimal `oscdimg` ISO containing `ipxe.efi` as `\EFI\Boot\bootx64.efi` failed with:

```text
No mapping
```

Padding to ~66 MB did not change the result.

Placing the arbitrary EFI binary inside the full WinPE media tree produced a bootable disc, but Windows Boot Manager booted WinPE directly; iPXE still did not execute.

**Evidence established:** the tested ADK `efisys.bin`/`efisys_noprompt.bin` mechanism behaved as a Windows Setup/WinPE-specific El Torito structure, not a generic arbitrary-EFI loader.

#### Generic FAT El Torito

A generic image built with `xorriso`/`mtools`:

```text
xorriso -as mkisofs -eltorito-alt-boot -e <fat-image> -no-emul-boot
```

successfully executed `ipxe.efi`. Genuine iPXE startup output appeared:

```text
iPXE initialising devices...
file:autoexec.ipxe... Not found
```

This proved the generic El Torito harness could execute the tested EFI application.

### Native-driver iPXE runtime result

Both self-built variants hung immediately after the local `autoexec.ipxe` probe:
- embedded script's first `echo` never appeared;
- no interactive shell appeared;
- injected keys had no effect after 30+ s.

The plain build failed identically, ruling out the embedded script as the cause. **wimboot was never reached.**

## Experiment 3 — GRUB EFI chainload to WinPE

GRUB:
- Ubuntu 24.04.1 `2.12-1ubuntu7.3`;
- built with `grub-mkstandalone -O x86_64-efi`;
- modules included GPT/MSDOS partitioning, ISO9660, FAT, UDF, normal/config/chain support;
- SHA-256 `dc3f7377f86d78318359224b4e1e55700be25cad7f25af290d6b7d4738c537e7`.

GRUB was loaded through the proven generic FAT El Torito mechanism. The outer optical volume contained the original WinPE tree:

```text
\EFI\Boot\bootx64.efi
\Boot\BCD
\Boot\boot.sdi
\sources\boot.wim
```

Chainload target:

```text
(cd0)/EFI/Boot/bootx64.efi
```

### Result

GRUB booted and executed its script normally, but:

```text
chainloader (cd0)/EFI/Boot/bootx64.efi
```

failed with:

```text
error: unknown error.
```

A subsequent `boot` failed because nothing was loaded.

Diagnostic `ls` confirmed `(cd0)` was the expected volume and the exact target file existed. The failure therefore occurred during the EFI chainloader handoff, not path resolution.

The specific EFI Boot Service/root cause was not determined. A UEFI DevicePath/device-handle problem and a separate reported GRUB generic-error bug were found as possible explanations for this error class, but neither was proven here.

This was an unusual **same-disc cross-track** test. It does not establish whether network-loaded GRUB chainloading a network-delivered Windows Boot Manager behaves the same way.

## Experiment 4 — Released iPXE control

A bounded final control tested official iPXE **v2.0.0**:
- published 2026-03-06;
- commit `12798ec`;
- product identifier `g12798`;
- release archive `ipxeboot.tar.gz`: **12,002,760 bytes**.

### Official `ipxe.efi`

Native-driver build.

SHA-256:

`868aa34057ff416ebf2fdfb5781de035e2c540477c04039198a9f8a9c6130034`

Result: **same hang** as the self-built current-upstream native-driver image, immediately after the `autoexec.ipxe` probe.

This ruled out the specific self-built snapshot as the cause.

### Official `snponly.efi`

SNP-only build.

SHA-256:

`f61c2ce34e05d7d857633df2e512d547df75b6aa18b2da152a7c9af222cfe28f`

Result:
- did not hang;
- printed the iPXE 2.0.0 banner/features;
- reported `No more network devices`;
- exited back to firmware;
- firmware retried the CD-ROM and eventually reported no bootable device.

**Evidence established:** native-driver iPXE hangs in this exact VirtualBox EFI64/NAT setup across self-built current source and official v2.0.0, while SNP-only iPXE runs to a deterministic outcome but sees no usable network device. No tested iPXE variant reached a usable network-capable shell, so wimboot remained unexercised.

Why SNP saw no network device was not diagnosed.

## Result summary

| Path | Result |
| --- | --- |
| Stock WinPE via ADK UEFI El Torito | **Succeeded** |
| VirtualBox NAT PXE/TFTP -> iPXE | Failed before iPXE load |
| ADK `efisys_noprompt.bin` -> arbitrary EFI | Did not execute arbitrary EFI |
| Generic FAT El Torito -> iPXE | EFI application loaded successfully |
| Self-built native-driver iPXE | Hung before shell/wimboot |
| Official v2.0.0 native-driver `ipxe.efi` | Same hang |
| Official v2.0.0 `snponly.efi` | Ran, found no network device, exited |
| GRUB 2.12 -> local WinPE Boot Manager | `chainloader` failed with `unknown error` |

## Empirical conclusion

The local evidence establishes **direct UEFI WinPE boot viability** for the tested stock ADK/WinPE build.

It does **not** establish a viable network-delivered WinPE path:
- iPXE/wimboot never reached wimboot because no tested iPXE path became both responsive and network-capable;
- GRUB reached the WinPE EFI target but failed during local EFI chainload before BCD/boot.wim processing;
- that GRUB experiment was local same-disc chainload, not the production-relevant network form.

Neither network-path failure is evidence of a fundamental Bamep incompatibility. Both remain bounded to this VirtualBox configuration.

Physical Integration Environment validation remains necessary for real PXE/DHCP/TFTP, firmware, NIC, and network-delivered WinPE behavior.

## Limits

Not established:
- physical OEM UEFI behavior;
- production PXE/DHCP/TFTP behavior;
- successful iPXE + wimboot execution;
- successful GRUB -> Windows Boot Manager chainload;
- whether network-loaded GRUB differs from the tested cross-track optical layout;
- root cause of native-driver iPXE hang;
- root cause of absent SNP network device;
- root cause of GRUB's `unknown error`;
- real-hardware driver coverage;
- Secure Boot behavior.

Secure Boot must not be inferred from this Secure-Boot-off experiment.

## Related

- `docs/reference/secure-boot-hardened-chain-spike.md` — Secure Boot evidence using related tooling/artifacts.
- `docs/reference/hardware-compatibility.md` — earlier FORGE PoC boot-chain evidence.
- `docs/reference/driver-provisioning.md` — physical driver/injection evidence.
- ADR-0009 — driver-provider integration rationale.
- `docs/specifications/m0-stack-and-boundaries-baseline.md` — Boot Port/Adapter boundary.
