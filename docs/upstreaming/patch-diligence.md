# Pre-send diligence for one upstream patch

The ledger (`ledger/README.md`) audits a whole series: is each patch still needed, is there
an issue or MR, is it security-sensitive. This is the deeper pass one patch gets before it is
filed, after it has been picked to send. Mesa's 0004 ("venus: don't run a thread's TLS
teardown from an unloaded driver", `spikes/upstream-repro/venus-tls-destructor/`) is the
worked example; the steps generalise.

The output is a reproducer README holding the evidence and the points the MR description
needs, a standalone branch whose tree is exactly what was tested, and an updated ledger row.
Filing stays with the user: agents never post, and Mesa's AI policy wants the commit message,
code comments and MR text in the submitter's own words (`ledger/mesa.md`).

## 1. Reproduce on the upstream stack

- The reproducer runs on a setup the maintainer has: stock QEMU + distro virglrenderer, a
  plain software driver, never limina or a macOS host. For Mesa, the Intel Fedora rig in
  `spikes/upstream-repro/README.md`.
- Minimal and standalone: one source file, one build line, one run line, a printed verdict.
- Before/after on `main` at a recorded SHA, several runs each (3/3, not "it worked"). Add the
  distro build if users hit it there.
- Record what the reproducer does **not** need (host GPU, page size, renderer): it tells the
  maintainer how widely the bug applies.

## 2. Understand the mechanism before judging the fix

- Read every path that reaches the faulty code, not just the one the reproducer uses. For
  0004 that was every caller of `vn_tls_get()`, which put the bug on every thread that
  creates a device.
- Read the lifetime and ownership rules around it (two-owner teardown, who frees what, in
  which order). The fix must keep them.
- Explain why the trigger has the shape it has (0004 needs a worker thread because `exit()`
  runs no key destructors for the main thread). Each such reason is a case to test.

## 3. Find the commit that introduced the condition

- `Fixes:` names the commit where the bug became **possible**, not the one that made it worse
  or easier to hit. Read the candidates' diffs. 0004's crash is the jump into unmapped code,
  so it dates from the commit that registered a driver function as destructor
  (`d17ddcc8477`), even though the destructor only called `free()` then. The later
  "TLS ring" commit gave it more work but changed nothing about the jump.
- Confirm the parent lacks the condition (`git grep` at `<sha>^`).
- Affected releases: `git describe --contains <sha>`. Before a release branch point ⇒
  `Cc: mesa-stable`.
- History searches need a full clone. The worktrees on `/Volumes/mesa-cs` are shallow (a few
  thousand commits) and silently return nothing for older history; the Mesa clone on the upstream repro rig is
  complete. Full-history pickaxe (`-S`/`-G`) takes many
  minutes: run it detached, or bound it with `--since`/`--until` and a path.
- Optional, when the MR should quote it: build the first-bad commit and run the reproducer.

## 4. Search for prior art

Search the tracker, the mailing list and the history for the specific bug and for its
**class**. The class searches find the useful material.

- **Tracker, specific:** function and file names, the symptom (backtrace frames such as
  `__nptl_deallocate_tsd`, error strings), the driver name and prefix (`vn_`, "venus").
