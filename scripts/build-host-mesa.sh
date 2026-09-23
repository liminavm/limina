#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Build the HOST Mesa from nothing: KosmicKrisp (the Vulkan-on-Metal ICD venus renders
# through) and the zink-on-KK superset (host GL for vrend, and the libEGL virglrs links).
#
# This is the bootstrap step that used to exist only in one developer's home directory.
# The source tree cannot live in the repo working copy -- macOS APFS is case-insensitive and
# Mesa does not check out cleanly on it -- so it lives on a case-sensitive sparse image.
# Every consumer assumed that image already existed: scripts/ensure-mesa-cs.sh only *attaches*
# one, spikes/virgl-zink-kk/build-mesa-zink-kk.sh bails unless the source is already cloned,
# and virglrs's build.rs hard-fails without the prefix -- so `cargo build` itself did not
# complete on a machine that had never been handed the image. This script creates it.
#
# Outputs -- BOTH are required, by different consumers:
#   /Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/
#       libvulkan_kosmickrisp.dylib + kosmickrisp_mesa_devenv_icd.<arch>.json
#       (VK_ICD_FILENAMES for every boot script; build-app.sh's KK_DRIVER)
#   /Volumes/mesa-cs/zink-kk-prefix/lib/
#       libEGL.dylib + libgallium-*-devel.dylib
#       (virglrs's build.rs links libEGL; scripts/build-app.sh bundles both)
#
# Usage: scripts/build-host-mesa.sh [kk|zink|both]        (default: both)
#   BUILDTYPE=debug       active KK/zink debugging: adds mesa_logd and -O0, and keeps asserts
#                         (see docs/drivers/kosmickrisp.rst -- do NOT ship a bundle from it)
#   NDEBUG=true|false     override the assert decision on its own
#   JOBS=N                ninja parallelism (default: ninja's own)
#   MESA_CS_SIZE=80g      max size of a freshly created sparse image (sparse: grows on demand)
#   LIBCLC_PC_DIR=<dir>   a directory holding libclc.pc, searched before Homebrew's -- for a
#                         host whose packaged libclc no longer matches the pinned Mesa rev
# Prereqs: brew install llvm bison expat molten-vk meson ninja; network on the first run.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

MOUNT="/Volumes/mesa-cs"
IMAGE="$ROOT/third_party/mesa-cs.sparseimage"
SRC="$MOUNT/mesa"

WHAT="${1:-both}"
case "$WHAT" in kk|zink|both) ;; *) echo "usage: $0 [kk|zink|both]" >&2; exit 1 ;; esac

# ---- the case-sensitive volume ------------------------------------------------------------
# hdiutil's SPARSE image grows on demand, so the size is a ceiling, not an allocation: the
# two build dirs plus the source run well under 20 GiB today.
if [ ! -e "$IMAGE" ]; then
  echo "==> creating $IMAGE (case-sensitive APFS, ${MESA_CS_SIZE:-80g} ceiling, sparse)"
  hdiutil create -type SPARSE -fs "Case-sensitive APFS" -volname mesa-cs \
      -size "${MESA_CS_SIZE:-80g}" "$IMAGE" >/dev/null
fi
# shellcheck source=scripts/ensure-mesa-cs.sh
. "$ROOT/scripts/ensure-mesa-cs.sh"

# ---- toolchain ------------------------------------------------------------------------------
command -v brew >/dev/null || { echo "Homebrew is required to locate the keg-only toolchain" >&2; exit 1; }
# KosmicKrisp compiles OpenCL C to SPIR-V to MSL, so it needs the whole CLC path, not just
# llvm-config: libclc and spirv-llvm-translator are separate formulae and meson only says
# "Dependency libclc not found" when one is absent. bison is for zink's GLSL glcpp grammar,
# molten-vk for the headers zink's darwin path compiles against.
# expat is deliberately NOT here: it is keg-only when present but the build does not require
# it (the reference machine has no expat formula at all), so it only joins PKG_CONFIG_PATH
# below, where a missing directory is ignored.
for f in llvm libclc spirv-llvm-translator bison molten-vk; do
  [ -d "$(brew --prefix "$f")" ] || { echo "missing Homebrew $f (brew install $f)" >&2; exit 1; }
