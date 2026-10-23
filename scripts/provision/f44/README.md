# Fedora 44 enhanced-tier scripts

Two scripts live here:

| Script | What it does |
|---|---|
| `build-kernel-rpm.sh` | Builds the enhanced tier's 16 KiB-page kernel as `limina-kernel-16k` + `-devel` RPMs, plus `limina-kernel-16k-source.tar.gz` (the source reference the payload ships). |
| `install-synoik-session.sh` | Builds synoik inside an enhanced guest and makes it the session compositor — the reproducible half of `Fedora-Workstation-44.enhanced.synoik.raw` (`docs/images.md`). |

Everything else in the enhanced tier is not an RPM: the guest keeps its distro's Mesa, and
limina's GPU drivers install beside it under `/opt/limina` (`scripts/build-gpu-drivers.sh`,
`scripts/package-gpu-drivers.sh`, `guest/gpu-drivers/install.sh`). The payload that carries the
kernel, the drivers and the agents is assembled on the host by `scripts/package-enhanced.sh`.

## The kernel: two places to run it, one script

`build-kernel-rpm.sh` has to run **on an F44 aarch64 system**, so that `rpmbuild` stamps `.fc44`
and the base config is Fedora's own. Either of these is one:

```sh
cargo xtask enhanced                          # from macOS: scripts/build-enhanced-kernel.sh, in the
                                              # unified build container -> target/enhanced-rpms/kernel
scripts/provision/f44/build-kernel-rpm.sh     # inside a booted F44 guest -> ~/limina-build/kernel
```

It builds the `[linux]` fork at the rev pinned in `third_party/manifest.toml` (no patch stage:
the branch is the delta) on Fedora's config with `CONFIG_ARM64_16K_PAGES=y`. The one behavioural
difference between the two places is the base config: in a guest it is the **running** kernel's,
in the container it is the image's `kernel-core` (`CONFIG_BASE` overrides either). Prefer the
container for a repeatable build from a clean checkout and its persistent caches; prefer the guest
for the dogfood signal of a guest building its own kernel. `RELEASE=<n>` bumps the RPM Release
when the content changes at the same kernel version.

Prerequisites for the in-guest run: the repo in the guest (`git clone`, or `limina --share
limina=/path/to/limina:ro` then `mount -t virtiofs limina-limina /mnt/limina`), networking
(`limina --net`: `dnf builddep` and the kernel source), several GB of persistent disk, a roomy
`--memory`, and passwordless `sudo`.

## From kernel to guest

```sh
KERNEL_DIR=<the kernel output dir> NOTE="what changed" scripts/package-enhanced.sh rNN
scripts/provision/deliver-payload.sh payload/limina-guest-tools-f44-rNN.tar.zst <image.raw>...
```

`package-enhanced.sh` wants `scripts/package-gpu-drivers.sh` to have staged the GPU drivers first.
`deliver-payload.sh` boots each image, runs the payload's `install-enhanced.sh` in it and verifies
the result; on a guest you manage by hand, unpack the payload and run
`sudo ./limina-guest-tools/install-enhanced.sh ~/limina-guest-tools`, then reboot. The full
procedure and the current revision are in `docs/images.md`.
