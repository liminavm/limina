# vTPM: a TPM 2.0 device backed by our own Rust TPM

Status: **design agreed, P0 done** (`spikes/vtpm-p0/RESULTS.md`), P1 next · Scope: a new TPM 2.0 engine crate (ours outright, like
virglrs), a TIS device in the libkrun fork, TPM2 support in the edk2 fork's `ArmVirtKrun`
platform, and the limina-side state file and VM setting.

## What it buys, and what it does not

A vTPM's guarantees are exactly as strong as wherever its state lives on the host. Anyone who
can run the VM as the host user can use it, so it adds nothing against a compromised host
account. Confidentiality at rest stays with FileVault, as `docs/roadmap.md` already says. What
it adds is **binding** (a secret sealed to it is useless in a copied, backed-up or shared `.raw`
without the TPM state), **per-VM identity**, and the **stock TPM ecosystem**:

1. **LUKS2 unlocked by the TPM** (`systemd-cryptenroll --tpm2-device=auto`): the crypto tier
   the roadmap left out for want of a substrate.
2. **TPM-sealed credentials** (`systemd-creds encrypt --with-key=tpm2`,
   `LoadCredentialEncrypted=`), alongside the SMBIOS credentials we already deliver.
3. **Keys that cannot leave the TPM**: `ssh-tpm-agent`, `tpm2-pkcs11`, commit signing, clevis.
4. **Measured boot and PCR policy**, from TPM2 support in our firmware.
5. **A stock-tier feature.** Fedora's kernel and userspace already carry the driver and the
   tools, so the device works with no limina guest component.

Session-keyring auto-unlock is **not** in scope here. When it comes, it goes through
`limina-agent-session` over vsock with the password in the macOS Keychain: the secret never
touches the guest disk, and the host can gate it on Touch ID or refuse a clone.

## Decisions

- **Our own TPM engine in Rust**, not libtpms, swtpm or the TCG reference code. Those stay
  available as **test oracles only**, never shipped. No Rust TPM engine exists to build on:
  `tpm-rs` is a client, and OpenVMM wraps Microsoft's C reference code.
- **The engine implements what our consumers send**, not the whole Library specification. The
  command set is measured in P0 from the real consumers (items 1–4 above and the firmware), and
  grows when a consumer needs more. An unimplemented command answers `TPM_RC_COMMAND_CODE`, which
  every client already handles.
- **Plain per-VM state file in v1.** Wrapping it with a Secure Enclave key is a later option. It
  would make the state unusable on another Mac, so a Time Machine restore onto a new machine
  would lose every sealed secret; that has to be the user's choice.
- **A clone keeps the TPM identity.** A later UI action resets it (a fresh seed), which the user
  invokes knowing it invalidates every secret sealed in that guest.
- **Suspend/resume carries the TPM state** in the snapshot, including the volatile state a
  `TPM2_Shutdown(STATE)` saves.
- **Firmware work is in scope**, so measured boot is part of the deliverable, not a follow-up.
- **The engine is `janus`**, `MIT OR Apache-2.0`, its own repository under liminavm. It is not
  published: the name is taken on crates.io.
- **RSA uses RustCrypto's `rsa` 0.10** (a release candidate at the time of writing) as it
  stands. Its timing advisory (RUSTSEC-2023-0071, "Marvin") is unpatched and accepted
  knowingly; ssh-tpm-agent's raw `RSA_Decrypt` is the exposed operation. RSA is one module that
  nothing else depends on, so replacing it stays local, and the advisory is re-checked before
  each release.
- **One PCR bank, SHA-256.** The allocation is part of the persisted state, so a bank can be
  added later without a new identity.

## Architecture

```
guest tpm_tis driver ──MMIO──▶ libkrun TIS device ──▶ Backend trait ──▶ engine crate
                                (fork, `limina`)      (one call:          (own repo,
                                 DT: tcg,tpm-tis-mmio  command → response)  #![forbid(unsafe_code)])
                                                                │
                                          limina: per-VM state file, vm.toml setting, snapshot
```

- **Device: TIS over MMIO, found through the device tree.** libkrun's aarch64 builds no ACPI
  tables (only `third_party/libkrun/src/arch/src/x86_64/acpi.rs` exists), so CRB, which Linux
  finds through ACPI's TPM2 table, is out. The node is `tcg,tpm-tis-mmio`, the same as QEMU's
  `virt` machine, so Fedora's `tpm_tis` driver binds without help. The device is mechanism only:
  register file, localities, FIFO, and one call into the backend per command. It is the
  upstreamable half.
- **Engine: `janus`, a crate of its own**, consumed by libkrun as a path dependency the way rutabaga
  consumes virglrs, pinned by `third_party/manifest.toml`. It knows nothing about TIS, libkrun or
  files: bytes in, bytes out, plus an explicit state value the caller persists. Randomness and
  time are injected, so a test can replay a command stream deterministically.