done
for t in meson ninja; do
  command -v "$t" >/dev/null || { echo "missing $t (brew install $t)" >&2; exit 1; }
done

# All three are keg-only, so a stock `meson setup` silently misses them, and meson bakes the
# bison path into build.ninja at CONFIGURE time -- they have to be on PATH before the first
# setup, not before the first ninja. Apple's /usr/bin/bison is 2.3 (2008) and cannot parse
# Mesa's GLSL glcpp grammar; KK alone never builds glcpp, but the zink/GL superset does.
export PATH="$(brew --prefix bison)/bin:$(brew --prefix llvm)/bin:$PATH"
export PKG_CONFIG_PATH="$(brew --prefix)/lib/pkgconfig:$(brew --prefix)/share/pkgconfig:$(brew --prefix expat)/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"

# Putting Homebrew LLVM's bin on PATH for llvm-config also shadows Apple's `clang` with
# Homebrew's -- and unlike Apple's, that clang resolves `ld` through PATH. So a developer who
# made an alternative linker their default (`~/.local/bin/ld -> mold` is a common Rust setup)
# hands Mesa's Mach-O link to a linker that cannot do Mach-O, and meson dies during linker
# detection with `mold: fatal: unknown command line option: -dynamic`. Mesa on macOS links only
# with Apple's ld, so pin it for this build without touching anyone's default.
APPLE_LD="$(xcrun -f ld 2>/dev/null || echo /usr/bin/ld)"
LD_ON_PATH="$(command -v ld || true)"
if [ -x "$APPLE_LD" ] && [ "$LD_ON_PATH" != "$APPLE_LD" ]; then
  LD_SHIM="$ROOT/target/host-mesa/bin"
  mkdir -p "$LD_SHIM"
  ln -sf "$APPLE_LD" "$LD_SHIM/ld"
  export PATH="$LD_SHIM:$PATH"
  echo "==> \`ld\` on PATH is ${LD_ON_PATH:-<none>}, not $APPLE_LD — shimming Apple's ld for this build"
fi

# libclc is version-coupled to the Mesa rev in a way `brew install libclc` does not express.
# Mesa finds it only through pkg-config, and then bakes DYNAMIC_LIBCLC_PATH from that file's
# `libexecdir` and opens `spirv64-mesa3d-.spv` under it AT RUNTIME (meson.options makes
# static-libclc empty by default, so the SPIR-V is not embedded). libclc 22.x ships both the
# .pc and those filenames; 23.x dropped the .pc and renamed the payload to
# `<target>/libclc.spv`. So a newer libclc does not merely fail to configure -- with a
# hand-written .pc it would configure, compile, and then fail when the guest first reaches
# CLC. Check the shape, not the presence.
# A machine whose Homebrew no longer offers a compatible libclc can point at one it has
# some other way (an extracted keg, a source build); it joins the search path ahead of brew's.
[ -n "${LIBCLC_PC_DIR:-}" ] && export PKG_CONFIG_PATH="$LIBCLC_PC_DIR:$PKG_CONFIG_PATH"
if ! pkg-config --exists libclc 2>/dev/null; then
  echo "libclc has no pkg-config file — Mesa locates it only that way." >&2
  echo "Homebrew's libclc 23.x dropped it (22.x shipped share/pkgconfig/libclc.pc); the Mesa" >&2
  echo "rev pinned in third_party/manifest.toml needs the 22.x layout. Install a libclc whose" >&2
  echo "libexecdir holds spirv-mesa3d-.spv and spirv64-mesa3d-.spv." >&2
  exit 1
fi
CLC_BASEDIR="$(pkg-config --variable=libexecdir libclc)"
for f in spirv-mesa3d-.spv spirv64-mesa3d-.spv; do
  [ -e "$CLC_BASEDIR/$f" ] && continue
  echo "libclc at $CLC_BASEDIR is missing $f — Mesa mmaps it at runtime (DYNAMIC_LIBCLC_PATH)," >&2
  echo "so this build would link and then fail the first time the guest reaches CLC." >&2
  echo "That is libclc 23.x's layout (<target>/libclc.spv); the pinned Mesa rev needs 22.x's." >&2
  exit 1
done

# Mesa's codegen is mako-driven; the venv also carries what virglrs's generators need.
# shellcheck source=scripts/ensure-venv-mesa.sh
. "$ROOT/scripts/ensure-venv-mesa.sh"

