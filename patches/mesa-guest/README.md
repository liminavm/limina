# patches/mesa-guest — the guest venus Mesa series (DERIVED artifact, do not hand-edit)

This directory is the **exported form of the fork branch** `liminavm/mesa` **`limina-guest`**
(fork model, task #11, 2026-08-05). The branch is the source of truth: one commit per patch,
rationale in the commit message, based on the tag `third_party/manifest.toml [mesa-guest]`
records (`base`, currently `mesa-26.1.5` — the same Fedora SRPM base both RPM tracks build).
`scripts/export-mesa-guest-patches.sh` regenerates this series from `base..rev`; **never edit
the `.patch` files directly** — commit on the fork, push, bump the manifest rev, re-export,
and commit the regenerated series together with the manifest.

Why the export is *committed* (unlike the linux series, which derives into `target/`): the
consumers are the guest-mesa RPM builds — `scripts/provision/f44/build-mesa-rpm.sh` runs
inside the F44 build guest, `scripts/build-mesa-rpm.sh` inside the fc43 container — and
neither environment has the `/Volumes/mesa-cs` checkout. Both derive their spec `Patch9NNN`
lines from this directory's sorted listing, so a re-export with different filenames needs no
script edit.

The working checkout is a worktree at `/Volumes/mesa-cs/mesa-guest` (same case-sensitive
sparse image as the host `limina-kk` tree — Mesa can't check out on a case-insensitive FS;
`scripts/ensure-mesa-cs.sh` mounts it).

History: this replaced the `patches/mesa/` raw-diff pool (three bases, per-consumer subsets);
see that directory's tombstone README for what happened to each old number. Old→new map:
0015→0001, 0011→0002, 0012→0003, 0013→0004, 0016→0005, 0017→0006. Audit/upstream status:
`docs/upstreaming/ledger/mesa.md`.
