#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
# DKMS's build step for limina-virtio-gpu: build the sources of the kernel's series into out/.
# The in-tree driver's internals move between series, so each one gets its own vendored copy;
# a kernel with none fails here and keeps its in-tree driver.
#
# Usage (from dkms.conf): sh ./dkms-make.sh <kernelver> <kernel_source_dir>
set -eu
kver="$1"
ksrc="$2"
series="$(echo "$kver" | sed -E 's/^([0-9]+\.[0-9]+).*/\1/')"
if [ ! -d "series/$series" ]; then
  echo "limina-virtio-gpu: no sources for kernel series $series (have: $(ls series | tr '\n' ' '))" >&2
  exit 1
fi
rm -rf out
cp -r "series/$series" out
make -C "$ksrc" M="$PWD/out" modules
