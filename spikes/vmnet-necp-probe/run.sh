#!/bin/bash
# Build the probe, sign one copy per signature, and dial a vmnet guest's IP:PORT with each, both
# straight from this shell and as a launchd job (how limina's worker is started).
#   run.sh IP [PORT]          LIMINA_SIGN_IDENTITY overrides the Apple Development identity
set -u
here="$(cd "$(dirname "$0")" && pwd)"
ip="$1"
port="${2:-22}"
id="${LIMINA_SIGN_IDENTITY:-$(security find-identity -v -p codesigning \
      | sed -n 's/.*\([0-9A-F]\{40\}\) "Apple Development.*/\1/p' | head -1)}"
mkdir -p "$here/build"
xcrun clang -Wall -O2 -o "$here/build/probe" "$here/probe.c" || exit 1
for v in adhoc identity identity-hardened; do
    cp "$here/build/probe" "$here/build/probe-$v"
done
codesign -s - --force "$here/build/probe-adhoc"
codesign -s "$id" --force "$here/build/probe-identity"
codesign -s "$id" --force -o runtime "$here/build/probe-identity-hardened"
for v in adhoc identity identity-hardened; do
    bin="$here/build/probe-$v"
    who="$(codesign -dvv "$bin" 2>&1 | sed -n 's/^Authority=//p' | head -1)"
    echo "== $v (${who:-ad-hoc})"
    echo -n "  shell:   "
    "$bin" "$ip" "$port"
    out="$here/build/launchd-$v.out"
    : > "$out"
    label="dev.limina.necp-probe.$v.$$"
    launchctl submit -l "$label" -o "$out" -e "$out" -- "$bin" "$ip" "$port"
    for _ in $(seq 1 30); do
        [ -s "$out" ] && break
        sleep 0.5
    done
    launchctl remove "$label" 2>/dev/null
    echo -n "  launchd: "
    if [ -s "$out" ]; then cat "$out"; else echo "(no output)"; fi
done
