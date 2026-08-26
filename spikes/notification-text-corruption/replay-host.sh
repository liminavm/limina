#!/usr/bin/env bash
# Run vrend-replay under the same host KosmicKrisp/zink environment the worker uses.
#
#   ./replay-host.sh <dump> [--ctx N] [--loops N] [--nodraw]
#
# ALWAYS run --nodraw first. It is the positive control: with the glyph draws dropped the readback
# must report TEXT LOST, or the oracle cannot express the outcome being looked for and a clean
# verdict below it means nothing.
set -eu
cd "$(dirname "$0")"
. ./kk-env.sh
[ -x ./vrend-replay ] || ./replay-build.sh
exec ./vrend-replay "$@"
