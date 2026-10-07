# vTPM P0: the guest side on a known-good TPM

P0 of `docs/design/vtpm.md`. It answers two questions before any engine or device code exists.
First, does every consumer we want work on stock Fedora 44 aarch64 against a TPM on the
device-tree path our libkrun device will use? Second, exactly which TPM commands, algorithms and
session shapes do those consumers send? The answers are a command corpus (`corpus/*.jsonl`) and
the command set derived from it (`summary.md`).

## Vehicle

`run.sh` boots a CoW clone of `Fedora-Workstation-44.stock.test.raw` (kernel
`6.19.10-300.fc44.aarch64`, systemd 259.5) under Homebrew QEMU 11.0 with HVF. The TPM is swtpm
0.10.2 (libtpms 0.10.2) on `-device tpm-tis-device`, with QEMU's bundled edk2 as firmware.
`swtpm_setup --pcr-banks sha256` manufactures the TPM with the one SHA-256 bank the engine will
allocate. `consumers/00-provision.sh` installs what the consumers need.
swtpm runs at log level 20, which dumps every command and response in hex. `capture.sh <name>
<script>` runs one consumer script from `consumers/` in the guest and keeps the slice of the log
it produced. `decode.py` turns a slice into JSON lines with the command code, response code,
session attributes and algorithm choices, plus the raw bytes. `decode.py --summary` builds the
table. The raw log slices (`corpus/*.swtpm.log`) are gitignored, because the JSON lines carry
every byte of them that matters.

There are two corpora. `corpus/` is the canonical one, with SHA-256 only. `corpus-4bank/` is the
same runs against swtpm's default allocation (SHA-1, SHA-256, SHA-384 and SHA-512), which libtpms
uses when swtpm creates its own state. There the firmware extends all four banks, so that corpus
carries multi-bank `TPML_DIGEST_VALUES`. Both have the same 32 commands. The 4-bank one is kept as
input the engine must also parse and answer correctly.

Measured 2026-10-07.

## Findings

- **The device-tree path works on a stock guest, firmware included.** With
  `-M virt,acpi=off` (limina guests boot with ACPI disabled: `Machine model: linux,dummy-virt`,
  `ACPI: Interpreter disabled.`), Fedora's kernel binds `tpm_tis c000000.tpm_tis` from the
  `tcg,tpm-tis-mmio` node. The firmware's event log still reaches Linux through the EFI
  configuration table (`efi: … TPMFinalLog=… TPMEventLog=…`). `/dev/tpm0` and `/dev/tpmrm0`
  appear with no guest component. With ACPI on, the same guest binds `MSFT0101` through the TPM2
  ACPI table instead. That path is not ours.
- **Every consumer works:**
  - `systemd-tpm2-setup` (persistent ECC SRK at `0x81000001`, plus two systemd NvPCRs);
  - `systemd-creds` (tpm2, tpm2 + PCR 7, host+tpm2, and `--user`);
  - `systemd-cryptenroll` with PCR 7, then PCR 7 + PIN, unlocking through `systemd-cryptsetup`;
  - clevis's tpm2 pin, unbound and bound to PCR 7;
  - ssh-tpm-agent 0.9.0, with ECDSA and RSA keys signing through the agent;
  - tpm2-pkcs11 ECDSA signing;
  - the tpm2 OpenSSL provider, with an EC and an RSA key generated in the TPM and signing.
  
  After a guest reboot, the PIN-enrolled LUKS volume unlocks again.
- **The firmware event log replays exactly.** `systemd-pcrlock log` computes the SHA-256 value
  of PCRs 0–7, 9 and 14 from the log, and each matches what the TPM reports. That is P3's
  oracle, and it holds on a known-good stack. PCR 10 (IMA) has a value and no firmware log
  entries, which is expected.
- **32 distinct commands cover all of it** (`summary.md`): Startup, Shutdown, SelfTest,
  GetCapability, GetRandom, TestParms, ECC_Parameters, ReadClock; PCR_Read, PCR_Extend;
  CreatePrimary, Create, Load, ReadPublic, EvictControl, Unseal, Sign, RSA_Decrypt; Hash,
  HashSequenceStart, SequenceUpdate, SequenceComplete; StartAuthSession, PolicyPCR,
  PolicyAuthValue, PolicyGetDigest; ContextSave, ContextLoad, FlushContext; NV_DefineSpace,
  NV_Extend; HierarchyChangeAuth.
