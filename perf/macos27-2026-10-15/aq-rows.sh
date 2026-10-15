#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Append a point's aquarium rows, read by a human off evidence/<label>/aquarium-r*/*-fps.png.
# Usage: perf/macos27-2026-10-15/aq-rows.sh <label> <venus|metal> <r1-25k> <r1-30k> <r2-25k> <r2-30k>
set -euo pipefail
cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
D=perf/macos27-2026-10-15
LABEL=$1; ARM=$2; NOTE=$(head -n 1 "$D/evidence/$LABEL/provenance.txt")
DATE=$(date +%Y-%m-%d); C=$(git rev-parse --short HEAD)
echo "$DATE,$C,aquarium-25k-$ARM,fps,$3,\"$NOTE; run 1/2; read off the fps crop\"" >> $D/ledger.csv
echo "$DATE,$C,aquarium-30k-$ARM,fps,$4,\"$NOTE; run 1/2; read off the fps crop\"" >> $D/ledger.csv
echo "$DATE,$C,aquarium-25k-$ARM,fps,$5,\"$NOTE; run 2/2; read off the fps crop\"" >> $D/ledger.csv
echo "$DATE,$C,aquarium-30k-$ARM,fps,$6,\"$NOTE; run 2/2; read off the fps crop\"" >> $D/ledger.csv
tail -n 4 $D/ledger.csv | cut -d, -f3-5
