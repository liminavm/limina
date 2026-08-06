# patches/libkrun — RETIRED (fork model since 2026-08-06)

The libkrun patch series that lived here (130 patches at retirement) was **migrated to the
fork model** in task #14: `github.com/liminavm/libkrun`, branch **`limina`**, is the delta —
the whole series rebased onto upstream tip (upstream moved to `github.com/libkrun/libkrun`).
124 commits landed: three upstream-superseded patches were dropped (0038 blk serial,
0044 blob-map overflow guard, 0090 fs worker fix) and 0020 was squashed into 0017.

- The checkout is pinned by `[libkrun]` in `third_party/manifest.toml`; `cargo xtask vendor`
  clones the fork and checks out the pinned rev. There is nothing to apply.
- To change libkrun: commit on the fork's `limina` branch, push, bump the manifest rev.
  The branch is rewritten as patches merge upstream or get dropped — **tag before every
  rewrite** (every rev ever pinned must stay reachable).
- Upstreaming verdicts live in `docs/upstreaming/ledger/libkrun.md` (SUBJECT-keyed; ordinals
  refer to the retired series and remain the ledger's row keys).
