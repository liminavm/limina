# Fork migration — the modus operandi

How a dependency moves from a committed `patches/<dep>/` series to a real fork under
**github.com/liminavm**, with our work on a `limina` branch. Written down after the imago pilot
(2026-08-03) and the linux migration (2026-08-04), which between them exercised every wrinkle so
far: a crates.io dep, a GitHub-native upstream, a multi-GB tree, a dep with more than one build
consumer, and a series whose verdicts said drop / rewrite / fold / carry.

The audit that decides *what happens to each patch* is a separate, already-completed piece of
work: `docs/upstreaming/ledger/` holds per-patch verdicts for all 7 series (2026-08-03). **This
document is about executing those verdicts, not re-deriving them** — though expect the execution
to correct some, because applying a verdict tests it in a way reading never does. Both linux
findings below came out that way.

## The model

- One fork per dependency in the **liminavm** org, **public**.
- **`limina`** is the integration branch and the repo's **default branch**. Upstream's own
  branch (`main`/`master`) stays untouched, tracking upstream.
- `third_party/manifest.toml` pins `{repo, upstream, branch, rev}` per dep. `cargo xtask vendor`
  clones and checks out the pinned rev. Optional keys: `base` (the upstream tag/rev the branch
  sits on) and `heavy = true` (a tree this host never builds — skipped unless `--heavy`, and
  cloned blobless when materialized).
- **Tag before every rebase.** Tag the outgoing head (`limina/<date>`) so every rev ever pinned
  stays reachable. This replaces `UPSTREAM_BASE`.
- **`git format-patch` becomes a derived artifact**, not a source of truth — for upstream
  submission, and for any build that applies our changes onto a *different* base than the branch
  (see "more than one consumer" below).

## Sequence per dependency

1. **Fork on GitHub** from the real upstream when it is GitHub-native; otherwise create the org
   repo and push upstream history. *Pick the parent that actually contains your base:* for linux
   that meant `gregkh/linux` (the stable mirror), because stable point releases do not exist in
   `torvalds/linux`.
2. **Migrate faithfully first, at the current base** (see the two-phase rule).
3. **Bring up to date** with upstream, in batches, applying the ledger's verdicts.
4. **Validate** (the ladder below), always including a poke VM the user can drive live.
5. **Push** the branch, then make `limina` the **default branch**.
6. **Update** the ledger, `third_party/manifest.toml`, `docs/codebases.md`, and any doc naming the
   old series path. Retire `patches/<dep>/`.

`liminavm/limina` (this repo) goes up **last**, after a publish audit for anything that shouldn't
be public.

## The two-phase rule (the important one for big series)

Do **not** combine "move to a fork" with "rebase onto a newer upstream". They fail differently and
you want to be able to tell which one broke something.

- **Phase 1 — faithful migration.** Create `limina` at the series' *current* `UPSTREAM_BASE` and
  apply the existing patches as commits, changing nothing. The branch now equals what we ship, so
  the built artifact should be **byte-identical** (or at least behaviourally identical) to the
  pre-migration build. That is a provable no-op and a cheap, high-confidence checkpoint.
- **Phase 2 — base bump + verdicts,** in batches. This is where behaviour can change, so it gets
  the real validation.

linux skipped phase 1 because six patches with four already-decided verdicts is not worth the
ceremony. **libkrun (126) and virglrenderer (58) must not skip it.**

## Batching

For anything above ~15 patches, work in batches of **5–10**, and:

- **Batch by arc, not by ordinal.** The ledger's `fold` column groups patches that must move
  together (virgl `0041+0043+0044+0049` squash into one artifact; KK `0001` splits into several).
  An ordinal batch would cut a fold in half and produce a branch state that is neither the old
  behaviour nor the new one. Read the fold column first and let it define the batches.
- **Never send or judge a fold member alone.** virgl 0041 as shipped regressed vkmark 2760→1193
  until 0043/0049 recovered it — the squash is the unit of correctness.
- **Each batch is a commit range that builds and boots.** If a batch can't reach that, it's the
  wrong batch boundary.
