# patches/mesa — RETIRED pool (2026-08-05, task #11)

**This directory is no longer a build input.** The shipping guest-mesa patches moved to the
fork model: commits on `liminavm/mesa`'s **`limina-guest`** branch (pinned by
`third_party/manifest.toml [mesa-guest]`), exported by `scripts/export-mesa-guest-patches.sh`
into **`patches/mesa-guest/`** — that committed series is what both RPM tracks
(`scripts/provision/f44/build-mesa-rpm.sh`, `scripts/build-mesa-rpm.sh`) now apply. The six
migrated diffs were deleted from here (old→new: 0015→0001, 0011→0002, 0012→0003, 0013→0004,
0016→0005, 0017→0006); this file's pre-retirement revision holds the full pool history
(bases×consumers map, 0009-vs-0015, the dev-enh forensics, drop-guest-zink) — `git log` it.

What remains here is **reference material with no live consumer**, kept because it is
upstream-send-queue collateral (`docs/upstreaming/ledger/mesa.md` is authoritative on each):

- `0001` zink nullDescriptor emulation (MR !37115 exists upstream) — dead in guest
  (drop-guest-zink 2026-08-04); host zink rides the `limina-kk` fork instead.
- `0002` GL-frontend discard NULL-guard — MR-worthy (reachable under virgl too), but not
  currently shipped by any build.
- `0003`/`0004`/`0006` zink external-semaphore/kopper guards — dead in guest; MR-worthy as
  "gate optional entrypoints".
- `0009`/`0010` the recovered dev-enh venus present fix (pre-26.1.4 bases) — historical;
  consumed only by the archived `scripts/archive/build-venus.sh` dev vehicle (archived with
  this retirement). The living variant is `limina-guest` commit 0001.
- `0014` zink unflushed-batch lost-wakeup fix — dead in guest; lives on in the HOST build as
  `limina-kk` fork commit `47308c0f026`; both bugs remain upstream-MR queue items.
- `reference/` — upstream MR snapshots and related collateral.

Do not add new patches here. Guest mesa changes go on the fork branch (worktree
`/Volumes/mesa-cs/mesa-guest`), per `patches/mesa-guest/README.md`.
