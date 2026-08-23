# Site Trust-Anchor Provisioning — Local Virtualized Evidence

Status: **Completed empirical reference.**

This document preserves the validated virtualized-firmware evidence comparing shim/MOK and direct UEFI `db`/PK site-key provisioning. It does not define Bamep's current first-site trust policy; ADR-0011 owns that decision and the trusted-bootstrap Specification owns the normative trust-anchor contract.

## Question

Can a previously unprepared UEFI x86-64 Endpoint establish a per-site trust key through either:

- **Candidate A — shim/MOK enrollment**, or
- **Candidate B — direct UEFI `db`/PK enrollment**,

and what per-Endpoint interaction, reboot, firmware-state, lifecycle, and automation constraints were actually observed?

## Environment and limits

The experiment reused `BamepSpike-WinPE-UEFI`:

- VirtualBox **7.2.14r174565**;
- firmware: `EFI64`;
- representative Microsoft-trusting Secure Boot configuration from the earlier Secure Boot spike;
- snapshot `pre-trust-anchor-spike` used to keep Candidate A and B independent.

This is **virtualized-firmware evidence**, not physical OEM evidence.

WSL2 Ubuntu 24.04.1 LTS on the Windows 11 host was used for key generation, signing, and media construction. WSL2 could not access the VirtualBox VM's UEFI variables; NVRAM operations were performed from an environment booted inside the VM.

Reused signed components:

- `shim-signed 1.58+15.8-0ubuntu1`;
- `grub-efi-amd64-signed 1.202.5+2.12-1ubuntu7.3`;
- shim's `mmx64.efi` MokManager.

Test key:

- RSA 2048 self-signed;
- `CN=Bamep Site Test Trust Anchor`;
- generated with `openssl req -x509 -new -newkey rsa:2048 … -nodes -days 3650`;
- `MOK.der` SHA-256:
  `c70613324734b47cf47b5d32625a57f30c3c53feecf20aabd8a1d85f6e766f62`.

## Disposable in-VM tooling

A minimal Linux environment was built to operate against the VM's real NVRAM:

- Canonical-signed `linux-image-generic 6.8.0-137.137`;
- custom initramfs via `update-initramfs -c`;
- `mokutil 0.6.0-2build3`;
- `efitools 1.9.2-3ubuntu3` tools as required;
- required shared libraries and the test certificate;
- previously validated shim/GRUB boot chain;
- `break=premount` to enter a BusyBox `v1.36.1` initramfs shell;
- `efivarfs` mounted inside the VM.

The environment had no persistent root filesystem or installed OS, allowing NVRAM persistence to be distinguished from disk/OS state.

## Candidate A — shim/MOK

### Enrollment

Observed sequence:

1. `mokutil --import /root/MOK.der`.
2. Password entered twice; request visible through `mokutil --list-new`.
3. Reboot.
4. shim displayed `Shim UEFI key management` / `Press any key to perform MOK management`.
5. Operator entered MokManager within its short countdown window.
6. `Enroll MOK` -> `Continue` -> `Yes` -> password.
7. Reboot.
8. `mokutil --list-enrolled` showed `Bamep Site Test Trust Anchor`.

**Measured cost: two reboots plus one mandatory interactive keyboard ceremony per enrollment.**

Two experiment attempts missed the short MokManager window. In both cases the pending request was silently consumed and `mokutil --import` had to be repeated. The exact window duration was not formally measured.

### Functional verification and SBAT finding

A minimal EFI executable was created with `grub-mkstandalone`, signed with the enrolled key using `sbsign`, and placed as shim's second stage.

Initial result:

```text
ERROR — Verification failed: (0x1A) Security Violation
```

The failure was not caused by absence of MOK trust. Comparison with Ubuntu's signed GRUB showed the test executable lacked a valid `.sbat` section.

A manual `objcopy --add-section` experiment produced a different `(0x3) Unsupported` error and was structurally unsuitable.

Rebuilding through:

```text
grub-mkstandalone --sbat=<minimal-sbat-file>
```

and re-signing produced a valid SBAT-compliant executable. That binary was accepted by shim and reached `grub>`.

**Empirical finding:** with shim 15.8 in this experiment, MOK trust alone was insufficient; the chain-loaded executable also needed acceptable SBAT metadata.

### Revocation

