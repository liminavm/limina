#!/bin/bash
# Bisect vehicle for the kk_draw SIGSEGV that limina-test::venus_replay catches.
#
# The crash is a host-side worker SIGSEGV in kk_draw/kk_heap, reached through
# vrend_renderer_create_fence -> _mesa_fence_sync -> tc_flush -> _tc_sync. It is
# clean at the manifest-pinned mesa rev 808f0497 and reproduces at the limina-kk
# tip, so it was introduced by the notification-text instrumentation series.
#
# Use from the mesa checkout:
#   git bisect start <bad> <good>
#   git bisect run <limina>/spikes/notification-text-corruption/bisect-venus-replay.sh
#
# Exit 0 = good, 1 = bad, 125 = skip (build broke, rev not testable).
#
# Both build dirs must be rebuilt: build-kk supplies the KosmicKrisp ICD the test
# harness loads (crates/limina-test/src/lib.rs), build-zink-kk supplies zink and
# gallium. Rebuilding only one leaves a version-skewed stack.
set -u
export PATH=/opt/homebrew/opt/llvm/bin:/opt/homebrew/opt/bison/bin:$PATH

MESA="${MESA:-/Volumes/mesa-cs/mesa}"
LIMINA=$(cd "$(dirname "$0")" && git rev-parse --show-toplevel)
rev=$(cd "$MESA" && git rev-parse --short HEAD)
echo "=== bisect: $rev"

for d in build-kk build-zink-kk; do
  if ! ninja -C "/Volumes/mesa-cs/$d" > "/tmp/bisect-ninja-$d.log" 2>&1; then
    echo "    build failed in $d -- skipping rev"
    exit 125
  fi
done

cd "$LIMINA" || exit 125
LIMINA_HVF_TESTS=1 cargo nextest run -p limina-test --test venus_replay \
  venus_replay_matches_llvmpipe_reference > "/tmp/bisect-test-$rev.log" 2>&1
rc=$?
if [ $rc -eq 0 ]; then echo "    GOOD"; else echo "    BAD (rc=$rc)"; fi
exit $rc