- **Algorithms and shapes:**
  - Every object uses a SHA-256 name algorithm.
  - Objects are ECC NIST P-256, RSA 2048 and KEYEDHASH (sealed data).
  - Storage keys use AES-128-CFB.
  - Sessions are HMAC, policy and trial. All use SHA-256 and AES-128-CFB parameter encryption in
    both directions (`decrypt` and `encrypt`). Some are salted, some bound.
  - Every salted session observed is salted to an ECC key: the kernel's null-hierarchy primary
    (`0x80000000`) or systemd's SRK. So StartAuthSession needs ECDH.
  - No consumer here uses PolicyAuthorize, PolicySigned, PolicyOR, Import or
    Duplicate.
  - That changes with signed PCR policies (`--tpm2-public-key`), which arrive with measured UKIs.
    `systemd-cryptenroll --tpm2-device-key` also needs Import.
- **RSA is required, and so is a decision about the advisory.** ssh-tpm-agent, the OpenSSL
  provider and tpm2-pkcs11 all create RSA 2048 keys. ssh-tpm-agent signs RSA through
  `RSA_Decrypt` with a NULL scheme, which is a raw private-key operation. That is exactly the
  operation the `rsa` crate's timing advisory (RUSTSEC-2023-0071) concerns.
- **NV_Extend is not optional.** systemd 259 defines two NvPCRs (`NV_DefineSpace` with the
  extend type, then `NV_Extend`) as part of `systemd-tpm2-setup`.
- **Error answers are part of the protocol.** The engine has to reproduce these:
  - `TPM_RC_INITIALIZE` (`0x100`), 19 times. tpm2-tss sends `Startup(CLEAR)` on every context
    initialisation and expects this answer.
  - `TPM_RC_REFERENCE_H0` (`0x910`), 103 times. The kernel's resource manager saves contexts of
    sessions that are already gone.
  - `TPM_RC_VALUE` on parameter 1 (`0x1c4`) for `TestParms` of RSA 4096. tpm2-pkcs11 probes key
    sizes this way and derives its mechanism list from the answers.
  
  The one `TPM_RC_FAILURE` (`0x101`) is QEMU's version probe of a not-yet-powered swtpm, not a
  guest command.
- **PCR banks:** the firmware extends exactly the banks the TPM reports as allocated. With
  SHA-256 alone, every consumer works and the event log replays the same way. No consumer read
  any other bank when four were allocated.
- **Stock-image details that matter for P2:**
  - `/dev/tpmrm0` is `root:tss 0660`. An unprivileged user needs the `tss` group, or every
    tool fails with `Permission denied`.
  - The KDE image's journal shows `tpm2-tss-fapi.conf` failing to resolve user `tss`. So not
    every image has the user, and there `/dev/tpmrm0` stays root-only.

## Not covered

- **tpm2-pkcs11 RSA signing** answered `CKR_MECHANISM_INVALID` for `SHA256-RSA-PKCS` and
  `RSA-PKCS`, even though the module lists both. This happens on swtpm too, so it is a
  module-side question, not one about our engine. RSA signing is covered by ssh-tpm-agent and
  OpenSSL instead.
- **Command bytes are captured, timing is not.** Nothing here measures latency.
- **No suspend/resume** (`Shutdown(STATE)`): not exercised here. P4 captures it under limina.

## Reproduce

```
brew install qemu swtpm
spikes/vtpm-p0/run.sh --fresh
echo "guest SSH forward ready: ssh -p 2240 claude@127.0.0.1" > spikes/vtpm-p0/work/fake-worker.log
scripts/wait-guest-ssh.sh spikes/vtpm-p0/work/fake-worker.log 300
for f in spikes/vtpm-p0/consumers/0*.sh; do spikes/vtpm-p0/capture.sh "$(basename "$f" .sh)" "$f"; done
# then reboot the guest, wait for ssh again, and run consumers/10-after-reboot.sh the same way
python3 spikes/vtpm-p0/decode.py --summary spikes/vtpm-p0/corpus/*.jsonl
```

`corpus/boot.jsonl` is the whole log up to the first ssh login: the QEMU probe, firmware and
kernel. On a fresh `vars.fd` it holds two boots, because the firmware resets once after its first
boot and the shim fallback writes the boot entries.
`corpus/09-reboot.jsonl` is a guest reboot from the shutdown to the next login.
