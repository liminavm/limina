# patches/mesa-guest — the guest venus Mesa series (DERIVED artifact, do not hand-edit)

This directory is the **exported form of the fork branch** `liminavm/mesa` **`limina-guest`**
(fork model, task #11, 2026-08-05). The branch is the source of truth: one commit per patch,
rationale in the commit message, based on the tag `third_party/manifest.toml [mesa-guest]`
records (`base`, currently `mesa-26.1.7` — the same Fedora SRPM base both RPM tracks build).
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

**Retired at the 26.1.7 rebase (2026-08-15):** the old `0006-venus-track-vn_ring_submit-capacity`
is gone — 26.1 stable picked up the upstream fix, so numbers above it shifted down by one
(old 0007→0006, 0008→0007, 0009→0008). Note *how* that was established, because the obvious
test lies: the commit message said "drop when the base contains `09fb7ca8d82`", and
`git merge-base --is-ancestor 09fb7ca8 mesa-26.1.7` still says **no**. Stable branches
cherry-pick, so SHA ancestry is the wrong instrument. Reading `vn_ring_get_submit` at the tag is
the right one — it now allocates at `MAX2(count, VN_MIN_SHMEM_COUNT)` and only recycles nodes for
requests `<= VN_MIN_SHMEM_COUNT`, so every free-list node can serve any request it is offered:
the same bug, fixed a different way. **Check the code, not the SHA**, before retiring a patch on
a stable base.

History: this replaced the `patches/mesa/` raw-diff pool (three bases, per-consumer subsets);
see that directory's tombstone README for what happened to each old number. Old→new map:
0015→0001, 0011→0002, 0012→0003, 0013→0004, 0016→0005, 0017→0006 *(0017/0006 since retired,
above)*. Audit/upstream status:
`docs/upstreaming/ledger/mesa.md`.
