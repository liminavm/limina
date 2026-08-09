# Allocator retirement: A/B against a same-session control (2026-08-09)

The pool bounded memory but never returned it (nothing destroyed before `vkDestroyDevice`, and on
the vrend tier that never happens before worker exit). kk `f2216e9dc29` retires a drained
allocator that has been idle past a decay window instead of resetting it, above a floor.

**Method.** One `Fedora-Workstation-44.enhanced.raw` clone used for both arms in sequence; VM
restarted between arms so the worker's VM regions start fresh. Display pinned at **1280x800 scale
1.0** and `--verify`'d before every run. 4 vCPU, 4096 MiB. `LIMINA_KK_ALLOC_DESTROY=0` is the
control — the *same binary*, so nothing differs but the policy.

## Memory — the reason the change exists

Acceptance test named in review: worker `IOAccelerator (graphics)` across an app launch/exit cycle
on the vrend tier. Firefox `--kiosk` aquarium at 20 000 fish (GL → vrend → host zink → KK), 75 s.

| | control (`DESTROY=0`) | destroy on | |
|---|---|---|---|
| baseline | 1774 regions / 620.1M | 1775 / 624.1M | — |
| **at peak** | 8148 regions / **1.5G** | 2294 / **815.6M** | **−46%**, regions −72% |
| after app exit + compositing | 10047 regions / **1.2G** | 3046 / **604.0M** | **−50%** |
| retirements | render 0, compute 0 | **render 508**, compute 74 | |

**The dominant effect is during the run, not after it.** The policy was designed to claw memory
back once a heavy app exits; it turns out to matter more while the app is still running, because
it stops the pool accumulating in the first place. That was not the predicted shape.

⚠ The region counts at "after reclaim" **rise** in both arms (2224→3046, 8127→10047) because the
stimulus used to drive reclaim — toggling the GNOME overview — itself allocates GPU resources.
Bytes are the clean metric here; the region count is contaminated by the instrument's own
stimulus. Both arms are contaminated identically, so the comparison holds, but the raw number is
not "memory after idle".

## Throughput — neutral

Medians of three `scripts/perf-ledger.sh` runs per arm, same session, display verified each run.

| instrument | control (`DESTROY=0`) | destroy on | delta |
|---|---|---|---|
| `gl-replay-venus` (fps) | 46.42 | 46.80 | **+0.8%** |
| `gl-replay-llvmpipe` (fps, CPU control) | 744.7 | 752.4 | +1.0% |
| `vk-replay-venus-headless` (fps) | 2022.7 | 1991.2 | **−1.6%** |
| `glmark2-wayland-venus` (score) | 2946 | 2924 | **−0.7%** |

**The CPU control moved as much as any graphics row (+1.0%)**, which is the signature of no real
effect rather than a small one. This despite the policy adding a second O(n) scan per acquire —
plausibly offset because the pool population is now far smaller (live 8–30 vs ~261), so the
existing pass-1 scan got cheaper at the same time. Not separated; the aggregate is what matters.

## An observation on the open 08-08 → 08-09 question, NOT a conclusion

This control reads `gl-replay-venus` 46.42 against 44.4 measured this morning on a *pre-pool*
build, and 47.6 on 2026-08-08. So the "~5–7% drop on composited paths" recorded in
`perf/2026-08-09-allocator-pool.md` is **not reproduced** here: the same workload on a fresh clone
and a rebooted guest sits within 2.5% of the 08-08 value.

That is a hint that some of that drop was session or guest state rather than a code regression,
and it is **not** evidence for any particular cause. Two things differ from the morning runs
besides the code (fresh disk clone, guest rebooted mid-session for display pinning), and neither
was controlled. The bisect remains worth doing; this only lowers confidence that there is a real
regression to find.

## Not covered

- **The floor/decay pair was not swept.** Defaults (floor 8 per class, decay 2000 ms) were picked
  from the design argument and validated once, not tuned against a ladder.
- **Burst behaviour after a long idle** — the pool decays to the floor, so the first burst after
  idle must mint back up. The decay window makes this at most once per idle period, and no
  workload here exercises it deliberately.
- The `LIMINA_KK_ALLOC_POOL_LOG` retirement log is off by default and was on only for the memory
  arms, not the throughput arms.