Observed sequence:

1. `mokutil --delete /root/MOK.der`;
2. password staging;
3. reboot;
4. MokManager `Delete MOK` ceremony;
5. reboot.

After revocation:

- the key disappeared from `mokutil --list-enrolled`;
- the same previously accepted SBAT-compliant MOK-signed executable again failed with `(0x1A) Security Violation`.

The full enroll -> accept -> revoke -> reject lifecycle was therefore demonstrated.

**Revocation had the same two-reboot plus interactive-ceremony shape as enrollment.**

### Persistence and automation boundary

MOK state survived complete VM power cycles despite the test environment having no persistent OS/disk. This established that the tested MOK state lived in UEFI NVRAM rather than an installed operating system.

Key generation, signing, media construction, `mokutil` staging, rebooting, screenshots, and synthetic keyboard events were scriptable in the VirtualBox experiment.

The MokManager confirmation itself still required console keyboard interaction before an OS was running. On physical hardware, equivalent automation would require physical presence or an out-of-band console capable of boot-time keyboard injection; that capability was not established for arbitrary OEM Endpoints.

## Candidate B — direct UEFI `db`/PK

Candidate B was completed with `KeyTool.efi` from `efitools 1.9.2-3ubuntu3`.

An earlier attempt to use `efi-updatevar` inside the minimal BusyBox initramfs failed because it invoked GNU `mount -l`, unsupported by BusyBox:

```text
mount: invalid option -- 'l'
```

That was recorded as a tooling-environment limitation, not evidence that the UEFI-variable mechanism itself failed.

### Preparing the firmware variables

Before resetting NVRAM, the existing variables were exported:

- `ms-db.bin`: **7636 bytes**, 5 Microsoft signature entries;
- `ms-kek.bin`: **3066 bytes**;
- `ms-pk.bin`: **1035 bytes**.

The site certificate was converted with:

```text
cert-to-efi-sig-list -g <GUID> MOK.pem bamep-own.esl
```

`bamep-own.esl`: **863 bytes**.

Prepared sets:

- `db-combined.esl` = existing `db` + site key: **8499 bytes**;
- `kek-combined.esl` = existing `KEK` + site key: **3929 bytes**;
- `pk-own.esl` = site key only, because `PK` is single-valued.

Authenticated update files were produced with:

```text
sign-efi-sig-list -c MOK.pem -k MOK.priv <VarName> <in.esl> <out.auth>
```

Resulting files:

- `db.auth`: **9765 B**;
- `KEK.auth`: **5195 B**;
- `PK.auth`: **2129 B**.

The VM NVRAM was reset into Setup Mode with VirtualBox's host-side `inituefivarstore`. Secure Boot reported off.

A boot disc then executed the stock unsigned `KeyTool.efi` directly with the three `.auth` files available.

### Enrollment

KeyTool reported:

```text
Platform is in Setup Mode
Secure Boot is off
```

Observed live sequence:

1. `KEK` -> `Add New Key` -> `KEK.auth`;
2. `db` -> `Add New Key` -> `db.auth`;
3. `PK` -> `Replace Key(s)` -> `PK.auth`.

After setting `PK`, without rebooting, KeyTool immediately reported:

```text
Platform is in User Mode
Secure Boot is on
```

**Measured enrollment cost after Setup Mode was already available: zero reboots.**

The procedure still required interactive KeyTool menu/file selection in this experiment.

### Functional verification

The site-key-signed test EFI executable was placed directly as `\EFI\Boot\BOOTX64.EFI`, with no shim.

Result: accepted and executed to `grub>`.

This established that firmware itself trusted the site key through `db`.

The original Microsoft-signed shim -> Canonical-signed GRUB test disc was then booted again and still succeeded, demonstrating that the constructed `db`/`KEK` state preserved the previously tested Microsoft-trusting path.

### Revocation and management-tool finding

After `PK` moved the platform into User Mode, the stock unsigned `KeyTool.efi` itself was rejected with firmware-level `Access Denied`.

Signing `KeyTool.efi` with the already-`db`-trusted site key restored its ability to run.

An authenticated `db` replacement was then prepared to restore only the original Microsoft entries:

```text
sign-efi-sig-list -c MOK.pem -k MOK.priv db ms-db.bin db-revoke.auth
```

