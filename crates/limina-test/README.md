# limina-test

End-to-end / regression test harness for limina. It drives the **shipped binaries** —
`limina` (supervisor) → `limina-vmm` (worker) → libkrun/HVF — exactly as a user would, with
**no shortcuts to libkrun's internal API**. If a test here passes, the real thing works.

## What's here

- `src/lib.rs` — the harness. [`Guest::boot`] launches the `limina` supervisor and captures
  the guest serial console; [`Guest::wait_for`] awaits a marker; [`Guest::shutdown`] (and
  `Drop`) tear it down so a panicking assertion can never leak a live VM holding HVF.
- `tests/boot.rs` — **L2** stock-baseline test: the unmodified Fedora `.raw` (opened
  **read-only**) must boot through firmware + GRUB.

## Test layers

| Layer | Guest | Speed | Proves |
|---|---|---|---|
| **L0** | none (no HVF) | ms | facade/supervisor logic — lives in each crate, not here |
| **L1** | our tiny kernel + Rust `init` | <1s | rich, vsock-asserted functionality *(next to build)* |
| **L2** | stock Fedora `.raw` (read-only) | ~5s | the user-facing chain boots; compatibility floor |

On a pristine Fedora image the kernel has no `console=`, so it goes silent after GRUB —
reaching GRUB proves the whole chain (limina → firmware → virtio-blk → ESP → bootloader).
Userspace/feature assertions belong to **L1**, where our own init prints a marker (and
talks vsock) with zero image surgery.

## Running

Boot tests touch Hypervisor.framework, so they need the worker **codesigned** with
`com.apple.security.hypervisor` and the gate on. Plain `cargo test` **skips** them
(prints `SKIPPED …`), keeping the default loop green:

```sh
cargo test -p limina-test           # L0-style: boot tests skip

scripts/test-boot.sh              # builds, signs the worker, runs boot tests (debug)
scripts/test-boot.sh release      # release profile
LIMINA_TEST_DISK=/path/to.raw scripts/test-boot.sh   # override the guest image
```

### Environment overrides

| Var | Default |
|---|---|
| `LIMINA_HVF_TESTS` | unset → boot tests skip; set `1` to run them |
| `LIMINA_BIN` / `LIMINA_VMM_BIN` | the binaries next to the test in `target/<profile>/` |
| `LIMINA_FIRMWARE` | `/opt/homebrew/share/krunkit/KRUN_EFI.silent.fd` |
| `LIMINA_TEST_DISK` | `Fedora-Workstation-43.raw` at the repo root |
| `LIMINA_TEST_SHUTDOWN_GRACE` | `3` (seconds the supervisor waits before force-kill) |

CI needs a **self-hosted Apple-Silicon runner** (hosted macOS runners can't do
hypervisor); the multi-GB Fedora image is hosted out-of-repo.
