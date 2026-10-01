# shellcheck shell=bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# The one shell reader for the dependency pins. Source it; it defines functions and nothing else.
#
# Two files:
#   third_party/manifest.toml        the committed pins: what every limina commit SAYS it builds.
#   third_party/manifest.local.toml  per-checkout overrides, never committed (the `third_party/*`
#                                    ignore covers it). Each worktree has its own.
#
# An override names a local tree; it never names a revision. Two keys per dependency:
#   source   = "<path>"   fetch the PINNED rev from this local repository instead of the network.
#                         What gets built is still exactly the pin — only where it comes from
#                         changes, so a pin can be committed before the fork is pushed.
#   checkout = "<path>"   use this working tree as the dependency, as it stands: its HEAD is what
#                         gets built, whatever the pin says. For iterating before a bump exists.
# Paths may start with `~/` or be relative to the limina checkout.
#
# `cargo xtask pins` reports what is in effect and whether every pin is pushed; the limina
# pre-push hook refuses to publish a commit whose pins no remote carries.
#
# xtask/src/manifest.rs is the Rust half and implements the same rules — keep the two in step.

_MANIFEST_ROOT="${LIMINA_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
# Resolved per call, so LIMINA_MANIFEST / LIMINA_MANIFEST_LOCAL can be pointed elsewhere (a test,
# or a manifest read out of another commit) after this file is sourced.
_manifest_file() { printf '%s\n' "${LIMINA_MANIFEST:-$_MANIFEST_ROOT/third_party/manifest.toml}"; }
_manifest_local_file() {
  printf '%s\n' "${LIMINA_MANIFEST_LOCAL:-$_MANIFEST_ROOT/third_party/manifest.local.toml}"
}

# _manifest_git <args...> — git, freed of the repository a calling hook binds it to: GIT_DIR and
# friends are exported to hooks, and `git -C <dep>` does not override them.
_manifest_git() {
  # shellcheck disable=SC2046
  (unset $(git rev-parse --local-env-vars); git "$@")
}

# _manifest_read <file> <section> <key> — one scalar from one table. Strings lose their quotes,
# bare values (booleans) their trailing comment. Enough TOML for these files, no more.
_manifest_read() {
  [ -f "$1" ] || return 0
  awk -v want="[$2]" -v key="$3" '
    /^[ \t]*\[/ {
      h = $0; sub(/[ \t]*#.*/, "", h); gsub(/[ \t]/, "", h)
      in_sec = (h == want); next
    }
    in_sec {
      line = $0
      if (match(line, "^[ \t]*" key "[ \t]*=")) {
        v = substr(line, RLENGTH + 1); sub(/^[ \t]+/, "", v)
        if (substr(v, 1, 1) == "\"") { v = substr(v, 2); v = substr(v, 1, index(v, "\"") - 1) }
        else { sub(/[ \t]*#.*/, "", v); sub(/[ \t]+$/, "", v) }
        print v; exit
      }
    }
  ' "$1"
}

# _manifest_path <path> — expand `~/` and anchor a relative path at the limina checkout.
_manifest_path() {
  case "$1" in
    "") ;;
    "~/"*) printf '%s\n' "$HOME/${1#\~/}" ;;
    /*) printf '%s\n' "$1" ;;
    *) printf '%s\n' "$_MANIFEST_ROOT/$1" ;;
  esac
}

# pin <dep> <key> — the COMMITTED value. Never affected by an override.
pin() { _manifest_read "$(_manifest_file)" "$1" "$2"; }

# pin_override <dep> <source|checkout> — the local override's path, expanded; empty when unset.
pin_override() {
  case "$2" in source|checkout) ;; *) echo "pin_override: unknown key '$2'" >&2; return 1 ;; esac
  _manifest_path "$(_manifest_read "$(_manifest_local_file)" "$1" "$2")"
}

# pin_tree <dep> — the local tree that IS this dependency on this checkout: a `checkout`
# override, else the manifest's `tree` (the mesa trees live on /Volumes/mesa-cs), else
# third_party/<dep>. A third_party dependency's override is materialized as a symlink there by
# `cargo xtask vendor`, so for those both answers are the same directory.
pin_tree() {
  local t
  t="$(pin_override "$1" checkout)"
  [ -n "$t" ] || t="$(_manifest_path "$(pin "$1" tree)")"
  [ -n "$t" ] || t="$_MANIFEST_ROOT/third_party/$1"
  printf '%s\n' "$t"
}

# pin_local_repo <dep> — a local repository to fetch this dependency from, or empty when it
# must come from the network: the `source` override, else the `checkout` override.
pin_local_repo() {
  local s
  s="$(pin_override "$1" source)"
  [ -n "$s" ] || s="$(pin_override "$1" checkout)"
  printf '%s\n' "$s"
}

# pin_build_rev <dep> — the revision a build of this dependency should use: the checkout
# override's HEAD when there is one (that is the point of it), else the committed pin.
pin_build_rev() {
  local c
  c="$(pin_override "$1" checkout)"
  if [ -n "$c" ]; then
    _manifest_git -C "$c" rev-parse HEAD
  else
    pin "$1" rev
  fi
}

# pin_git_dir <repo> — the absolute common git directory behind a working tree. What a container
# mounts: a linked worktree's own `.git` is a file naming a host path that means nothing inside.
pin_git_dir() {
  local d
  d="$(_manifest_git -C "$1" rev-parse --git-common-dir)" || return 1
  case "$d" in /*) ;; *) d="$(cd "$1/$d" && pwd)" ;; esac
  printf '%s\n' "$d"
}

# pin_note <dep> — say on stderr which override, if any, applies. Call it where a build starts,
# so a log always records that it did not build from the network at the pin.
pin_note() {
  local s c
  s="$(pin_override "$1" source)"; c="$(pin_override "$1" checkout)"
  if [ -n "$c" ]; then
    echo "==> [$1] LOCAL CHECKOUT OVERRIDE ($(_manifest_local_file)): building $c as it stands," \
         "HEAD $(_manifest_git -C "$c" rev-parse --short=12 HEAD 2>/dev/null || echo '?'), pin $(pin "$1" rev | cut -c1-12)" >&2
  elif [ -n "$s" ]; then
    echo "==> [$1] local source override: fetching the pin from $s" >&2
  fi
}
