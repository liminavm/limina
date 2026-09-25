#!/bin/bash
# Wait for a fresh target/release/limina-vmm worker to appear, then sample its restore.
D="$1"; OLD=$(pgrep -f "^target/release/limina-vmm")
until W=$(pgrep -f "^target/release/limina-vmm") && [ -n "$W" ] && [ "$W" != "$OLD" ]; do sleep 0.02; done
echo "worker $W"; sample "$W" 6 1 -mayDie -file "$D/resume-sample.txt" >/dev/null 2>&1; echo done
