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

cargo xtask test                  # builds + signs worker + builds L1 guest, runs L1 & L2
cargo xtask test --release        # release profile
cargo xtask test -- --test venus  # forward a filter to the test run
LIMINA_TEST_DISK=/path/to.raw cargo xtask test   # override the L2 guest image
```

`cargo xtask test` wraps `scripts/test-boot.sh` (the source of truth — call it directly for
anything the command doesn't expose). Both also run `scripts/build-test-guest.sh` (extracts the
kernel, cross-builds `guest/limina-init`, stages the rootfs); run that standalone to rebuild just
the L1 guest.

## Debugging a failing L2 test

An L2 test boots a whole desktop and runs several apps in it. When one fails, the fault is
somewhere in guest + workload + host, and the cheapest way to be wrong for a day is to start
theorising about the host. Work in this order.

**0. Read the host renderer's log first.** Before any theory about the guest, grep the supervisor
log for `[virglrs] refused:` and `submit_command -> Err`. virglrs poisons a context that raises a
host GL error: every later submit from it is refused, and if that context is the compositor's the
desktop stops being painted while the guest stays perfectly healthy — processes alive, windows
present, last good frame still on screen. The guest is downstream of the renderer, so a guest-side
investigation of a renderer fault finds only symptoms. `renderer::assert_renderer_served` makes
this automatic for a test that carries it; do it by hand for one that does not.

**1. Halve the workload before theorising.** Run each app alone, then in pairs
(`LIMINA_L2_WORKLOAD=nautilus` in `l2_desktop_restore_landmarks`, `VKSTILL_IDLE_AFTER` and the
app list elsewhere). One boot per app is minutes; it names the culprit or proves the fault needs
a combination, and either answer redirects everything after it. A whole day has gone into the
display path of a failure that one app reproduced on its own.

**2. Ask whether the guest is doing what you think.** The guest's own state is one ssh away and
is not guessable from the host: `gdbus call … Mutter.DisplayConfig.GetCurrentState` for the mode
and scale actually in force, `/sys/kernel/debug/dri/0/framebuffer` and `/state` for what KMS is
scanning out and who allocated it, `journalctl --user` for what the session thinks. A parameter
named in a doc comment ("the guest drops to 100%") is a design intent until something reads it
back.

**3. Count restarts, not processes.** `pgrep -c` says 1 whether that process is the original or
the ninth. A relaunch cycle shows up in `systemctl --user show -p NRestarts`, in failed units,
and in process ages — never in a count.

**4. Put the probe inside the window the fault happens in.** Diagnostics gathered before the
symptom appears report a healthy system, truthfully and uselessly. If the failure is a 90-second
settle that times out, read the guest again *at the timeout*, not only before it.

**5. Beware oracles that cannot fail.** `pgrep -f <pattern>` matches the shell running it, so a
count keyed on it never reaches zero and the assertion built on it never fires. A log needle is
a claim about text another repository writes, and it rots silently. Pair any "nothing bad in the
log" check with a line the code writes unconditionally, and assert on that too.

Two failures of this rule cost the same investigation twice over. The renderers used to poison in
different words, so checking venus's needle, finding it clean and concluding "the renderer refuses
nothing" read vrend's 46318 refusals as silence — one needle for a two-renderer question. And the
line that finally named the fault had been *printed* on three earlier runs, inside a DIAG dump,
where nothing asserted on it. **A diagnostic is not an oracle.** If a line would change the
verdict, assert on it; if it would not, it does not belong in the dump.

**6. Prefer a window when the question is "what does it look like".**
`with_windowed_coexist_display` opens a real window; a person watching it for thirty seconds
answers questions no capture PNG can ("the dash was throbbing"). Note the two display flags are
mutually exclusive, so a windowed boot writes no capture and the pixel oracles cannot run — this
is for looking, not for gating.

### Environment overrides

| Var | Default |
|---|---|
| `LIMINA_HVF_TESTS` | unset → boot tests skip; set `1` to run them |
| `LIMINA_BIN` / `LIMINA_VMM_BIN` | the binaries next to the test in `target/<profile>/` |
| `LIMINA_FIRMWARE` | `/opt/homebrew/share/krunkit/KRUN_EFI.silent.fd` (L2) |
| `LIMINA_TEST_DISK` | `Fedora-Workstation-43.raw` at the repo root (L2) |
| `LIMINA_TEST_KERNEL` / `LIMINA_TEST_ROOTFS` | `target/test-guest/{Image,rootfs}` (L1) |
| `LIMINA_TEST_KERNEL_16K` | `target/test-guest/kernel/Image-16k` (enhanced/venus L2, 6.12) |
| `LIMINA_TEST_KERNEL_71` | `target/test-guest/kernel/Image-16k-71` (≥7.1 virtiofs share guard, `l2_share_71`) |
| `LIMINA_TEST_CMDLINE` | `console=ttyAMA0 rootfstype=virtiofs rw init=/init` (L1) |
| `LIMINA_TEST_SHUTDOWN_GRACE` | `3` (seconds the supervisor waits before force-kill) |

CI needs a **self-hosted Apple-Silicon runner** (hosted macOS runners can't do
hypervisor); the multi-GB Fedora image is hosted out-of-repo.