- **Tracker, class:** the mechanism's vocabulary (`dlclose`, `nodelete`, "unload",
  `pthread_key`, "thread local destructor"), across all drivers. 0004's closest precedent was
  a sysprof crash in anv and radv (mesa#13571), not anything venus.
- Read closed and rejected items, not only open ones. A declined MR shaped like ours shows the
  objection the submission must answer (mesa!36978, a `-z nodelete` attempt closed for a fix
  elsewhere).
- Follow `Closes:`/`Fixes:` trailers and "mentioned in" links out of every hit. They lead to
  the fix that actually landed, sometimes in another project (GNOME/sysprof!152).
- **Mailing list:** Mesa before ~2020 discussed on mesa-dev
  (`mail-archive.com/search?l=mesa-dev@lists.freedesktop.org&q=…`). Old maintainer opinions
  on the approach live there.
- **Git history:** `git log -i --grep=<term>` for commit messages and `-S<term>` for code
  that came and went. This finds fixes for the same class (0004: Perfetto shut down before
  unload) and reverts, which show what upstream wants preserved (0004: `dlclose()` of the
  driver is kept on purpose; leak-checking builds are the only place it is disabled).
- Record what the search could not cover. Mesa GitLab comments need a login to search, so
  only titles, descriptions and the threads opened directly were read.
- Tooling: `/api/v4/projects/mesa%2Fmesa/{issues,merge_requests}?search=` works with curl.
  Notes return 401 on REST, but `/-/issues/<n>/discussions.json` and
  `/-/merge_requests/<n>/discussions.json` serve them. More craft in `ledger/README.md`.

## 5. Look for the fix a maintainer would write

The first working fix is a candidate, not the answer. A fix can work and still have the wrong
shape, and a maintainer then rewrites it. Look for that shape before sending:

- **State what the fix changes besides the bug.** 0004's first version pinned the driver for
  the rest of the process, which ended unloading, something upstream keeps on purpose.
- **Fix the decision, not its consumers.** When several stages act on one decision and
  disagree, make the decision give the same answer everywhere instead of reconciling each
  stage. A fix that needs a follow-up commit for a sibling stage is the signal. Mesa 0019:
  venus judged whether a signal semaphore holds a sync fd from its current payload, which the
  waits change mid-submission. Correcting the sync count, then the pNext arrays, fixed two
  consumers; judging by the permanent payload, which a temporary import never replaces, fixed
  them all in one place.
- **Get independent alternatives** (below), then **list them all** and judge each on: scope
  (only the processes or threads that need it), preserved behaviour, races, leaks,
  portability, and how it reads to a reviewer. Reject infeasible ones with the reason. That
  reason goes in the MR, because reviewers propose the same ideas.
- **Look for the mechanism the platform already provides for this exact problem.** C++
  `thread_local` destructors have 0004's problem, so the C library solves it
  (`__cxa_thread_atexit_impl` holds the library while a teardown is pending). A
  purpose-built mechanism usually beats a general hammer.
- **Ask where the fast path is.** Process exit should do no new work (0004 skips registration
  on the main thread).
- **Ask what else can run after the fix's teardown.** Another thread-exit destructor can call
  back in (0004 clears its pointer before freeing).
- **Choose fallbacks per platform with the user.** The trade-off is theirs. 0004 takes a small
  leak and a narrow race over a pin where the hook is missing.

### Independent alternatives

Whoever wrote the fix, or has read it, reviews it instead of looking for the cause. So the
alternatives come from proposers who have never seen it:

1. **A brief without the fix.** It holds the mechanism from §2, every path that reaches the
   faulty code, the invariant that breaks, the reproducer with its verdicts, and the upstream
   SHA. It holds nothing that names or points at our fix: not the diff, not our branch, not
   the reproducer README or the ledger row.
2. **A tree that holds only upstream**: a worktree at that SHA with no other refs. Our forks'
   clones carry the fix on their branches, so a proposer must not work in them. The shallow
   worktrees on `/Volumes/mesa-cs` are enough for recent authors of the touched files; older
   history needs the full clone on the upstream repro rig.
3. **One proposer per lens**, each independent:
   - the invariant that breaks, restored where no other code has to know (the rule above);
   - the idiom of the people who maintain the touched files, from how they fixed similar bugs;
   - the smallest diff, preferring to remove state or conditions over adding them;
   - the mechanism the subsystem already has for this problem.
4. **A judge that cannot tell which fix is ours.** It gets every candidate, ours included,
   under neutral labels, and ranks them on: cause or consequences, how many places must stay
   in agreement afterwards, behaviour changed besides the bug, whether it reads like the
   surrounding code, size, and correctness on every path in the brief.
5. **A skeptic per top-two candidate** looks for a call path, ordering or input that breaks it.
   What it finds becomes a reproducer mode in §6. When the mode fails, the fix changes and the
   changed fix gets a fresh skeptic: repeat until a round finds nothing. The zink MSRTT series
   took three rounds, and each one found a real bug.

§6 then builds the top candidate, the runner-up and ours. Proposers that arrive at our shape
are a result too: the README says so, next to the alternatives that lost and why.

Calibration, measured 2026-10-16 on Mesa 0019 at `3b1fece6ff5`
(`spikes/upstream-repro/venus-sync-count/alternatives-brief.md`, written as the bug was understood
before the maintainer's fix): all four blind proposers reached the maintainer's shape, one with a
helper body identical to theirs, and the judge ranked our two-commit fix last as patching
consequences. Rerun it when the procedure or its lenses change.

When a maintainer rewrites one of our patches, the difference becomes a rule in this section,
written as a rule; a rule that applies to every patch also becomes a lens.

## 6. Prove each claim with a test that could fail

- Build every serious candidate and run the same reproducer modes on each, including the
  unfixed base. Report them side by side.
- A mode that passes on the unfixed build proves nothing about the fix. 0004's `main-alive`
  passed everywhere, so `main-unload` was added: it gives "yes" if the main thread registers
  anything, and "no" otherwise. Check each column against the baseline before trusting it.
- Test the property the change is meant to buy, not only "no crash". For 0004 that is "the
  driver still unloads", read from `/proc/self/maps`.
- Exercise fallback paths for real. Use a test-only branch that forces the build check off,
  and confirm which path got compiled in (`nm -D` for the imported symbol).
- List the cases no run covers in the README: races, `exit()` from a worker, platforms we
  cannot build.

## 7. Verify platform claims in the source

- Enumerate every OS and libc the component builds for. Read the component's own
  `DETECT_OS_*` branches and meson conditions; venus has Windows branches but builds no
  renderer there.
- Check library behaviour in that library's source (glibc `dl-close.c` and
  `cxa_thread_atexit_impl.c`, bionic `__cxa_thread_atexit_impl.cpp`), never from memory.
- Check the project's own emulation layers (Mesa's `src/c11/impl/threads_win32.c`): they
  decide whether a platform even has the bug.
