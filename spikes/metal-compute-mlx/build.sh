#!/bin/bash
# Builds the probe's two halves into out/: axpy.metallib with Xcode's Metal toolchain
# (`xcodebuild -downloadComponent MetalToolchain` installs it), and the guest program, a static
# aarch64 musl binary.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$here/out"
xcrun -sdk macosx metal -o "$here/out/axpy.metallib" "$here/axpy.metal"
cp "$here/axpy.metal" "$here/out/"
# From inside probe/: cargo takes .cargo/config.toml (the target and linker) from the directory it runs in.
(cd "$here/probe" && cargo build --release)
cp "$here/probe/target/aarch64-unknown-linux-musl/release/metal-compute-probe" "$here/out/"