- **Firmware.** `ArmVirtKrun` builds with ArmVirtQemu's `TPM2_ENABLE` configuration, on by
  default: PlatformPeiLib finds the `tcg,tpm-tis-mmio` node, and Tcg2Pei/Tcg2Dxe measure the boot
  into the SHA-256 bank and hand the event log to Linux. Unlike ArmVirtQemu, it links
  DxeTpm2MeasureBootLib without Secure Boot, so the images it loads reach PCR 4. A TPM needs
  persistent UEFI variables (`docs/design/efi-vars.md`): with a TPM present, shim's fallback
  resets after recreating the boot entry, and volatile variables make that a loop. A managed VM
  keeps them in the bundle's `efi.vars`. An ad-hoc run with a TPM keeps them in a temporary
  store for as long as its supervisor runs, unless `--efi-vars <file>` names one.
- **limina** owns policy: whether a VM has a TPM, where its state file lives (beside the VM's
  disks), snapshot inclusion, and later the reset action.

Stock Fedora boots through GRUB, not a UKI, and our firmware has no Secure Boot. PCR policy is
therefore weaker on the stock tier than with systemd-pcrlock and a measured UKI (our journals
show those units skipping on `ConditionSecurity=measured-uki`). Sealing without a PCR policy, or
to the stable PCRs, still works, and covers items 1–3.

## The Rust discipline, from the first commit

The rules are virglrs's (`third_party/virglrs/CLAUDE.md`), and they apply unchanged. What they
mean for a TPM:

- **No unsafe at all.** The engine is `#![forbid(unsafe_code)]`; it has no FFI, no guest memory
  and no threads, so it never needs any. Miri then covers every test.
- **Bad state is unrepresentable.** TPM 2.0 is a soup of `u32`s in the same way virgl is:
  - A `TPM_HANDLE` encodes its class in the top byte. It is classified **once**, at unmarshal,
    into distinct types (`TransientHandle`, `PersistentHandle`, `NvIndex`, `PcrHandle`,
    `SessionHandle`, a `Hierarchy` enum). A handler cannot be handed the wrong class.
  - **Authorization is a token, not a check.** A handler that touches an object takes an
    `Authorized<'_, T>` that only the session layer mints after the HMAC or policy has been
    verified. A handler cannot forget to check, because it cannot obtain the object otherwise.
  - **Sessions are typed by kind.** HMAC, policy and trial sessions are distinct types, so a
    policy command applied to an HMAC session does not compile, and a trial session cannot
    authorize.
  - **Lifecycle is a typestate.** Before `TPM2_Startup` only `Startup` is accepted;
    `Tpm<Initialized>` and `Tpm<Running>` are different types, and a self-test failure is a
    third (`Failure` mode answers only `GetTestResult` and `GetCapability`).
  - **Secrets are their own types.** Sensitive areas, seeds and session keys zeroize on drop,
    have no `Debug`, no `Clone`, and compare in constant time.
  - **Two values that must agree are one value.** An object's name is derived from its public
    area, never stored beside it; a `TPM2B` size and its buffer are one slice from the
    unmarshal boundary onward.
- **The trust boundary is the command buffer.** No byte sequence a guest sends may panic the
  worker. Malformed input answers the TPM response code the specification assigns; an `assert!`
  fires only on a violated engine invariant behind that boundary.
- **No global state.** The engine is a value the device owns.
- **Marshalling is generated, not hand-written per type.** A derive (or generator) emits the
  marshal/unmarshal code for every structure from one description, so the hundreds of Part 2
  types cannot drift from each other. A marshalling bug is fixed in the generator.

## Verification, from the first commit

