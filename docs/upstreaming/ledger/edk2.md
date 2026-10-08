# edk2 — upstreaming ledger

`liminavm/edk2` `limina` branch, based on `slp/edk2@krun-support` (the tree krunkit's firmware is
built from, itself carrying the `ArmVirtKrun` platform that tianocore does not have). "Upstream"
for these rows is therefore `slp/edk2` first. No row has been researched against it yet; the
dispositions are first guesses. Method: `README.md`.

| commit | subject | first guess at disposition |
|---|---|---|
| `96006ce` | ArmVirtKrun: wire VirtioKeyboardDxe into ConIn | **upstream-worthy** — a typeable ConIn on krun's virtio keyboard; no limina coupling |
| `920a265` | ArmVirtKrun: measure the boot into a TPM 2.0 from the device tree | **upstream-worthy once libkrun has a TPM** — ArmVirtQemu's TPM2 blocks ported to ArmVirtKrun (Tcg2ConfigPei/Tcg2Pei/Tcg2Dxe, SHA-256, `Tcg2PhysicalPresenceLibQemu`); finds the TPM through the `tcg,tpm-tis-mmio` node. Useless upstream until libkrun's `devices/tpm: add a TPM 2.0 behind the TIS MMIO interface` is |
| `4ab2a7b` | ArmVirtKrun: keep UEFI variables in a VMM-provided store | **upstream-worthy, pairs with libkrun `vmm: map a firmware's UEFI variable store from a file`** — `KrunEfiVarsPei` hands a `libkrun,efi-variable-store` DT range to the emulated variable driver. A torn write formats the store (no fault-tolerant writes); CFI flash is the sturdier design (`docs/hardening-backlog.md`) and may be what upstream would rather take |
