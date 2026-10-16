# Accepting the weekly fork rebases

A weekly routine rebases our forks onto their upstreams and opens one `[auto-rebase]` PR per
branch **in the fork** (head = the rebased branch, base = a snapshot of the upstream tip, so the
diff is exactly our delta). It never moves `limina`, `main` or any tag, and nothing it pushes is
tested against a real host. Accepting those PRs is a local job, and this is how it runs:

**find → review → fix → build and test → (fix again) → report → push on approval.**

Nothing is pushed before the user approves the round. Approving the round covers the fork
branches, their tags and the limina pin commit. It covers nothing else.

## 1. Find what is pending

```sh
for r in libkrun imago mesa linux virglrenderer virglrs janus; do
  gh pr list -R liminavm/$r --state open \
    --json number,title,headRefName,baseRefName,updatedAt \
    --jq '.[] | "\(.number) \(.updatedAt) \(.headRefName) -> \(.baseRefName) | \(.title)"'
done
```

For each `[auto-rebase]` PR, save the description and the refs to the scratchpad:
`gh pr view N -R liminavm/<fork> --json title,body,commits,headRefOid,baseRefOid`. The description
is the routine's own account of the run. It lists the old base and tip, the new base, the rebased
tip, conflicts and how they were resolved, and items it marks **NEEDS HUMAN**.

The Mesa PRs are stacked. `limina-kk` goes onto upstream main.
`metal-native-context` is restacked on the rebased kk, so its PR's base is the kk rebase branch.
`limina-guest` moves only within its point-release series, and moving to a new series is the
user's call.

**Check every PR's tip against the fork's live branch.** The fork keeps moving after the routine
runs. Compare the "limina tip" the PR rebased from with `git ls-remote liminavm <branch>`. If the
live branch has commits the PR lacks, replay them onto the PR's rebased tip with
`git cherry-pick <pr-limina-tip>..<live-head>` and accept *that*. Later, lease the push against the
**live** head.

## 2. Review

Work in `cargo xtask worktree new <name>`. Its `third_party/` clones are worktrees of main's, so
the PR refs are visible after a `git fetch liminavm`.

- **Review and edit Mesa outside the shared clones.** `/Volumes/mesa-cs/mesa` and its sibling
  worktrees belong to every session on the host. In them, run only `fetch` (to get the PR refs:
  `git -C /Volumes/mesa-cs/mesa fetch liminavm`) and read-only commands (`log`, `show`,
  `range-diff`). The one sanctioned move of the shared checkout is the host KK build in step 4.
  To edit a candidate or build the guest drivers from it, make a private case-sensitive volume
  and detached worktrees on it:
  ```sh
  hdiutil create -size 12g -fs "Case-sensitive APFS" -type SPARSE -volname mesa-accept <scratch>/mesa-accept.sparseimage
  hdiutil attach <scratch>/mesa-accept.sparseimage -mountpoint /Volumes/mesa-accept
  git -C /Volumes/mesa-cs/mesa worktree add --detach /Volumes/mesa-accept/mesa-kk  <rebased-kk-tip>
  git -C /Volumes/mesa-cs/mesa worktree add --detach /Volumes/mesa-accept/mesa-mnc <rebased-mnc-tip>
  ```
- **Hand each non-trivial PR to a separate read-only reviewer**: a fable subagent, briefed with
  the four refs and the PR description. The core of the review is the per-commit range-diff
  `git range-diff <old-base>..<old-tip> <new-base>..<new-tip>`. Ask for every NEEDS HUMAN item,
  every conflict resolution and every commit whose range-diff is not `=`. Ask what an upstream
  change newly enables that our commits assumed away.
- **The failure to look for: a conflict "resolved by taking upstream's side" can keep half of a
  paired change.** For example, a commit made readers lock-free *and* made the matching writers
  atomic. Upstream reworked the writers, the resolution took upstream's writers, and the lock-free
  readers were left racing plain stores. The build was clean and the bot's own validation passed.