# ---- source, at the pin ---------------------------------------------------------------------
pin() { awk -v k="$1" '/^\[kosmickrisp\]/{f=1;next} /^\[/{f=0} f && $1==k {gsub(/"/,"",$3); print $3; exit}' third_party/manifest.toml; }
MESA_REPO="$(pin repo)"; MESA_BRANCH="$(pin branch)"; MESA_REV="$(pin rev)"
[ -n "$MESA_REPO" ] && [ -n "$MESA_BRANCH" ] && [ -n "$MESA_REV" ] \
  || { echo "incomplete [kosmickrisp] entry in third_party/manifest.toml" >&2; exit 1; }

if [ ! -d "$SRC/.git" ]; then
  echo "==> cloning $MESA_REPO ($MESA_BRANCH) into $SRC"
  git clone --branch "$MESA_BRANCH" "$MESA_REPO" "$SRC"
  git -C "$SRC" remote add upstream "$(awk '/^\[kosmickrisp\]/{f=1;next} /^\[/{f=0} f && $1=="upstream" {gsub(/"/,"",$3); print $3; exit}' third_party/manifest.toml)" 2>/dev/null || true
fi
git -C "$SRC" cat-file -e "${MESA_REV}^{commit}" 2>/dev/null \
  || { echo "==> fetching mesa (pinned rev $MESA_REV not present)"; git -C "$SRC" fetch origin --tags; }
# Same rule as `cargo xtask vendor`: only move the branch when the pin is missing from it, so
# local work on limina-kk survives a re-run.
if git -C "$SRC" merge-base --is-ancestor "$MESA_REV" "$MESA_BRANCH" 2>/dev/null; then
  git -C "$SRC" checkout --quiet "$MESA_BRANCH"
else
  echo "==> mesa: moving $MESA_BRANCH to the pinned rev $MESA_REV"
  git -C "$SRC" checkout --quiet -B "$MESA_BRANCH" "$MESA_REV"
fi
# The pin is a claim; the checkout HEAD is the fact. Nothing downstream records which rev a
# dylib came from, so say it here.
echo "==> mesa source: $SRC @ $(git -C "$SRC" rev-parse HEAD) ($MESA_BRANCH; pin $MESA_REV)"

# ---- build ------------------------------------------------------------------------------------
BUILDTYPE="${BUILDTYPE:-debugoptimized}"
# Asserts OFF for anything the .app will bundle: a Mesa assert reached from the guest SIGABRTs
# the worker and takes the whole VM down, and meson's default (b_ndebug=if-release) leaves them
# live in a debugoptimized build. An explicit BUILDTYPE=debug is active debugging, so keep them.
if [ "$BUILDTYPE" = "debug" ]; then NDEBUG="${NDEBUG:-false}"; else NDEBUG="${NDEBUG:-true}"; fi

NINJA_JOBS=()
[ -n "${JOBS:-}" ] && NINJA_JOBS=(-j "$JOBS")

# `ninja` re-runs meson when build.ninja is stale, so the setup flags below have to stay
# reachable from the environment exported above -- that is why PATH is set once, up top.
setup_build() {   # setup_build <builddir> <meson args...>
  local build="$1"; shift
  if [ -d "$build" ]; then
    meson setup --reconfigure "$build" "$SRC" "$@"
  else
    meson setup "$build" "$SRC" "$@"
  fi
}

COMMON=(
  -Dplatforms=macos
  -Dvulkan-drivers=kosmickrisp
  -Dzstd=disabled
  -Dprefer_static=true
  -Dbuildtype="$BUILDTYPE"
  -Db_ndebug="$NDEBUG"
)

if [ "$WHAT" = "kk" ] || [ "$WHAT" = "both" ]; then
  echo "==> KosmicKrisp: $MOUNT/build-kk (buildtype=$BUILDTYPE b_ndebug=$NDEBUG)"
  setup_build "$MOUNT/build-kk" "${COMMON[@]}" -Dgallium-drivers= -Dopengl=false
  ninja -C "$MOUNT/build-kk" ${NINJA_JOBS[@]+"${NINJA_JOBS[@]}"}
fi

