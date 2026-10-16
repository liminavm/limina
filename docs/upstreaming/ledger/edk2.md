<!-- ledger-check: dep=edk2 -->
# edk2 — upstreaming ledger

`liminavm/edk2` `limina` branch, 9 commits on base `13e8adac` (`slp/edk2@krun-support` — the
tree krunkit's firmware is built from, itself carrying the `ArmVirtKrun` platform that tianocore
does not have). "Upstream" for these rows is therefore `slp/edk2` first. No row has been researched against it yet; the
dispositions are first guesses. Method: `README.md`.

| commit | subject | first guess at disposition |
|---|---|---|
| `aa76090` | ArmVirtKrun: drop the stale TerminalPcdProducerLib reference | **upstream-now** (unresearched guess) — a plain build fix: the platform does not build at the base's own HEAD. sec: no |
| `92c1055` | OvmfPkg/VirtioSerialDxe: raise the SerialIo TPL to TPL_NOTIFY | **upstream-now** (unresearched guess) — OvmfPkg code, so tianocore as well as `slp/edk2`; the illegal TPL lowering hits any VirtioSerial user. sec: no |
| `449dd50` | ArmVirtPkg: keep the PL031 DT node enabled for the OS | **upstream-after-cleanup** (unresearched guess) — reverses a deliberate ArmVirt policy, so may have to become a krun-only option. sec: no |
| `c694c8e` | OvmfPkg/VirtioKeyboardDxe: import from edk2-stable202505 | **drop on rebase** (unresearched guess) — a verbatim backport, gone once the base includes `edk2-stable202505` or later. sec: no |
| `41058d2` | ArmVirtKrun: graphical boot console via VirtioGpuDxe | **upstream-worthy** (unresearched guess) — the `PcdVideoResolutionSource=1` pin is a policy choice that may need discussion. sec: no |
| `96006ce` | ArmVirtKrun: wire VirtioKeyboardDxe into ConIn | **upstream-worthy** — a typeable ConIn on krun's virtio keyboard; no limina coupling |
| `920a265` | ArmVirtKrun: measure the boot into a TPM 2.0 from the device tree | **upstream-worthy once libkrun has a TPM** — ArmVirtQemu's TPM2 blocks ported to ArmVirtKrun (Tcg2ConfigPei/Tcg2Pei/Tcg2Dxe, SHA-256, `Tcg2PhysicalPresenceLibQemu`); finds the TPM through the `tcg,tpm-tis-mmio` node. Useless upstream until libkrun's `devices/tpm: add a TPM 2.0 behind the TIS MMIO interface` is |
| `4ab2a7b` | ArmVirtKrun: keep UEFI variables in a VMM-provided store | **upstream-worthy, pairs with libkrun `vmm: map a firmware's UEFI variable store from a file`** — `KrunEfiVarsPei` hands a `libkrun,efi-variable-store` DT range to the emulated variable driver. A torn write formats the store (no fault-tolerant writes); CFI flash is the sturdier design (`docs/hardening-backlog.md`) and may be what upstream would rather take |
| `0a8c904` | ArmVirtKrun: drive the physical timer when the firmware runs at EL2 | **upstream-worthy, needs review** (unresearched guess) — adds a new ArmPkg library (`ArmGenericTimerVirtPhyCounterLib`), likely needing tianocore buy-in; matters only under HVF nested virtualization. sec: no |
