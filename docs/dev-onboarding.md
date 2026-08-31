# Dev onboarding

The short path from a fresh clone to a running, tested limina. Everything below is one
`cargo xtask` command; each wraps a tested script under `scripts/` (the source of truth),
so when you need a knob a command doesn't expose, reach for the script it wraps. Run
`cargo xtask --help` for the full list.

## 0. Prerequisites

- macOS on Apple Silicon (developed on macOS 26.5, M1 Max, 16 KiB host pages).
- Rust (stable), full Xcode + command-line tools (codesign, otool).
- Homebrew VM stack: `libkrun krunkit libkrunfw virglrenderer molten-vk vulkan-loader
  gvproxy libusb qemu cmake meson ninja`.
- The host **KosmicKrisp / zink-on-KK Mesa builds** live on `third_party/mesa-cs.sparseimage`
  (mounted at `/Volumes/mesa-cs`), not in the repo — needed for venus. `cargo xtask run`
  and the venus tests auto-mount it (`scripts/ensure-mesa-cs.sh`); see `docs/codebases.md`
  for how it's produced.
- A guest disk image (`*.raw`, gitignored). Inventory + how they're built:
  `docs/images.md`.

## 1. Bootstrap

```sh
cargo xtask setup
```

`setup` = `vendor` (clone libkrun + virglrenderer if absent, apply every committed patch
series, vendor+patch imago — i.e. recreate the gitignored `third_party/` trees) + enable
the in-repo git hooks (`fmt` + `clippy` pre-commit). Idempotent; safe to re-run. Run
`vendor` on its own after re-cloning a `third_party/` tree.

> The native deps (patched **virglrenderer** into `third_party/virgl-prefix`, the host
> **KK/zink Mesa**, the **GOP KRUN_EFI** firmware, the guest **16 KiB kernel / Mesa /
> agent** RPMs) are heavier, container/`meson`-driven builds that stay as their own
> scripts — `vendor` only recreates the source trees + applies patches. See
> `docs/codebases.md` for which script builds what.

## 2. Inner loop

```sh
cargo xtask build          # cargo build limina + limina-vmm, codesign the worker, virgl link-check
cargo xtask sign           # just re-codesign the worker (after a plain `cargo build`)
```

`build` produces a runnable, codesigned worker. Two traps it guards for you:

- **The worker needs the `com.apple.security.hypervisor` entitlement** (for `hv_vm_*`) —
  `build`/`sign` codesign it (`crates/limina-vmm/sign.sh`).
- **The worker MUST link our `third_party/virgl-prefix` virglrenderer, not Homebrew's** —
  a wrong link silently degrades venus to software-2D and reads like a guest bug.
  `build` runs `check-virgl-link.sh` and fails loudly if it's wrong (see the
  `limina-virgl-link-trap` note).

## 3. Run it

```sh
cargo xtask run --disk <enhanced.raw>              # seated venus desktop in a window
cargo xtask run --disk <enhanced.raw> --no-net --cpus 4 --ram-mib 4096
cargo xtask run --disk <enhanced.raw> -- --no-normalize-modifiers  # trailing flags go to `limina`
```

This is the **default boot: EFI + venus** — the guest's own installed kernel via our GOP
firmware → GRUB → BLS entry, enforcing SELinux, coexist venus (3D + software-2D) on
KosmicKrisp, windowed, with user-mode NAT. It tests the image exactly as it really runs.
`run` builds+signs the worker and mounts `/Volumes/mesa-cs` first, then hands off to
`spikes/venus-draw-probe/boot-enhanced-efi-kk.sh`.

- The disk boots **in place** — clone it (`cp -c src.raw work.raw`, instant CoW) if you
  want to keep it pristine.
- Fringe boot modes stay as their own scripts: `--kernel-inject` (deterministic external
  test kernel) and `--gpu-software-2d` (software-2D subject only). Don't reach for them by
  habit — EFI+venus is the default. See `CLAUDE.md`.
- Networking: read the auto-allocated SSH port from the worker log
  (`guest SSH forward ready: ssh -p N …`) — don't assume 2222. Creds + recipe:
  the `limina-fedora-access` memory.
- **Verify venus in the seated GNOME session, not over ssh:** `vulkaninfo` in the desktop
  shows `Virtio-GPU Venus`; over a non-login ssh shell it enumerates nothing (a false
  negative — the venus ICD is selected via `/etc/environment.d`).

## 4. Validate

```sh
cargo xtask test                       # the whole HVF-gated boot suite
cargo xtask test -- --test venus       # one binary (forwarded to the test run)
```

`test` (= `scripts/test-boot.sh`, `LIMINA_HVF_TESTS=1`) builds, codesigns, link-checks,
builds the L1 guest + trap probe, and runs the boot tests against real HVF. **This is the
"did I break boot" command** — a plain `cargo test` deliberately *skips* the HVF tests
(no codesign/sandbox), so green there means almost nothing for boot behavior. It needs
sandbox-disabled execution (it hits `hv_vm_*`).

## 5. Package

```sh
cargo xtask app        # full self-contained target/Limina.app (the shipping deliverable)
cargo xtask bundle     # minimal Limina-smoke.app that boots the L1 guest (launch-path smoke test)
```

`app` (= `scripts/build-app.sh`) vendors the whole host venus/GL dylib closure into the
bundle, relocated to `@rpath`, and signs with the Apple-Development identity when one is in
the keychain (keeps TCC grants stable across redeploys). The dogfood deliverable is
`target/Limina.app` copied to the *other* Mac — never installed into `/Applications` on the
dev Mac.

`bundle` writes a **different** path on purpose. It is debug-by-default and ad-hoc signed, so
it cannot carry TCC grants (Accessibility is pinned to a CDHash); at `target/Limina.app` it
would be indistinguishable from the deliverable. Anything a human is meant to run — poking a
change, a dogfood drop — comes from `app`.

## Where things are

`docs/roadmap.md` (milestone status) · `docs/codebases.md` (the source-tree map: host vs
guest, which script builds what) · `docs/images.md` (disk-image inventory + component
versions) · `docs/graphics.md` (the GPU tier ladder, present path, pitfalls) · `CLAUDE.md` (project tenets,
working conventions, environment quirks).
