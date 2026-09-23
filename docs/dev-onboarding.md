# Dev onboarding

The short path from a fresh clone to a running, tested limina. Everything below is one
`cargo xtask` command; each wraps a tested script under `scripts/` (the source of truth),
so when you need a knob a command doesn't expose, reach for the script it wraps. Run
`cargo xtask --help` for the full list.

## 0. Prerequisites

- macOS on Apple Silicon (developed on macOS 26.5, M1 Max, 16 KiB host pages).
- Rust (stable), full Xcode + command-line tools (codesign, otool).
- Homebrew: `molten-vk vulkan-loader gvproxy libusb cmake meson ninja llvm
  spirv-llvm-translator spirv-tools bison`. The keg-only ones (`llvm`, `bison`, and `expat`
  where it exists) are what the host Mesa build needs on `PATH` / `PKG_CONFIG_PATH`;
  `scripts/build-host-mesa.sh` puts them there for you but cannot install them.
  **Not `libclc`** — it is version-coupled to the Mesa rev and pinned in
  `third_party/manifest.toml` instead, fetched and digest-checked by the build (step 1.5). (`qemu` for `qemu-img`, `glslang` for `scripts/gen-vkstill-spv.sh`, and
  `cargo-nextest` for a parallel suite are each used by one script and optional until you
  run it. `libkrun`/`libkrunfw`/`krunkit`/`virglrenderer` are **not** needed — we build our
  own forks, and the C virglrenderer is no longer loaded at runtime at all.)
- **`python3`** — virglrs's build script generates the venus wire and the vrend format
  tables at compile time through a bare `python3`, so this is a *build* dependency of the
  workspace, not just of Mesa. The modules it imports (mako, pyyaml) come from
  `third_party/venv-mesa`, which `cargo xtask vendor` creates and `cargo xtask build`
  puts on the child's `PATH` — nothing is installed into the host's Python.
- The host **KosmicKrisp / zink-on-KK Mesa builds** live on `third_party/mesa-cs.sparseimage`
  (mounted at `/Volumes/mesa-cs`), not in the repo, because Mesa will not check out on a
  case-insensitive filesystem. **`cargo xtask build` links `libEGL` out of that prefix**, so
  this is required to build at all, not only to run venus — see step 1.5.
- Apple **`container`** (`brew install container`, then
  `container system start --enable-kernel-install`) — for every Linux build: the firmware, the
  test kernels, and the enhanced-tier guest RPMs. The boot suite's default firmware is one of
  its outputs, so the suite needs this. See step 6.
- A guest disk image (`*.raw`, gitignored). Inventory + how they're built:
  `docs/images.md`.

## 1. Bootstrap

```sh
cargo xtask setup
```

`setup` = `vendor` + enable the in-repo git hooks (`fmt` + `clippy` pre-commit).
`vendor` clones the fork-model deps (libkrun, virglrs, imago) from github.com/liminavm at
the rev `third_party/manifest.toml` pins — the fork's branch **is** the delta, so there is
no patch series to apply — lets virglrs vendor its own dependencies (the C virglrenderer
it generates tables from, and the venus-protocol), and creates `third_party/venv-mesa`.
Idempotent; safe to re-run, and the way to repair a `third_party/` tree you deleted.

> The native deps (the host **KK/zink Mesa**, the **GOP KRUN_EFI** firmware, the guest
> **16 KiB kernel / Mesa / agent** RPMs) are heavier, container/`meson`-driven builds that
> stay as their own scripts — `vendor` only materializes source trees. See
> `docs/codebases.md` for which script builds what.

## 1.5. Host Mesa (once per machine)

```sh
cargo xtask mesa                 # KosmicKrisp + zink-on-KK; both halves
cargo xtask mesa kk              # just the ICD, e.g. after a KK-only change
```

`mesa` (= `scripts/build-host-mesa.sh`) creates the case-sensitive sparse image, mounts it,
clones `liminavm/mesa` at the `[kosmickrisp]` pin and builds **both** halves, because two
different consumers need one each:

