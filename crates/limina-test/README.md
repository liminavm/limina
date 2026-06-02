# limina-test

End-to-end / regression test harness for limina. It drives the **shipped binaries** —
`limina` (supervisor) → `limina-vmm` (worker) → libkrun/HVF — exactly as a user would, with
**no shortcuts to libkrun's internal API**. If a test here passes, the real thing works.

## What's here

- `src/lib.rs` — the harness. [`Guest::boot`] launches the `limina` supervisor and captures
  the guest serial console; [`Guest::wait_for`] awaits a marker; [`Guest::shutdown`] (and
  `Drop`) tear it down so a panicking assertion can never leak a live VM holding HVF.
- `tests/l1_boot.rs` — **L1** fast test: our tiny direct-boot guest reaches userspace and
  powers off cleanly (worker exit 0) in ~0.4s.
- `tests/boot.rs` — **L2** stock-baseline test: the unmodified Fedora `.raw` (opened
  **read-only**) must boot through firmware + GRUB.

## Test layers

| Layer | Guest | Speed | Proves |
|---|---|---|---|
| **L0** | none (no HVF) | ms | facade/supervisor logic — lives in each crate, not here |
| **L1** | tiny kernel + virtio-fs rootfs + Rust `init` | ~0.4s | reaches *our* userspace + clean power-off |
| **L2** | stock Fedora `.raw` (read-only) | ~4s | the user-facing chain boots; compatibility floor |

The **L1 guest** (`guest/limina-init` + `scripts/build-test-guest.sh`) boots a kernel Image
directly (libkrun `ExternalKernel`) with its root served over virtio-fs from a host
directory — so it reaches *our* init, which prints a marker and powers off via PSCI. It's
the workhorse for the RED-first rule. The kernel is our **custom 6.12** build
(`scripts/build-test-kernel.sh`, via Apple `container`); if you haven't built one,
`build-test-guest.sh` falls back to libkrunfw's bundled Image. See `docs/roadmap.md`.

For **L2**, a pristine Fedora image has no `console=`, so the kernel goes silent after
GRUB — reaching GRUB proves the whole chain (limina → firmware → virtio-blk → ESP →
bootloader). Userspace/feature assertions belong to L1.

## Running

Boot tests touch Hypervisor.framework, so they need the worker **codesigned** with
`com.apple.security.hypervisor` and the gate on. Plain `cargo test` **skips** them
(prints `SKIPPED …`), keeping the default loop green:

```sh
cargo test -p limina-test           # L0-style: boot tests skip

scripts/test-boot.sh              # builds + signs worker + builds L1 guest, runs L1 & L2
scripts/test-boot.sh release      # release profile
LIMINA_TEST_DISK=/path/to.raw scripts/test-boot.sh   # override the L2 guest image
```

`test-boot.sh` also runs `scripts/build-test-guest.sh` (extracts the kernel, cross-builds
`guest/limina-init`, stages the rootfs); run that standalone to rebuild just the L1 guest.

### Environment overrides

| Var | Default |
|---|---|
| `LIMINA_HVF_TESTS` | unset → boot tests skip; set `1` to run them |
| `LIMINA_BIN` / `LIMINA_VMM_BIN` | the binaries next to the test in `target/<profile>/` |
| `LIMINA_FIRMWARE` | `/opt/homebrew/share/krunkit/KRUN_EFI.silent.fd` (L2) |
| `LIMINA_TEST_DISK` | `Fedora-Workstation-43.raw` at the repo root (L2) |
| `LIMINA_TEST_KERNEL` / `LIMINA_TEST_ROOTFS` | `target/test-guest/{Image,rootfs}` (L1) |
| `LIMINA_TEST_CMDLINE` | `console=ttyAMA0 rootfstype=virtiofs rw init=/init` (L1) |
| `LIMINA_TEST_SHUTDOWN_GRACE` | `3` (seconds the supervisor waits before force-kill) |

CI needs a **self-hosted Apple-Silicon runner** (hosted macOS runners can't do
hypervisor); the multi-GB Fedora image is hosted out-of-repo.
