# Spike: M1 boot via libkrun's INTERNAL Rust API (no C ABI)

**Question:** Can limina vendor libkrun and drive it through its **internal Rust crates**
(`krun-vmm`, `krun-devices`, `krun-polly`, `krun-utils`) directly — skipping the C
ABI entirely — to get the Rust compiler's guarantees and full control?

**Verdict: YES, cleanly.** This crate boots `Fedora-Workstation-43.raw` to systemd
userspace with **zero C / FFI**, at full parity with the C-ABI spike (`../m1-boot`).
It is the architecture-validating spike for the decision to **vendor libkrun and use
its internal APIs**.

## What it does (the whole surface we need for M1)

A standalone Rust binary that path-deps into the vendored workspace and:

```rust
let mut vmr = VmResources::default();
vmr.set_vm_config(&VmConfig { vcpu_count: Some(4), mem_size_mib: Some(ram), .. })?;
vmr.set_firmware_config(FirmwareConfig { path: firmware.into() });   // KRUN_EFI.silent.fd
vmr.add_block_device(BlockDeviceConfig { disk_image_format: ImageType::Raw, .. })?;
vmr.disable_implicit_console = true;                                 // pub field
vmr.serial_consoles.push(SerialConsoleConfig { input_fd, output_fd });
let mut em = EventManager::new()?;
let (tx, _rx) = crossbeam_channel::unbounded();
let _vmm = vmm::builder::build_microvm(&vmr, &mut em, None, tx)?;     // Arc<Mutex<Vmm>>
loop { em.run()?; }                                                   // OUR loop, not krun_start_enter
```

That is exactly what the C `krun_start_enter` does internally — minus the C
marshalling, and minus libkrun's `ctx_cfg → VmResources` orchestration, which we
reimplement ourselves (for M1 that's trivial: firmware + disk + serial + machine
config; see notes for what grows later).

## Findings

- **The internal API is fully public and ergonomic.** `VmResources` is `Default`
  with public setters *and* public fields (`firmware_config`, `block`,
  `serial_consoles`, `disable_implicit_console`, `console_output`, …).
  `build_microvm`, `EventManager::{new,run}`, `Vmm`, `worker` are all `pub`. No C.
- **The vendored crates build clean as path-deps on macOS arm64** (Rust 1.88), with
  just `features = ["blk"]` on `krun-vmm`/`krun-devices`. First build compiles the
  whole tree (~one-time); the empty `[workspace]` in `Cargo.toml` detaches this crate
  from both the limina root and `third_party/libkrun`'s own workspace.
- **We own the run loop.** The boot path no longer routes through a
  forever-looping C entrypoint. (The guest-shutdown → `Vmm::stop` → `libc::exit`
  path still lives inside `krun-vmm` — proven in the C spike — so the child-process
  model still holds *for now*; taming it for true in-process control is a separate,
  deliberate patch, not required by this decision.)
- **Type safety as promised:** config is real typed structs/enums (`ImageType::Raw`,
  `CacheType::Writeback`, `SyncMode::Full`), not `u32` flags marshalled over FFI.
  The display/input backends (M2) are native Rust traits — no `#[repr(C)]` vtable
  dance the C path forced.

## Cost we accept (the rebase tradeoff)

Using the internal API couples us to libkrun internals, not just our patches. Two
concrete obligations going forward:
1. **Reimplement the orchestration** that lives in `src/libkrun/src/lib.rs` (not in
   `krun-vmm`): `ctx_cfg → VmResources` translation, device-ordering, kernel-cmdline
   assembly, krunfw payload loading, vsock/net heuristics. For M1 it's ~30 lines;
   it grows with each feature. This becomes **limina's** code (in `limina-vmm`), which is
   the point — it's where our policy lives.
2. **Track internal API drift on rebase.** `build_microvm`'s signature,
   `VmResources` shape, and device internals can change between libkrun versions.
   Mitigation: our usage is concentrated in one builder module; keep it small.

## Reproduce

`cargo build` then codesign (`codesign --entitlements hv.entitlements -s - --force
target/debug/boot-internal`) and run like `../m1-boot/run.sh`. To see kernel dmesg
(not just firmware+GRUB), apply `../m1-boot/serial-grub.cfg` to the image ESP first
(see that spike's RESULTS). Image is left pristine-stock after running.
