#!/usr/bin/env bash
# Find out who corrupts the heap when GStreamer's VA plugin probes our device.
#
# Symptom: gst-inspect-1.0 on libgstva.so finds /dev/dri/renderD128 and then dies with
# "malloc(): unaligned tcache chunk detected", registering no elements -- so every
# GStreamer pipeline in the guest silently falls back to avdec while Firefox, on the
# same device through libva, decodes in hardware.
#
# The question a backtrace answers, and nothing else does: whether gst-va walks off its
# own allocation, or whether our VA driver answers vaQueryConfigProfiles /
# vaQueryConfigEntrypoints with something malformed that gst-va then trusts. Frames
# inside libgstva mean theirs; frames reaching our driver's reply mean ours.
#
# Copy into the guest and run from an ssh session. Mutates the guest (installs gdb and
# debuginfo), so run it on a throwaway clone, never on a dogfood guest.
set -euo pipefail

log="${1:-/tmp/gstva-probe.log}"
plugin=/usr/lib64/gstreamer-1.0/libgstva.so

sudo dnf install -y gdb >/dev/null
# The two that matter: the plugin doing the probing and the driver answering it.
sudo dnf debuginfo-install -y gstreamer1-plugins-bad-free mesa-dri-drivers >/dev/null 2>&1 || \
  echo "note: debuginfo-install fell short; the trace may be frames-only" >&2

{
  echo "=== plugin: $plugin"
  ls -l "$plugin"
  echo "=== abort backtrace ==="
  # MALLOC_CHECK_=3 makes glibc abort at the first inconsistency rather than wherever it
  # happens to notice, so the stack is nearer the corruption than the detection.
  MALLOC_CHECK_=3 GST_DEBUG=va:5 gdb -batch \
    -ex 'set pagination off' \
    -ex 'run' \
    -ex 'bt full' \
    -ex 'info sharedlibrary libgstva' \
    --args gst-inspect-1.0 "$plugin" 2>&1
} | tee "$log"

echo
echo "wrote $log"
