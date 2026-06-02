# limina

The limina **front-end / supervisor**. For M1 it's a CLI that resolves a VM config,
spawns the entitled `limina-vmm` worker as a dedicated child process (decision **D3**),
and supervises its lifecycle. The AppKit UI grows on top of this supervisor later.

The supervisor does **not** call `hv_vm_create`, so it needs no entitlement and no
libkrun dependency — only the worker is codesigned.

## Lifecycle (`src/supervisor.rs`)

- Spawns `limina-vmm` in its **own process group**, so a terminal Ctrl-C (SIGINT to the
  foreground group) reaches only the supervisor — which then forwards shutdown
  explicitly, rather than the worker being killed out from under us.
- On SIGINT/SIGTERM: asks the guest to power off (SIGTERM → worker → libkrun shutdown
  eventfd → guest GPIO power button).
- If the guest hasn't powered off within `--shutdown-grace-secs` (default 20), escalates
  to **SIGKILL**.
- Maps the worker's exit to a VM-stopped outcome and reports it (clean power-off vs.
  forced).

### Graceful shutdown is tier-dependent

A **stock EFI Fedora guest does not honor the GPIO power button** — KRUN_EFI's ACPI
doesn't advertise it, so the orderly-shutdown request is a no-op and the supervisor
falls back to SIGKILL after the grace period. This is expected under the two-tier
guarantee: reliable graceful shutdown belongs to the **enhanced tier** (our DT /
`limina-agent` handling the signal), while the baseline still stops correctly via the
forceful fallback. (Verified 2026-06: stock guest needed the SIGKILL fallback.)

## Run

```sh
cargo build -p limina -p limina-vmm
crates/limina-vmm/sign.sh debug          # the worker must be signed; the supervisor need not be

./target/debug/limina \
  --firmware /opt/homebrew/Cellar/krunkit/<ver>/share/krunkit/KRUN_EFI.silent.fd \
  --disk /path/to/Fedora-Workstation-43.raw \
  --console /tmp/console.log           # tail -f to watch the boot
# Ctrl-C (or `kill -TERM <limina pid>`) → graceful request, then forced after the grace period.
```

The supervisor finds the worker next to its own executable (cargo `target/<profile>/`);
override with `--vmm-bin` or `$LIMINA_VMM_BIN`.