Each tool goes where `docs/design/in-crate-checkers.md` says it fits, and **every gate is armed
by a sabotage entry** (janus's `harness/sabotage/sweep.py`) that breaks the property and watches it fail.
An entry lands with its witness.

- **Kani**, on fixed-shape code with wide data: the unmarshal primitives for every length a
  guest can send; handle classification for every `u32`; `TPMS_PCR_SELECTION` bitmap parsing;
  NV offset/size bounds (`offset + size <= dataSize` without overflow); the size arithmetic of
  KDFa/KDFe and parameter encryption; the TIS register decoder for every offset and width.
- **Exhaustive enumeration** (`every_sequence`), for state machines with small domains: the TIS
  locality and command/response state machine; the session table; the transient object slots;
  NV index define/write/lock/undefine; Startup/Shutdown(CLEAR|STATE) sequences; the
  dictionary-attack lockout counter.
- **cargo-fuzz** (its own `fuzz/` workspace): whole commands into a running engine (no panic,
  and the response header's size equals the response length); a marshal round trip per type;
  the state-file decoder; TIS MMIO access sequences. Corpora are seeded from the P0 captures.
- **Miri** over the whole engine test suite.
- **Differential testing** against the TCG reference TPM and libtpms, driven with the same
  command stream. Where no randomness enters (PCR extends and reads, policy digests, names,
  capabilities, hash sequences, NV contents) responses must match byte for byte. Where it does,
  the comparison is structural, and correctness is checked by round trips through the real
  clients.
- **The consumers themselves**, as L2 tests on the HVF suite: the kernel probes `/dev/tpmrm0`;
  `systemd-cryptenroll` enrolls and the volume unlocks on the next boot; `systemd-creds` seals
  and unseals; `ssh-tpm-agent` signs; the `tpm2-tools` and `tpm2-pkcs11` test suites pass to the
  extent of the implemented command set; the firmware event log replays to the PCR values the
  TPM reports (`systemd-pcrlock log` / `tpm2_eventlog`), which is a real oracle for item 4.

## Phases

- **P0, guest-side premise check (spike). Done.** Homebrew QEMU with `swtpm` booted a clone of the
  F44 stock image with ACPI off, the device-tree path ours will use. Every consumer works, the
  firmware event log replays to the reported PCRs, and the corpus is in
  `spikes/vtpm-p0/corpus/`. **The measured command set is 32 commands** (`spikes/vtpm-p0/summary.md`):
  Startup, Shutdown, SelfTest, GetCapability, GetRandom, TestParms, ECC_Parameters, ReadClock,
  PCR_Read, PCR_Extend, CreatePrimary, Create, Load, ReadPublic, EvictControl, Unseal, Sign,
  RSA_Decrypt, Hash, HashSequenceStart, SequenceUpdate, SequenceComplete, StartAuthSession,
  PolicyPCR, PolicyAuthValue, PolicyGetDigest, ContextSave, ContextLoad, FlushContext,
  NV_DefineSpace, NV_Extend, HierarchyChangeAuth. Algorithms: SHA-256 names, ECC P-256 (ECDSA,
  and ECDH for every salted session), RSA 2048, KEYEDHASH sealing, AES-128-CFB parameter
  encryption. The error answers clients depend on (`TPM_RC_INITIALIZE` to a repeated Startup,
  `TPM_RC_REFERENCE_H0` to the kernel's stale ContextSave, `TPM_RC_VALUE` to an unsupported
  `TestParms`) are part of the set. Signed PCR policies (PolicyAuthorize) and Import join it
  with measured UKIs.
- **P1, the engine.** Crate, marshalling generator, typestate core, sessions and authorization,
  then commands in the order the corpus needs them, each landing with its tests, checkers and
  differential run. Exit: the whole P0 corpus replays correctly, and every checker in the
  section above is armed.
- **P2, the device.** libkrun TIS device, device-tree node, backend trait, limina setting and
  state file. Exit: a stock F44 guest under limina runs every P0 consumer, as L2 tests.
- **P3, firmware.** `ArmVirtKrun.dsc` TPM2 configuration and the event log. Exit: the event log
  replays to the reported PCRs, and a PCR-7-bound LUKS volume unlocks.
- **P4, snapshot.** TPM state in suspend/resume. Exit: a sealed credential unseals after a
  restore, and a restore across a `Shutdown(STATE)` resumes the session state.
- **Later:** reset-identity UI; Secure Enclave wrapping as an option; an EK certificate from a
  per-install limina CA, if remote attestation is ever wanted.
- **Later, larger algorithms.** The engine implements RSA-2048 and ECC P-256 only, which is every
  key the P0 consumers create; `TPM2_TestParms` refuses the rest, so `tpm2-pkcs11` (the one
  client that probes widely) offers only those, and an explicit request for a larger key fails
  at creation. Two additions, in this order: RSA-3072 (larger bounds and slower prime
  generation, which costs most in primaries, re-derived on every creation); then P-384 with
  SHA-384, which brings a second hash into names, sessions, tickets and signing (still one PCR
  bank, SHA-256). Not planned: RSA-1024, P-192, P-224, RSA-4096 (which swtpm refuses too).

## Open questions

None open. Settled in P2:

- **The `tss` group.** `/dev/tpmrm0` is `root:tss 0660`, so a user-level consumer (ssh-tpm-agent,
  tpm2-pkcs11, clevis as a user) needs the group. No limina image adds anyone to it, on either
  tier: it is the user's choice, as on any Fedora machine with a TPM. root's consumers
  (systemd-cryptenroll, systemd-creds) do not need it. The L2 test adds it in its own clone.
- **Where the state lives.** A managed VM with `[hardware] tpm = true` keeps it in the bundle's
  `tpm.state`. An ad-hoc `--tpm` stays in memory for one run unless `--tpm-state <file>` names
  one, so tests and pokes are throwaway by default.