Using the signed KeyTool:

1. live `db` contents showed 5 Microsoft entries plus the site key;
2. `db` -> `Replace Key(s)` -> `db-revoke.auth`;
3. re-reading `db` showed only the 5 Microsoft entries.

No reboot was required to apply that revocation update.

Afterward:

- direct boot of the site-key-signed test executable failed with `Access Denied`;
- the original Microsoft-trusted shim -> Canonical-signed GRUB chain still succeeded.

The full enroll -> accept -> preserve Microsoft trust -> revoke -> reject lifecycle was demonstrated.

**Empirical finding:** once Secure Boot enforcement was active, the firmware-management executable itself also had to be trusted/signed.

### Firmware-state prerequisite

The critical unresolved prerequisite was **initial Setup Mode**.

In this experiment Setup Mode was entered with VirtualBox's host-side:

```text
VBoxManage modifynvram <vm> inituefivarstore
```

That has no demonstrated equivalent on an arbitrary previously unprepared physical OEM Endpoint.

Observed once Setup Mode was available:

- `db`/`KEK`/`PK` writes required no reboot;
- setting `PK` transitioned to User Mode immediately;
- Secure Boot activated immediately;
- later `db`/`KEK` changes could use authenticated updates signed by an authorized key;
- routine post-enrollment update/revocation therefore did not require returning to Setup Mode in the tested lifecycle.

The experiment did **not** establish how arbitrary OEM hardware reaches initial Setup Mode without firmware-menu/console intervention.

### `UpdateVars.efi`

`UpdateVars.efi`, the potentially non-interactive companion tool, was not exercised. Its expected use required a UEFI Shell environment not constructed in this round.

Therefore no unattended claim is based on `UpdateVars.efi`.

## Comparative evidence

| Property | shim/MOK | direct `db`/PK |
| --- | --- | --- |
| Full enroll/accept/revoke/reject lifecycle | demonstrated | demonstrated |
| Initial state prerequisite | working shim/MokManager chain | UEFI Setup Mode |
| Enrollment reboot cost observed | **2** | **0 after Setup Mode is reached** |
| Revocation reboot cost observed | **2** | **0 to apply authenticated `db` update** |
| Interactive ceremony | mandatory MokManager confirmation | interactive KeyTool in tested path |
| Narrow boot-time interaction window | observed; missed twice | not observed in KeyTool path |
| Trust state persisted in firmware NVRAM | yes | yes |
| Extra executable policy observed | valid SBAT required by shim | management tool must itself be trusted once User Mode is active |
| Existing Microsoft-trusting path preserved | inherited through shim | explicitly verified after enrollment and revocation |
| Post-enrollment key updates without initial firmware mode | MOK ceremony repeats | authenticated `KEK`/`db` update demonstrated |
| Unattended first provisioning on arbitrary OEM Endpoint | not demonstrated | not demonstrated; initial Setup Mode unresolved |

Both mechanisms were technically functional in the virtualized experiment.

Neither experiment established zero-touch first-site-key provisioning for arbitrary previously unprepared OEM hardware.

Candidate A's unavoidable MokManager ceremony and Candidate B's unresolved initial Setup Mode prerequisite were the decisive empirical limitations later consumed by ADR-0011.

## Limits

Not established:

- behavior on physical OEM firmware;
- a generic unattended path into UEFI Setup Mode;
- a generic physical/OOB mechanism for the MokManager ceremony;
- `UpdateVars.efi` unattended operation;
- exact MokManager countdown duration;
- recovery after complete loss of the currently-authorized site private key;
- behavior across other hypervisors/firmware implementations;
- TPM/measured-boot alternatives;
- vendor enterprise firmware tooling.

No labor-time estimate was measured or inferred; only observed reboot/interaction characteristics are preserved.

## Related

- ADR-0011 — operator-verified first-site-key pairing decision informed by this evidence.
- ADR-0010 — Secure Boot baseline consumed by the experiment.
- `docs/reference/secure-boot-hardened-chain-spike.md` — prior shim/GRUB Secure Boot evidence.
- `docs/reference/winpe-boot-mechanism-spike.md` — underlying VM/media tooling.
- `docs/specifications/m0-trusted-bootstrap-and-server-fingerprint-contract.md` — normative site trust-anchor and trusted-bootstrap contract.