- Follow the project's conventions for build-time detection (`cc.has_function` →
  `-DHAVE_…` in Mesa's root `meson.build`).

## 8. Package it

- Tag before rewriting any branch the series lives on. Replace the series commit in place and
  check that the old and new tips differ only by the fix (`git diff --stat`).
- The standalone MR branch has one commit on the base the tests ran on, and its tree is
  identical to the tested commit. Say if `main` has moved since.
- Trailers: `Fixes:`, `Cc: mesa-stable` where it applies, the AI disclosure
  (`Assisted-by: Claude Code`), then `Signed-off-by`. No `Co-authored-by` for tools.
- Reproducer README: setup, command, a dated results table per build and mode, uncovered
  cases, prior art to cite, and the points the MR description must make (no drafted prose:
  the submitter writes it).
- Ledger row: upstream subject, `Fixes:`, the one-line verdict.
- Re-check `main` right before sending: fetch, rerun the reproducer if the touched files
  moved, and search the tracker again. Verdicts on active code go stale in weeks.
- Re-check the security class (`ledger/README.md`, phase 3). A guest-triggerable host
  memory-safety bug goes through disclosure, never straight to a public MR. KosmicKrisp is the
  exception: its fixes are already public and only limina puts it behind a guest
  (`ledger/README.md`, phase 3).
- Name the people to ask for review: the author of the regressing commit and the recent
  authors of the touched code (`git log --format=%an -- <files>`), and Mesa's
  root `CODEOWNERS` where it covers the path.
- Push only when the user asks. Before the first push to a new host, check its SSH key
  against a published source. freedesktop publishes SSHFP records:
  `dig +short SSHFP ssh.gitlab.freedesktop.org` against `ssh-keyscan`.
