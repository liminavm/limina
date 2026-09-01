#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Refuse to publish a bundle whose fork-model source trees differ from the revisions recorded in
# third_party/manifest.toml. Build directories and other ignored outputs are allowed; tracked
# source edits and a checkout at the wrong commit are not. The output doubles as the provenance
# note uploaded next to a development DMG.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"
MANIFEST="$ROOT/third_party/manifest.toml"

manifest_rev() {
  local section="$1"
  awk -v wanted="$section" '
    $0 == "[" wanted "]" { in_section = 1; next }
    in_section && /^\[/ { exit }
    in_section && $1 == "rev" {
      gsub(/"/, "", $3)
      print $3
      exit
    }
  ' "$MANIFEST"
}

check_pinned_tree() {
  local section="$1" path="$2" expected actual dirty
  expected="$(manifest_rev "$section")"
  [ -n "$expected" ] || { echo "manifest has no rev for [$section]" >&2; exit 1; }
  git -C "$path" rev-parse --git-dir >/dev/null 2>&1 || {
    echo "missing release input checkout: $path" >&2
    exit 1
  }
  actual="$(git -C "$path" rev-parse HEAD)"
  if [ "$actual" != "$expected" ]; then
    echo "$section checkout is $actual, expected $expected" >&2
    exit 1
  fi
  dirty="$(git -C "$path" status --porcelain --untracked-files=normal)"
  if [ -n "$dirty" ]; then
    echo "$section checkout has source changes: $path" >&2
    printf '%s\n' "$dirty" >&2
    exit 1
  fi
  printf '%-16s %s\n' "$section" "$actual"
}

[ -z "$(git status --porcelain --untracked-files=normal)" ] || {
  echo "limina worktree is dirty" >&2
  git status --short >&2
  exit 1
}

. "$ROOT/scripts/ensure-mesa-cs.sh"

printf '%-16s %s\n' limina "$(git rev-parse HEAD)"
check_pinned_tree libkrun "$ROOT/third_party/libkrun"
check_pinned_tree imago "$ROOT/third_party/imago"
check_pinned_tree virglrenderer "$ROOT/third_party/virglrenderer"
check_pinned_tree kosmickrisp /Volumes/mesa-cs/mesa

# libepoxy predates the fork manifest. Record its exact source commit so a development artifact is
# still attributable; pinning it in manifest.toml remains a prerequisite for a stable channel.
EPOXY="$ROOT/third_party/libepoxy"
git -C "$EPOXY" rev-parse --git-dir >/dev/null 2>&1 || {
  echo "missing release input checkout: $EPOXY" >&2
  exit 1
}
epoxy_dirty="$(git -C "$EPOXY" status --porcelain --untracked-files=normal)"
if [ -n "$epoxy_dirty" ]; then
  echo "libepoxy checkout has source changes: $EPOXY" >&2
  printf '%s\n' "$epoxy_dirty" >&2
  exit 1
fi
printf '%-16s %s (recorded; not yet manifest-pinned)\n' libepoxy "$(git -C "$EPOXY" rev-parse HEAD)"