- **Upstream may have turned on a feature our code never had to handle.** For example, a newly
  advertised capability (16-bit shader I/O) reached a lowering pass of ours that assumed 32-bit
  slots. Read the upstream log between the two bases for capability or feature-flag changes in
  code our commits touch.
- Libkrun: check our adaptations of new upstream code paths keep our invariants. The vsock RX
  drain, for example, must go through our `pop_rx`/`signal_rx` so EVENT_IDX guests still get
  their kicks.

## 3. Fix

Fix on the candidate branch so that each fix lands in the commit it belongs to:

```sh
git commit --fixup=<commit>          # or an `amend! <subject>` commit to also rewrite its message
GIT_SEQUENCE_EDITOR=: git rebase -i --autosquash <new-base>
git range-diff -s <new-base>..<pr-tip> <new-base>..HEAD | grep -v ' = '   # only the fixed commits may show
```

If the old commit message describes a design the rebase changed, rewrite it. A message that still
cites a measurement from the old design must say the number is owed until it is re-measured.

Restack anything stacked on a fixed branch:
`git rebase --onto <fixed-kk> <pr-kk-tip> <mnc-tip>`. Then point the worktree's
`third_party/manifest.toml` at the candidate revs (`rev`, and `base` where the branch has one).

Before blaming the candidate for a build failure, check that **every other** `third_party/`
checkout sits at its pin (`cargo xtask pins`, plus `git -C third_party/<dep> rev-parse HEAD`). A
virglrs clone behind its pin breaks the worker build in ways that look like a libkrun API break.

## 4. Build and run the suite

Each fork is covered as follows:

| Fork / branch | Build | Covered by the suite as |
|---|---|---|
| libkrun, imago | `cargo xtask build` (worktree clones at the candidate revs) | every test |
| mesa `limina-kk` (host KK + zink) | `scripts/build-host-mesa.sh both` **in place**, run from the worktree. It moves the shared checkout's `limina-kk` branch to the *worktree's* manifest pin (so bump the pin first, step 3) and builds into the shared `/Volumes/mesa-cs/build-kk` and `zink-kk-prefix`, the only KK the harness loads. Keep it if the suite is clean. If the round is rejected, run the script again from a checkout at the old pin. | every venus/vrend test |
| mesa `metal-native-context` (guest vendor drivers) | `MESA_TREE=/Volumes/mesa-accept/mesa-mnc VOLUME=<own-volume> scripts/build-gpu-drivers.sh`, then `scripts/package-gpu-drivers.sh` | the metal and enhanced tests, through image clones (below) |
| mesa `limina-guest` (guest venus RPM) | none: bump the pin and run `scripts/export-mesa-guest-patches.sh` | **not tested.** No RPM build is part of acceptance. |
| Cardea (when the pin moved, or the suite refuses a stale firmware) | `scripts/build-firmware.sh` (`cargo xtask firmware`) | every EFI boot |

Deliver the vendor drivers into **clones**, never the shared images:

```sh
for r in metal.test enhanced.test enhanced.synoik enhanced.kde; do
  cp -c <main-checkout>/Fedora-Workstation-44.$r.raw accept-$r.raw
done
mkdir -p <scratch>/deliver          # the script does not create it
LIMINA_GPU_DRIVERS=target/gpu-drivers/limina-gpu-drivers.tar.gz LIMINA_DELIVER_BACKUP=0 \
LIMINA_DELIVER_LOGDIR=<scratch>/deliver \
  scripts/provision/deliver-metal-transport.sh accept-metal.test.raw accept-enhanced.test.raw \
    accept-enhanced.synoik.raw accept-enhanced.kde.raw
```

Then run the suite against the clones, as a backgrounded task:

```sh
LIMINA_TEST_DISK_METAL=$PWD/accept-metal.test.raw LIMINA_TEST_DISK_ENH=$PWD/accept-enhanced.test.raw \
LIMINA_TEST_DISK_SYNOIK=$PWD/accept-enhanced.synoik.raw LIMINA_TEST_DISK_KDE=$PWD/accept-enhanced.kde.raw \
  scripts/run-suite.sh <scratch>/suite.log
```

