#!/bin/bash
# The host half: out/ gets Apple's metal-cpp (fetched and checked against its sha256), the test's
# kernels as a .metallib (Xcode's Metal toolchain: `xcodebuild -downloadComponent MetalToolchain`),
# limina-metal as a static library for aarch64-unknown-linux-gnu (`rustup target add
# aarch64-unknown-linux-gnu`), the stand-in headers and the test's source. run.sh builds the
# program in the guest, against those.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
out=$here/out
mkdir -p "$out/include"
url=https://developer.apple.com/metal/cpp/files/metal-cpp_26.zip
sum=4df3c078b9aadcb516212e9cb03004cbc5ce9a3e9c068fa3144d021db585a3a4
if [ ! -d "$out/metal-cpp" ]; then
  curl -sSfL -o "$out/metal-cpp.zip" "$url"
  echo "$sum  $out/metal-cpp.zip" | shasum -a 256 -c -
  unzip -q -o "$out/metal-cpp.zip" -d "$out"
fi
xcrun -sdk macosx metal -o "$out/kernels.metallib" "$here/test/kernels.metal"
cp "$here/test/kernels.metal" "$here/test/axpy.cpp" "$out/"
cp -R "$here/include/." "$out/include/"
# From inside shim/: cargo takes .cargo/config.toml (the target) from the directory it runs in.
(cd "$here/shim" && cargo build --release)
cp "$here/shim/target/aarch64-unknown-linux-gnu/release/liblimina_metal.a" "$out/"
