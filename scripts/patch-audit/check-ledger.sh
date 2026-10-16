#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Does each upstreaming ledger have a row for every commit on its fork's pinned branch?
#
#   scripts/patch-audit/check-ledger.sh [--skeleton] [<ledger-name>...]
#
# Each ledger file under docs/upstreaming/ledger/ that tracks a fork opens with one marker line:
#
#   <!-- ledger-check: dep=<manifest section> [manifest=<path>] [base=<rev>] [paths=<p>,<p>] -->
#
# `dep` names the pin; `manifest` reads it from another manifest (virglrenderer's pin lives in
# virglrs's); `base` is used only when that manifest records none; `paths` limits the range to
# commits touching those paths (virglrenderer's branch also carries the Rust rewrite, which is
# not an upstreaming delta). The range is `base..rev`, read in the dependency's local tree
# (`pin_tree`, so `third_party/manifest.local.toml` can point it at a clone elsewhere).
#
# A commit counts as tracked when its subject appears verbatim anywhere in the ledger file. Rows
# are keyed by subject (README.md), so rewritten SHAs after a rebase do not show up as drift.
# Prints every untracked commit and exits 1 if there is one; 2 if a range cannot be read (a
# shallow clone missing the base, a tree that is not there). `--skeleton` prints the untracked
# commits as ledger rows instead, ready to paste under the file's unaudited section.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
LEDGER="$ROOT/docs/upstreaming/ledger"
# shellcheck source=../lib/manifest.sh
. "$ROOT/scripts/lib/manifest.sh"

skeleton=0
names=()
for a in "$@"; do
    case "$a" in
        --skeleton) skeleton=1 ;;
        -h|--help) sed -n '5,22p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) names+=("${a%.md}") ;;
    esac
done
if [ "${#names[@]}" -eq 0 ]; then
    for f in "$LEDGER"/*.md; do
        grep -q '^<!-- ledger-check:' "$f" && names+=("$(basename "$f" .md)")
    done
fi

marker_field() { # <marker line> <key>
    sed -n "s/.*[[:space:]]$2=\([^[:space:]]*\).*/\1/p" <<<"$1"
}

status=0
for name in "${names[@]}"; do
    file="$LEDGER/$name.md"
    marker="$(grep -m1 '^<!-- ledger-check:' "$file" || true)"
    if [ -z "$marker" ]; then
        echo "$name: no ledger-check marker in $file" >&2; status=2; continue
    fi
    dep="$(marker_field "$marker" dep)"
    alt="$(marker_field "$marker" manifest)"
    paths="$(marker_field "$marker" paths | tr ',' ' ')"
    if [ -n "$alt" ]; then
        rev="$(LIMINA_MANIFEST="$ROOT/$alt" pin "$dep" rev)"
        base="$(LIMINA_MANIFEST="$ROOT/$alt" pin "$dep" base)"
        tree="$(dirname "$(dirname "$ROOT/$alt")")/third_party/$dep"
    else
        rev="$(pin "$dep" rev)"
        base="$(pin "$dep" base)"
        tree="$(pin_tree "$dep")"
    fi
    [ -n "$base" ] || base="$(marker_field "$marker" base)"
    if [ -z "$rev" ] || [ -z "$base" ]; then
        echo "$name: no rev or base for [$dep]" >&2; status=2; continue
    fi
    if ! _manifest_git -C "$tree" rev-parse -q --verify "$rev^{commit}" >/dev/null 2>&1 ||
       ! _manifest_git -C "$tree" rev-parse -q --verify "$base^{commit}" >/dev/null 2>&1; then
        echo "$name: $tree lacks $base..$rev (not vendored, or a shallow clone; fetch deeper)" >&2
        status=2; continue
    fi
    # shellcheck disable=SC2086
    untracked="$(_manifest_git -C "$tree" log --reverse --format='%h%x09%s' "$base..$rev" \
        ${paths:+-- $paths} | while IFS=$'\t' read -r sha subject; do
            grep -qF -- "$subject" "$file" || printf '%s\t%s\n' "$sha" "$subject"
        done)"
    total="$(_manifest_git -C "$tree" rev-list --count "$base..$rev" ${paths:+-- $paths})"
    if [ -z "$untracked" ]; then
        [ "$skeleton" -eq 1 ] || echo "$name: all $total commits in ${base:0:12}..${rev:0:12} have a row"
        continue
    fi
    n="$(wc -l <<<"$untracked" | tr -d ' ')"
    [ "$status" -eq 2 ] || status=1
    if [ "$skeleton" -eq 1 ]; then
        echo "<!-- $name: $n untracked of $total in ${base:0:12}..${rev:0:12} -->"
        echo '| commit | subject | files | diag | first guess (unresearched) |'
        echo '|---|---|---|---|---|'
        while IFS=$'\t' read -r sha subject; do
            files="$(_manifest_git -C "$tree" show --format= --name-only "$sha" |
                awk 'NR<=3 {printf "%s`%s`", (NR>1 ? ", " : ""), $0} END {if (NR>3) printf " +%d", NR-3}')"
            diag=""
            _manifest_git -C "$tree" show --format= "$sha" | grep -q '^+.*/tmp/limina' && diag="DIAG"
            printf '| `%s` | %s | %s | %s | |\n' "$sha" "${subject//|/\\|}" "$files" "$diag"
        done <<<"$untracked"
    else
        echo "$name: $n of $total commits in ${base:0:12}..${rev:0:12} have no row:"
        sed 's/^/  /' <<<"$untracked"
    fi
done
exit "$status"