`scripts/run-suite.sh` refuses to start while another run is live; wait for that run rather than
working around the check. Do not build or commit while the suite runs. Run the suite after the
review's fixes are in, not before: a run on a tree the review is about to change proves nothing.

## 5. Triage, and cycle

**A failing test is a regression only against a baseline rate.** Measure the rate on both sides
before reading anything into one red test:

```sh
# candidate: the worktree as built for the suite
for i in $(seq 1 12); do
  LIMINA_HVF_TESTS=1 gtimeout --kill-after=30 600 \
    cargo nextest run -p limina-test --test <binary> --no-fail-fast > <scratch>/cand-$i.log 2>&1
  echo "cand $i exit=$?"
done
# baseline: check the third_party clones out at the OLD pins, `cargo xtask build`, same loop
```

Compare the failure counts on the same assertion. A small, uneven split is not evidence. For
example, 3/12 failures on the candidate against 1/18 on the baseline (measured 2026-10-14) is
p ≈ 0.16 by Fisher's exact test. Either take more runs or call it a pre-existing flake and name
it in the report. A failure that is real on the candidate goes back to step 3, and the suite runs
again on the fixed tree.

## 6. Report, then push on approval

Stop and report to the user:

- what was accepted as-is;
- what was replayed onto a stale tip;
- each fix and why it was needed;
- the suite verdict;
- any flake, with both rates;
- what is owed: measurements a fix invalidated, coverage that was skipped (the guest RPM).

Push nothing until the user approves.

On approval, push each fork branch in this order. Push from the clone that has the commits, to a
remote with write access to `liminavm`:

```sh
git tag pre-rebase-<date> <old-live-head>            # -mnc / -guest suffix for the other mesa branches
git push liminavm pre-rebase-<date>
git push --force-with-lease=<branch>:<old-live-head> liminavm <new-tip>:refs/heads/<branch>
git push liminavm <new-upstream-base>:refs/heads/main       # the fork's main tracks upstream
```

Then the limina side:

1. Commit `third_party/manifest.toml`, plus `patches/mesa-guest/` when that pin moved. The
   message gives what each fork moved onto, each fix, and the suite verdict, including the flake
   and both of its rates.
2. Bring the upstreaming ledger along in the same push (`docs/upstreaming/ledger/`, one file per
   fork). For each fork that moved, update the file's header: the new base, the commit count, the
   rebase date and the `pre-rebase-<date>` tag. Rewrite every SHA the file cites to its rebased
   commit; `git range-diff` from step 2 pairs them. Record each commit that left the branch and
   why: superseded upstream (name the upstream commit), folded into another commit, or dropped.
   Then add a skeleton row for each new commit with
   `cargo xtask ledger --skeleton <ledger>`. `cargo xtask ledger` must then report every ledger
   clean. Rows are keyed by subject, so rewritten SHAs alone never show up as drift. Nothing
   else reads the ledger, so a round that skips this step leaves the gap unnoticed.
3. `cargo xtask pins --check`. The pre-push hook runs it too, and it refuses a pin no remote
   carries. The hook also warns, without refusing, when a pushed pin bump leaves the ledger
   behind.
4. Push limina `main`.
5. Close each PR with a comment that names the accepted rev, any fix or replay, and the tag that
   keeps the old tip.

## 7. Clean up, and say what is left behind

Remove what the round created:

- the worktree and its branches;
- the private Mesa worktrees (`git -C /Volumes/mesa-cs/mesa worktree remove …`) and their
  branches;
- the sparse image (`hdiutil detach`, then delete it);
- the `accept-*.raw` clones;
- the drivers container volume (`container volume delete <own-volume>`).

If the Cardea pin moved, rebuild with `cargo xtask firmware`, then copy the new `target/firmware/efi.fd` and its `.rev` into the
main checkout. Otherwise main's next suite refuses to run with the stale firmware.

The shared Mesa trees that other sessions build from (`/Volumes/mesa-cs/mesa-guest`,
`/Volumes/mesa-cs/mesa-metal-guest`) are **not** moved by this procedure. Say in the report that
they now sit behind the pins, so whoever builds from them next moves them first.
