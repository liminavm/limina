#!/bin/bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Build the IOKit/VM interposer and boot the enhanced tier with it inserted into the worker.
#
# The worker carries com.apple.security.cs.allow-dyld-environment-variables AND
# com.apple.security.cs.disable-library-validation, so an ad-hoc-signed dylib inserts cleanly.
#
#   LIMINA_DISK=Fedora-Workstation-44.ioclass.raw \
#     spikes/vrend-region-leak/iokit-trace/run-traced.sh
#
# Then drive a workload (spikes/vrend-region-leak/memcycle.sh) and read the [IOTRACE] dumps out
# of the worker log. The dump prints the top stacks by map count; the leaking caller is meant to
# be stack #1.
set -eu
ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
cd "$ROOT"
DYLIB=/tmp/libiokittrace.dylib
clang -dynamiclib -O1 -g -o "$DYLIB" spikes/vrend-region-leak/iokit-trace/iokittrace.c \
  -framework IOKit -framework CoreFoundation
codesign -s - -f "$DYLIB" >/dev/null 2>&1
echo "built $DYLIB"

# Hand the path over under a NON-DYLD name. /bin/bash is SIP-protected, so dyld strips DYLD_*
# every time a bash script starts — exporting DYLD_INSERT_LIBRARIES here and exec'ing the boot
# script (another #!/bin/bash) loses it silently, and an interposer that never loads produces an
# empty trace indistinguishable from "nothing allocated". boot-enhanced-efi-kk.sh renames it in
# the process that actually execs the worker.
export LIMINA_IOTRACE_DYLIB="$DYLIB"
export LIMINA_IOTRACE_DUMP="${LIMINA_IOTRACE_DUMP:-30}"
export LIMINA_IOTRACE_DEPTH="${LIMINA_IOTRACE_DEPTH:-16}"
exec spikes/venus-draw-probe/boot-enhanced-efi-kk.sh