| Output | Who opens it |
|---|---|
| `/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/` (dylib + devenv ICD json) | `VK_ICD_FILENAMES` for every boot script; `build-app.sh`'s `KK_DRIVER` |
| `/Volumes/mesa-cs/zink-kk-prefix/lib/` (`libEGL` + `libgallium`) | virglrs's `build.rs` links `libEGL`; `build-app.sh` bundles both |

It is a long build the first time and incremental after that. Skip it only if you already
have the volume, or if you point `EGL_LIB_DIR` at a Mesa prefix of your own — `cargo xtask
build` checks for the prefix up front and says so rather than failing deep in a build
script. Asserts are compiled out by default (`-Db_ndebug=true`): a Mesa assert reached from
the guest `SIGABRT`s the worker and takes the VM down, so `BUILDTYPE=debug` is for active
Mesa debugging and never for a bundle. Background + toolchain traps:
`docs/drivers/kosmickrisp.rst`.

## 2. Inner loop

```sh
cargo xtask build          # cargo build limina + limina-vmm, then codesign the worker
cargo xtask sign           # just re-codesign the worker (after a plain `cargo build`)
```

`build` produces a runnable, codesigned worker. The traps it guards for you:

- **The worker needs the `com.apple.security.hypervisor` entitlement** (for `hv_vm_*`) —
  `build`/`sign` codesign it (`crates/limina-vmm/sign.sh`). Anything that relinks the
  worker strips it again, so a plain `cargo build` is always followed by `sign`.
- **The host Mesa prefix and the Python venv** have to be there before `cargo` starts —
  `build` checks the first (and remounts the volume macOS drops on reboot) and puts the
  second on the child's `PATH`. Without them the failure would otherwise surface as a
  panic inside virglrs's build script, hundreds of crates in.

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
  `docs/images.md` §SSH access.
- **Verify venus in the seated GNOME session, not over ssh:** `vulkaninfo` in the desktop
  shows `Virtio-GPU Venus`; over a non-login ssh shell it enumerates nothing (a false
  negative — the venus ICD is selected via `/etc/environment.d`).

## 4. Linux-side builds (firmware + the enhanced tier)

Everything that has to be built *on Linux* runs in one container image —
`limina-build:fc<FEDORA_REL>`, Fedora 44 by default (`scripts/build-image.sh`, built on first
use). There is no second image and no second toolchain:

```sh
cargo xtask firmware               # target/krun-efi/KRUN_EFI.gop.fd — the suite's default firmware
cargo xtask enhanced               # 16k kernel + venus mesa RPMs + agents + install-ready payload
cargo xtask enhanced kernel        # just one component
```

`enhanced` (= `scripts/build-enhanced-rpms.sh`) runs `scripts/provision/f44/*.sh` — **the same
scripts a booted guest runs**, not a second implementation. Those need an F44 aarch64 system,
which is what the image is; that they once had to run inside a guest was a fact about the image
being pinned to Fedora 43, not about containers. Run them in a guest instead when you want the
dogfood signal of a guest building its own components — `scripts/provision/f44/README.md` has
that path. Moving the whole toolchain to a new Fedora is `FEDORA_REL=45 FORCE=1
scripts/build-image.sh`.

## 5. Validate

```sh
cargo xtask test                       # the whole HVF-gated boot suite
cargo xtask test -- --test venus       # one binary (forwarded to the test run)
```

`test` (= `scripts/test-boot.sh`, `LIMINA_HVF_TESTS=1`) builds, codesigns, link-checks,
builds the L1 guest + trap probe, and runs the boot tests against real HVF. Its default
firmware is step 4's `target/krun-efi/KRUN_EFI.gop.fd` (`LIMINA_FIRMWARE` overrides). **This is the
"did I break boot" command** — a plain `cargo test` deliberately *skips* the HVF tests
(no codesign/sandbox), so green there means almost nothing for boot behavior. It needs
sandbox-disabled execution (it hits `hv_vm_*`).

## 6. Package

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
