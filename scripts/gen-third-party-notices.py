#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

"""Write THIRD-PARTY-NOTICES.txt: every third-party component Limina.app ships, with its
licence text.

    scripts/gen-third-party-notices.py --out <file> [--check-bundle <Limina.app>]

Two sources, one file:
  - assets/third-party/components.toml, hand-maintained: the bundled libraries, gvproxy, the
    firmware and the Rust standard library, each with the licence file it ships under.
  - `cargo metadata`: every crate `limina` or `limina-vmm` links for aarch64-apple-darwin,
    following normal dependencies only (build and dev dependencies are not shipped, and
    neither is a proc-macro crate). Workspace members are ours and left out; the forks under
    third_party/ (libkrun, rutabaga, virglrs, imago) are third-party and stay in.

A crate's licence files are the LICENSE*/LICENCE*/COPYING*/NOTICE* in its package directory;
a path dependency with none there inherits its repository root's (libkrun's sub-crates keep
one LICENSE at the top). A crate with no file anywhere gets the standard text of its declared
licence with its authors as the copyright line, and is named on stderr so the approximation is
visible. Identical texts are printed once and referred to by number.

--check-bundle makes the run fail when a library in the bundle's Frameworks (or an executable
beside the two binaries) is claimed by no component: the bundler discovers the link closure
on its own, and this is what keeps a new dependency from shipping without its notice.

Run by scripts/build-app.sh; `cargo xtask notices` wraps it. Python 3.11+ (tomllib).
"""

import argparse
import fnmatch
import hashlib
import html
import json
import os
import re
import subprocess
import sys
import tomllib

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
COMPONENTS = os.path.join(REPO, "assets/third-party/components.toml")
ROOTS = ("limina", "limina-vmm")
TARGET = "aarch64-apple-darwin"
# Bundle files that are ours, so no component claims them.
FIRST_PARTY = {"limina", "limina-vmm", "liblimina_sep.dylib"}
LICENCE_PREFIXES = ("LICENSE", "LICENCE", "COPYING", "NOTICE", "COPYRIGHT")
# For a crate with no licence file: which option of an OR expression to reproduce, and where
# its standard text lives. MIT first — its text carries the copyright line the others lack.
FALLBACK_ORDER = ("MIT", "Apache-2.0", "Zlib", "BSD-3-Clause", "BSD-2-Clause", "ISC")


def rust_sysroot():
    return subprocess.run(
        ["rustc", "--print", "sysroot"], check=True, capture_output=True, text=True
    ).stdout.strip()


def spdx_text(ident):
    """The standard text of a licence, from the repo's LICENSES/ or the toolchain's copies."""
    for d in (
        os.path.join(REPO, "LICENSES"),
        os.path.join(rust_sysroot(), "share/doc/rust/licenses"),
    ):
        p = os.path.join(d, ident + ".txt")
        if os.path.exists(p):
            return read_text(p)
    return None


def read_text(path):
    with open(path, encoding="utf-8", errors="replace") as f:
        text = f.read()
    if path.endswith(".html"):
        # The Rust std notice ships only as HTML; the dialog shows plain text.
        text = re.sub(r"<(script|style)[^>]*>.*?</\1>", "", text, flags=re.S)
        text = re.sub(r"<(br|/p|/h\d|/li|/tr|/pre)[^>]*>", "\n", text)
        text = html.unescape(re.sub(r"<[^>]+>", "", text))
        text = re.sub(r"\n{3,}", "\n\n", text)
    return text.strip() + "\n"


class Texts:
    """Licence texts, deduplicated by content and numbered in order of first use."""

    def __init__(self):
        self.order = []
        self.index = {}

    def add(self, title, text):
        key = hashlib.sha256(text.encode()).hexdigest()
        if key not in self.index:
            self.order.append((title, text))
            self.index[key] = len(self.order)
        return self.index[key]


def component_version(c):
    if "keg" in c:
        return os.path.basename(os.path.realpath(c["keg"]))
    if "version_file" in c:
        with open(c["version_file"]) as f:
            return f.read().strip()
    if "pin" in c:
        with open(os.path.join(REPO, "third_party/manifest.toml"), "rb") as f:
            return "rev " + tomllib.load(f)[c["pin"]]["rev"][:12]
    if c.get("rustc"):
        out = subprocess.run(["rustc", "--version"], check=True, capture_output=True, text=True)
        return out.stdout.split()[1]
    return "unknown"


def load_components():
    with open(COMPONENTS, "rb") as f:
        return tomllib.load(f)["component"]


def shipped_crates():
    meta = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--filter-platform", TARGET, "--locked"],
            cwd=REPO,
            check=True,
            capture_output=True,
            text=True,
        ).stdout
    )
    packages = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    members = set(meta["workspace_members"])
    stack = [i for i in members if packages[i]["name"] in ROOTS]
    if len(stack) != len(ROOTS):
        sys.exit(f"gen-third-party-notices: expected workspace members {ROOTS}")
    seen = set()
    while stack:
        i = stack.pop()
        if i in seen:
            continue
        seen.add(i)
        for dep in nodes[i]["deps"]:
            if any(k["kind"] is None for k in dep["dep_kinds"]):
                stack.append(dep["pkg"])

    def proc_macro(p):
        return all("proc-macro" in t["kind"] for t in p["targets"])

    crates = [packages[i] for i in seen if i not in members and not proc_macro(packages[i])]
    return sorted(crates, key=lambda p: (p["name"], p["version"]))


