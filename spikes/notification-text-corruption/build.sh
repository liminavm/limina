#!/usr/bin/env bash
# Build the probe binaries. They are Swift sources in-tree; the binaries are not committed.
set -eu
cd "$(dirname "$0")"
for t in bannerprobe iosscan nudge winbounds; do
    [ "$t.swift" -nt "$t" ] 2>/dev/null || [ ! -x "$t" ] && swiftc -O "$t.swift" -o "$t"
done
# iosdump lives with the venus probes; it is the generic global-IOSurface dumper.
[ -x iosdump ] || swiftc -O ../venus-draw-probe/iosdump.swift -o iosdump
echo "built: bannerprobe iosscan nudge winbounds iosdump"
