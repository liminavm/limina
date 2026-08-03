#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva

"""Phase 0 of the patch audit: build ledger skeletons from the patch series.

Parses patches/<series>/*.patch (and mesa's *.diff) into one markdown ledger per
series under docs/upstreaming/ledger/, one row per patch keyed by Subject line.
Mechanical only: subject, files touched, DIAG detection. Judgment columns are left
empty for phases 1-3 (see docs/upstreaming/ledger/README.md).

Re-runnable: if a ledger already exists, rows whose subject already appears are
kept verbatim (filled columns survive); new patches get fresh skeleton rows and
vanished subjects are reported, never silently dropped.
"""

import re
import sys
from email.header import decode_header, make_header
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
PATCHES = REPO / "patches"
LEDGER = REPO / "docs" / "upstreaming" / "ledger"

SERIES = {
    "libkrun": "*.patch",
    "virglrenderer": "*.patch",
    "kosmickrisp": "*.patch",
    "mesa": "*.diff",
    "linux": "*.patch",
    "imago": "*.patch",
    # mutter: retired 2026-08-03 — limina no longer patches mutter (writing a
    # drop-in gnome-shell/mutter replacement). ledger/mutter.md kept as history.
}

SKELETON = dict(need="", checked="", issue="", mr="", sec="", fold="", tier="", disp="", notes="")


def parse_patch(path: Path):
    subject = None
    raw_subject = None
    files = []
    diag = False
    in_headers = True
    in_subject = False
    for line in path.read_text(errors="replace").splitlines():
        if in_headers and line.startswith("Subject:"):
            raw_subject = line[len("Subject:"):].strip()
            in_subject = True
            continue
        if in_subject:
            # format-patch wraps long Subjects onto indented continuation lines;
            # the key must be the FULL subject or re-wrapping re-keys the row
            if line.startswith((" ", "\t")):
                raw_subject += " " + line.strip()
                continue
            in_subject = False
        if line.startswith("+++ b/"):
            files.append(line[6:].split("\t")[0].strip())
            in_headers = False
        if "/tmp/limina" in line:
            diag = True
    if raw_subject is not None:
        if "=?" in raw_subject:  # RFC 2047 — decode, subjects are row keys
            raw_subject = str(make_header(decode_header(raw_subject)))
        subject = re.sub(r"^(\[PATCH[^\]]*\]\s*)?", "", raw_subject).strip()
    if subject is None:
        # mesa MR backports may lack a Subject header; derive from the filename
        subject = re.sub(r"^\d+-", "", path.stem).replace("-", " ")
    ordinal = path.name.split("-", 1)[0]
    return ordinal, subject, files, diag


def files_cell(files):
    if not files:
        return ""
    shown = ", ".join(f"`{f}`" for f in files[:3])
    extra = len(files) - 3
    return shown + (f" +{extra}" if extra > 0 else "")


def row(ordinal, subject, files, diag, filled):
    cells = [ordinal, subject, files_cell(files), "DIAG" if diag else ""]
    cells += [filled[k] for k in ("need", "checked", "issue", "mr", "sec", "fold", "tier", "disp", "notes")]
    return "| " + " | ".join(c.replace("|", "\\|") for c in cells) + " |"


def existing_tail(ledger_path: Path):
    """Everything from '## Findings' on — hand-written prose survives re-runs."""
    if not ledger_path.exists():
        return None
    text = ledger_path.read_text()
    idx = text.find("## Findings")
    return text[idx:].rstrip("\n") if idx != -1 else None


def existing_rows(ledger_path: Path):
    """subject -> dict of filled judgment columns, from a previous run."""
    if not ledger_path.exists():
        return {}
    rows = {}
    for line in ledger_path.read_text().splitlines():
        if not line.startswith("|") or line.startswith("| ord") or set(line) <= {"|", "-", " "}:
            continue
        cells = [c.strip() for c in line.strip("|").split("|")]
        if len(cells) != 13:
            continue
        subject = cells[1].replace("\\|", "|")
        keys = ("need", "checked", "issue", "mr", "sec", "fold", "tier", "disp", "notes")
        rows[subject] = dict(zip(keys, (c.replace("\\|", "|") for c in cells[4:])))
    return rows


HEADER = (
    "| ord | subject | files | diag | need | checked | issue | mr | sec | fold | tier | disp | notes |\n"
    "|---|---|---|---|---|---|---|---|---|---|---|---|---|"
)


def main():
    LEDGER.mkdir(parents=True, exist_ok=True)
    for series, glob in SERIES.items():
        src = PATCHES / series
        if not src.is_dir():
            print(f"skip {series}: no such dir", file=sys.stderr)
            continue
        patches = sorted(src.glob(glob))
        ledger_path = LEDGER / f"{series}.md"
        kept = existing_rows(ledger_path)
        tail = existing_tail(ledger_path)
        base_file = src / "UPSTREAM_BASE"
        base = base_file.read_text().strip()[:12] if base_file.exists() else "floating — see the series README"

        lines = [f"# {series} — patch-audit ledger", ""]
        lines.append(f"{len(patches)} patches; `UPSTREAM_BASE` `{base}`. Schema + protocol: `README.md`.")
        lines.append("Rows are keyed by SUBJECT; ordinals are informational and drift on re-export.")
        lines.append("")
        lines.append(HEADER)

        seen = set()
        for p in patches:
            ordinal, subject, files, diag = parse_patch(p)
            seen.add(subject)
            filled = kept.get(subject, SKELETON)
            lines.append(row(ordinal, subject, files, diag, filled))

        vanished = [s for s in kept if s not in seen]
        if vanished:
            lines += ["", "## Vanished since last run (folded or dropped — carry their columns forward by hand)", ""]
            lines += [f"- {s}" for s in vanished]
        lines += ["", tail if tail else "## Findings", ""]

        ledger_path.write_text("\n".join(lines) + "\n")
        n_diag = sum(1 for p in patches if parse_patch(p)[3])
        print(f"{series}: {len(patches)} rows ({len(kept)} carried, {len(vanished)} vanished, {n_diag} DIAG)")


if __name__ == "__main__":
    main()
