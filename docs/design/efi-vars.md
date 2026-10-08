# Persistent UEFI variables

## Why

Without a store, our firmware (`ArmVirtKrun`) keeps UEFI variables in RAM, so every boot starts
with none. A stock Fedora disk then always boots from the removable path: shim finds no Fedora
boot entry and runs `fallback.efi` to recreate it. With a TPM present, fallback then resets so the
PCRs reflect a boot that did not go through fallback (shim `fallback.c`,
`fallback_should_prefer_reset`; it skips the reset only when `FB_NO_REBOOT` is set). Volatile
variables turn that into a reset loop. Persistent variables also make boot-order changes made
inside the guest stick, and Secure Boot will need them for enrolled keys.

## Mechanism: a file mapped as the emulated store

- **libkrun** maps a file `MAP_SHARED` into guest memory at `EFI_VARS_START` (`0x0400_0000`,
  256 KiB, between the firmware and RAM) and describes it with a `libkrun,efi-variable-store`
  device-tree node (`VmResources::efi_vars`). A missing or empty file is created at the store's
  size. A file of any other size is refused, never truncated.
- **The firmware** stays in emulated variable mode. `KrunEfiVarsPei` finds the node, maps the range
  as write-back data memory, reserves it as runtime-services memory so the OS keeps it and
  `SetVariable()` reaches it at runtime, and sets `PcdEmuVariableNvStoreReserved`. The variable
  driver adopts a store whose header is valid and formats one that isn't
  (`InitEmuNonVolatileVariableStore`). Without the node, variables stay in RAM.

Every variable write lands in the file through the page cache, so a worker crash loses nothing. A
host power loss before writeback can tear a write, and nothing makes that atomic: the firmware
then rejects the header and formats a new store. Today that costs one more fallback boot; once
Secure Boot keys live there, it would lose them. ArmVirtQemu's CFI flash with fault-tolerant
writes is the fix, and it's in `docs/hardening-backlog.md`, due before Secure Boot.

## Policy

- A managed VM keeps its variables in the bundle's `efi.vars`, beside `tpm.state`.
- An ad-hoc run (`limina --firmware …`) keeps them in RAM unless `--efi-vars <file>` names a
  store. With a TPM it gets a temporary store that lasts as long as its supervisor runs, reboots
  included, and is removed when the supervisor exits, so the first boot's fallback reset isn't a
  loop.
- A snapshot carries the store like RAM (it lies below the SHM window), and a restore rolls it
  back to its contents at snapshot time. That's what the restored guest's `efivarfs` view
  expects.
- A new store means one fallback boot: fallback records the entry and resets (with a TPM), and
  every later boot goes straight to it. PCR 4 is the same on every boot after that first one.
