# limina-vmm

The limina **VMM worker**: boots a Linux guest on libkrun + Hypervisor.framework.
It drives libkrun through its *internal* Rust crates (decision **D2.1** — no C ABI),
and runs as a dedicated child process (decision **D3**).

## Layout

- `src/main.rs` — CLI, logging, builds a `VmSpec`.
- `src/config.rs` — `VmSpec`/`DiskSpec`/`ConsoleSpec`: limina's typed config, the input
  to the facade.
- `src/krun/` — the **facade**: the one place that translates a `VmSpec` into
  libkrun's `VmResources`, calls `vmm::builder::build_microvm`, and runs our own
  `EventManager` loop. All coupling to libkrun's internal API is concentrated here so
  upstream rebases touch one module.
  - `mod.rs` — `build_resources` (machine/firmware/disk) + `boot`.
  - `console.rs` — serial console wiring (disable the output-dropped implicit serial,
    attach ours; output-only by default, optional input fd).

## Build, sign, run

The binary must be codesigned with `com.apple.security.hypervisor` or `hv_vm_create`
fails.

```sh
cargo build -p limina-vmm
crates/limina-vmm/sign.sh debug      # ad-hoc sign with the hypervisor entitlement

./target/debug/limina-vmm \
  --firmware /opt/homebrew/Cellar/krunkit/<ver>/share/krunkit/KRUN_EFI.silent.fd \
  --disk /path/to/Fedora-Workstation-43.raw \
  --cpus 4 --ram-mib 4096 \
  --console /tmp/console.log         # tail -f to watch the boot
```

A **stock** image shows only firmware + GRUB on serial. To see the kernel dmesg,
apply `spikes/m1-boot/serial-grub.cfg` to the image's ESP first (adds
`earlycon=pl011,mmio32,0x0a001000 console=ttyAMA0`); see `spikes/m1-boot/RESULTS.md`.