if [ "$WHAT" = "zink" ] || [ "$WHAT" = "both" ]; then
  echo "==> zink-on-KK: $MOUNT/build-zink-kk -> $MOUNT/zink-kk-prefix (buildtype=$BUILDTYPE b_ndebug=$NDEBUG)"
  # The GL superset of the same tree: zink (GL->Vulkan) over KK, with a headless surfaceless
  # EGL. `macos` is a valid windowing platform but NOT a valid egl-native-platform, so the
  # default would pick an undefined _EGL_PLATFORM_MACOS and fail to compile -- pin it to
  # surfaceless, which is exactly what vrend wants anyway. zink's darwin path needs the
  # MoltenVK headers to COMPILE; at runtime it talks to whatever ICD VK_DRIVER_FILES names.
  setup_build "$MOUNT/build-zink-kk" "${COMMON[@]}" \
      -Dgallium-drivers=zink \
      -Dopengl=true \
      -Dgles2=enabled \
      -Degl=enabled \
      -Degl-native-platform=surfaceless \
      -Dglx=disabled \
      -Dglvnd=disabled \
      -Dshared-llvm=enabled \
      -Dmoltenvk-dir="$(brew --prefix molten-vk)" \
      --prefix "$MOUNT/zink-kk-prefix"
  ninja -C "$MOUNT/build-zink-kk" ${NINJA_JOBS[@]+"${NINJA_JOBS[@]}"}
  meson install -C "$MOUNT/build-zink-kk"
fi

# ---- verify what the consumers actually open --------------------------------------------------
# Each check names the consumer, because "the Mesa build worked" is not the useful claim:
# what matters is whether the next step can find its input.
fail=0
check() { [ -e "$2" ] && echo "    ok   $1: $2" || { echo "    MISS $1: $2" >&2; fail=1; }; }

echo "==> verifying"
if [ "$WHAT" = "kk" ] || [ "$WHAT" = "both" ]; then
  KKDIR="$MOUNT/build-kk/src/kosmickrisp/vulkan"
  check "build-app.sh KK_DRIVER" "$KKDIR/libvulkan_kosmickrisp.dylib"
  icd=("$KKDIR"/kosmickrisp_mesa_devenv_icd.*.json)
  if [ -e "${icd[0]}" ]; then echo "    ok   boot VK_ICD_FILENAMES: ${icd[0]}"; else
    echo "    MISS boot VK_ICD_FILENAMES: $KKDIR/kosmickrisp_mesa_devenv_icd.<arch>.json" >&2; fail=1; fi
fi
if [ "$WHAT" = "zink" ] || [ "$WHAT" = "both" ]; then
  check "virglrs build.rs (EGL_LIB_DIR/MESA_PREFIX)" "$MOUNT/zink-kk-prefix/lib/libEGL.dylib"
  gallium=("$MOUNT"/zink-kk-prefix/lib/libgallium-*-devel.dylib)
  # build-app.sh refuses to bundle when there is more than one: a stale second copy ships
  # silently otherwise.
  if [ "${#gallium[@]}" -eq 1 ] && [ -e "${gallium[0]}" ]; then
    echo "    ok   build-app.sh gallium: ${gallium[0]}"
  else
    echo "    MISS build-app.sh gallium: expected exactly ONE $MOUNT/zink-kk-prefix/lib/libgallium-*-devel.dylib, found: ${gallium[*]}" >&2; fail=1
  fi
fi

# The same tripwire scripts/build-app.sh applies before bundling -- run it here so a bad
# b_ndebug is caught at the build that caused it, not at packaging time days later.
if [ "$NDEBUG" = "true" ]; then
  while IFS= read -r f; do
    nm -u "$f" 2>/dev/null | grep -q "assert_rtn" || continue
    echo "    ASSERTS ON: $f still references __assert_rtn (a guest can abort the worker)" >&2
    fail=1
  done < <(find "$MOUNT/build-kk/src/kosmickrisp/vulkan" "$MOUNT/zink-kk-prefix/lib" \
             -name '*.dylib' -type f 2>/dev/null)
fi

[ "$fail" -eq 0 ] || { echo "==> host Mesa build INCOMPLETE (see MISS/ASSERTS above)" >&2; exit 1; }
echo "==> host Mesa ready — \`cargo xtask build\` and \`cargo xtask run\` can find it"
