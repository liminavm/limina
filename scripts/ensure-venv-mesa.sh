#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

# Ensure third_party/venv-mesa exists and carries the Python modules the code generators need.
#
# Two unrelated consumers reach for the same two modules through a bare `python3`, and both
# used to fail on any machine that merely happened not to have them in its system Python:
#
#   - virglrs's build.rs runs venus-gen (needs mako) and vrend-gen (needs yaml), so
#     `cargo build` itself does not complete without them -- the traceback comes out of a
#     build script, which reads like a broken dependency rather than a missing prerequisite;
#   - the host Mesa builds run Mesa's own mako codegen, and meson resolves `python3` to this
#     venv once it is on PATH, so Mesa's own Python prerequisites belong here too
#     (scripts/build-host-mesa.sh).
#
# Keeping them in a repo venv rather than the system Python means a fresh clone is
# self-sufficient and no host Python is mutated (Homebrew's is externally-managed anyway).
#
# Source it (`. scripts/ensure-venv-mesa.sh`) to also get the venv on PATH -- which is what
# the bare `python3` in those generators resolves through -- or run it standalone just to
# materialize it. Idempotent either way.
set -euo pipefail

VENV_MESA="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/third_party/venv-mesa"

# Module name (what the build imports) paired with the distribution that provides it.
# mako:      Mesa's and venus-protocol's template engine.
# yaml:      gallium's u_format.yaml parser.
# packaging: Mesa's meson.build needs packaging or distutils, and distutils was removed in
#            Python 3.12 -- so on a modern interpreter this one is not optional. It matters
#            here and not only in Mesa's own docs because this venv goes on PATH *ahead of*
#            the system Python, so whatever Mesa needs has to be in it, not merely on the host.
_venv_mesa_modules=(mako yaml packaging)
_venv_mesa_packages=(mako pyyaml packaging)

if [ ! -x "$VENV_MESA/bin/python3" ]; then
  echo "==> creating $VENV_MESA"
  python3 -m venv "$VENV_MESA"
fi

# Check the modules, not just the directory: a venv from an older prerequisite set is the
# case that otherwise reappears as the same build-script traceback.
_venv_mesa_missing=()
for _i in "${!_venv_mesa_modules[@]}"; do
  "$VENV_MESA/bin/python3" -c "import ${_venv_mesa_modules[$_i]}" 2>/dev/null \
    || _venv_mesa_missing+=("${_venv_mesa_packages[$_i]}")
done
if [ "${#_venv_mesa_missing[@]}" -gt 0 ]; then
  echo "==> installing into venv-mesa: ${_venv_mesa_missing[*]}"
  "$VENV_MESA/bin/pip" install --quiet --disable-pip-version-check "${_venv_mesa_missing[@]}"
fi
unset _venv_mesa_modules _venv_mesa_packages _venv_mesa_missing _i

export PATH="$VENV_MESA/bin:$PATH"