- **Log what a batch deferred.** A batch that drops a patch "for now" writes that into the ledger
  row in the same commit, or it will read as done.

## Validation ladder

Match the cost to what the batch touched; don't run the top rung for a docs-only change.

1. **Build + link check** — for the graphics stack, `otool -L` the worker
   (`limina-virgl-link-trap`).
2. **Targeted tests** — the L1/L2 binaries covering the subsystem the batch touched
   (`cargo xtask test -E 'binary(...)'`).
3. **Poke VM, all features on** — build and launch it **before** the long suite so the user can
   stress it while the suite runs: windowed, `--ssh-port` pinned outside the auto-allocation
   (2222+; use e.g. 2299), plus `--snapshot-file` (else window-close is a plain shutdown, not
   suspend) and `--memory MIN..MAX` (+ `--balloon-free-page-reporting` on enhanced guests).
4. **Full HVF suite** — once per component, or per few batches for the large series. ~28 min
   parallel. **Never `cargo build` while its guests are booting.**
5. **Perf pass** — for anything on the graphics path. Pin display mode and scale first
   (`limina-perf-display-pinning`), and check whether prior ledger rows are even comparable: if
   the batch changed the present path, older rows are a different regime, not a baseline.

## Handling a dep with more than one build consumer

The series is usually consumed by more than the one build you're thinking of — linux had **four**
(the enhanced kernel RPM, two test-kernel recipes at other tags, and the payload's source-reference
bundle). Enumerate them with a repo-wide grep for the series path *before* deleting it.

The rule that fell out:

- A consumer that builds **the branch's own base** fetches the pinned rev directly and has **no
  patch stage at all**.
- A consumer that builds **some other base** (test kernels at other tags) still needs a series:
  generate it with an export script (`scripts/export-linux-patches.sh` is the template —
  `git format-patch base..rev`, with a guard that the manifest's `base` really is an ancestor of
  `rev`).
- A consumer that only needs **provenance** (a source-reference bundle) ships the manifest pin,
  not patch files.

## Patches that leave the series entirely

A patch that is **known-rejected upstream** *and* a **no-op for the branch's own consumer** does
not belong on a branch whose purpose is upstreamable delta. Move it next to its real consumer with
a README recording the rejection and the exit condition. The 16 KiB alignment patch went to
`guest/virtio-gpu-dkms/` this way.

Beware the exit that looks reachable but isn't: its replacement (`VIRTIO_GPU_F_BLOB_ALIGNMENT`) is
merged upstream, but two of the three links in the chain are stock Fedora's kernel and Mesa, not
ours — so the module stays. **Check who ships each link before declaring an exit.**

## Lessons that cost us something

- **A tolerant apply is a silent-failure machine.** `patches/linux/0001` stopped applying at the
  7.1.x bump and printed `SKIP …` into build logs nobody read; a patch we believed we shipped had
  been absent for months, and a *separate* investigation then "eliminated" a suspect by assuming
  that code ran. The fork model removes the third state — a change is on the branch or it is not.
  Where a tolerant apply must survive, treat every skip as a finding to chase.
- **"Fixed upstream" ≠ "fixed in your base."** The page-reporting UAF fix was merged, Acked and
  Cc: stable, and still absent from v7.1.6. Check the base you are actually building, then
  backport the upstream commit rather than keeping your own shape — upstream's covered cases ours
  did not.
- **A citation is a claim.** The ledger attributed the XRGB-only plane list to a 2017 series
  through several revisions; it is 2018's `42fd9e6c29b3`, and the narrowing is a side note inside
  a big-endian fix — which changes how the submission should be pitched. Run `git log` before
  repeating a SHA, and never abbreviate one from memory.
- **Stale artifacts in an output directory outrank your new one.** `build-kernel-rpm.sh` copies
  everything out of `rpmbuild/RPMS/`, and `install-enhanced.sh` picks with `head -1` — which sorts
  to the *older* version. Verify the artifact you think you built is the one being consumed.

## Agents never post

No agent files an issue, opens an MR, or comments on an upstream tracker. Submissions are the
user's step. The audit and this migration produce *drafts and verdicts*, never traffic.