def licence_files(directory):
    try:
        names = sorted(os.listdir(directory))
    except OSError:
        return []
    files = [
        os.path.join(directory, n)
        for n in names
        if n.upper().startswith(LICENCE_PREFIXES) and os.path.isfile(os.path.join(directory, n))
    ]
    # A REUSE-style tree (virglrs) keeps its texts in LICENSES/ and only a NOTICE at the top.
    reuse = os.path.join(directory, "LICENSES")
    if os.path.isdir(reuse):
        files += [os.path.join(reuse, n) for n in sorted(os.listdir(reuse)) if n.endswith(".txt")]
    return files


def crate_licence_files(p):
    directory = os.path.dirname(p["manifest_path"])
    files = licence_files(directory)
    if p.get("license_file"):
        named = os.path.join(directory, p["license_file"])
        if os.path.isfile(named) and named not in files:
            files.insert(0, named)
    if files or p["source"] is not None:
        return files
    # A path dependency: walk up to its repository root.
    d = directory
    while d != os.path.dirname(d) and d.startswith(os.path.join(REPO, "third_party")):
        d = os.path.dirname(d)
        files = licence_files(d)
        if files or os.path.exists(os.path.join(d, ".git")):
            return files
    return []


def fallback_text(p):
    expr = p["license"] or ""
    options = re.split(r"\s+OR\s+|/", expr.replace("(", "").replace(")", ""))
    options = [o.strip() for o in options if o.strip()]
    for ident in FALLBACK_ORDER:
        if ident in options:
            text = spdx_text(ident)
            if text is None:
                continue
            holders = ", ".join(p["authors"]) or f"the {p['name']} authors ({p['repository']})"
            text = text.replace("<year> <copyright holders>", holders)
            return ident, text
    return None, None


def check_bundle(app, components):
    claims = [g for c in components for g in c["claims"]]
    contents = os.path.join(app, "Contents")
    found = []
    for sub in ("Frameworks", "MacOS", "Resources"):
        d = os.path.join(contents, sub)
        if not os.path.isdir(d):
            continue
        for name in sorted(os.listdir(d)):
            path = os.path.join(d, name)
            shipped = (
                (sub == "Frameworks" and name.endswith(".dylib"))
                or (sub == "MacOS" and os.access(path, os.X_OK))
                or (sub == "Resources" and name.endswith(".fd"))
            )
            if shipped and name not in FIRST_PARTY:
                found.append(name)
    unclaimed = [n for n in found if not any(fnmatch.fnmatch(n, g) for g in claims)]
    return unclaimed


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--check-bundle")
    args = ap.parse_args()

    components = load_components()
    if args.check_bundle:
        unclaimed = check_bundle(args.check_bundle, components)
        if unclaimed:
            sys.exit(
                "gen-third-party-notices: no entry in assets/third-party/components.toml "
                "claims " + ", ".join(unclaimed)
            )

    texts = Texts()
    lines = [
        "Limina — third-party notices",
        "",
        "Limina is licensed GPL-2.0-only WITH LicenseRef-limina-exception. It includes the",
        "third-party software listed below, each under its own licence, whose text follows the",
        "lists. Where a component offers a choice of licences, the files it ships are reproduced",
        "as provided.",
        "",
        "gvproxy is a Go program; this file attributes it but not the Go modules compiled into it.",
        "",
        "=" * 78,
        "Bundled components",
        "=" * 78,
        "",
    ]
    sysroot = rust_sysroot()
    for c in components:
        nums = []
        for f in c["files"]:
            path = f.replace("{rust_sysroot}", sysroot)
            if not os.path.isabs(path):
                path = os.path.join(REPO, path)
            if not os.path.exists(path):
                sys.exit(f"gen-third-party-notices: {c['name']}: missing licence file {path}")
            nums.append(texts.add(c["name"], read_text(path)))
        lines.append(f"{c['name']} {component_version(c)}")
        lines.append(f"  {c['license']} — {c['url']}")
        lines.append(f"  licence text: {', '.join(f'[{n}]' for n in nums)}")
        lines.append("")

    lines += ["=" * 78, "Rust crates", "=" * 78, ""]
    approximated = []
    for p in shipped_crates():
        files = crate_licence_files(p)
        if files:
            nums = [texts.add(f"{p['name']} {p['version']}", read_text(f)) for f in files]
        else:
            ident, text = fallback_text(p)
            if text is None:
                sys.exit(
                    f"gen-third-party-notices: {p['name']} {p['version']} ships no licence file "
                    f"and its licence ({p['license']}) has no standard text here"
                )
            nums = [texts.add(f"{p['name']} {p['version']} ({ident})", text)]
            approximated.append(f"{p['name']} {p['version']} ({ident})")
        licence = p["license"] or "see licence text"
        url = p["repository"] or p["homepage"] or ""
        lines.append(f"{p['name']} {p['version']} — {licence}" + (f" — {url}" if url else ""))
        lines.append(f"  licence text: {', '.join(f'[{n}]' for n in nums)}")

    lines += ["", "=" * 78, "Licence texts", "=" * 78]
    for n, (title, text) in enumerate(texts.order, 1):
        lines += ["", "-" * 78, f"[{n}] first used by {title}", "-" * 78, "", text.rstrip()]

    with open(args.out, "w", encoding="utf-8") as f:
        f.write("\n".join(lines) + "\n")
    if approximated:
        print(
            "gen-third-party-notices: no licence file shipped; reproduced the standard text for: "
            + ", ".join(approximated),
            file=sys.stderr,
        )
    print(
        f"gen-third-party-notices: {args.out}: {len(components)} components, "
        f"{len(texts.order)} distinct licence texts",
        file=sys.stderr,
    )


if __name__ == "__main__":
    main()
