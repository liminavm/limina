# Memo — 2026-08-13: the settle-sweep/scrub triggers in the field

What the dogfood Dev VM (24 GiB guest, `reclaim=moderate`, 32 GiB host) taught us the day
the demand sweep and idle scrub were deployed. Written from the deployed build's own
instrumentation — the balloon decision trace (`<bundle>/logs/balloon-trace.jsonl`), which
since `663716d` carries `footprint_bytes` and `compressed_bytes` per report, plus
`ledger-dump` and `footprint(1)` on the live worker.

Companion spike records: `spikes/hv-ledger-gap/RESULTS.md` (the ledger gap itself),
`spikes/hv-ledger-marker/RESULTS.md` (per-pmap billing shapes).

## What shipped that day

| commit | what |
|---|---|
| `663716d` | idle scrub (quiet-day settle) + `compressed=`/`footprint=` in the balloon stats verb, from `TASK_VM_INFO` |
| `9e53151` | end-to-end test driving `on_pressure` into the idle-scrub start over a real socket |
| `f50ed38` | gap-triggered demand sweep, bounding the churn ratchet |

Full HVF suite green for the first two (104/104). The demand sweep's own suite run was
103/104, the single failure being `venus_shell_replay_matches_llvmpipe_reference` stalling
for 956 s under parallel-suite load; re-run solo it passed in 63.6 s. Load flake, not a
regression — but if that stall recurs it deserves its own investigation, because a
16× slowdown of a guest-side `eglretrace` under host load is not obviously benign.

## Finding 1 — the demand sweep works, and the cadence alone never could

The problem it was written for, observed that morning on the pre-fix build: heavy guest
churn regrows the double-billed share at roughly 1 GiB/min, so between two 30-minute
cadence sweeps the worker ratcheted to a **40 G footprint on a 24 G guest**.

After deploy, under comparable load (a research session plus a backup), the demand path
paced sweeps at its 60 s rate limit — 11 sweeps in one 20-minute window — and held the
footprint at 14.3 G. The cadence could have fired at most once in that window.

The trigger is a measured quantity, not a clock: `footprint − compressed − (guest_size −
balloon_fill) ≥ 4 GiB`.

## Finding 2 — the sweep cannot reach compressed billing (measured, after a wrong call)

**This is the one that cost a bad recommendation, so the reasoning is recorded in full.**

Under host memory pressure the double-billed phantom does not stay resident — it gets
compressed along with everything else. At the benchmark peak the worker showed ledger
`phys_footprint` 42.75 G / `internal_compressed` 29.89 G, while `footprint --swapped`
(each mapping counted once) totalled 19 G with ~15.7 G genuinely swapped. So ~24 G was
phantom, and ~14 G of the phantom sat in the compressed column.

The tempting conclusion — that the sweep should therefore chase compressed billing, i.e.
that the `compressed_bytes` subtraction in the demand sensor is wrong — was reached from a
single correlation and **is false**:

- The evidence *for* it: at 13:13:44 a sweep debiting 4.11 G coincided with compressed
  falling 24.23 → 20.35 G (−3.89 G) while the balloon stayed flat (4.2 → 4.4 G).
- The evidence that kills it: per-second trace mining shows `released_bytes` jumped
  **+3.84 G in that same tick**. The free-page-reporting release path discarded those
  pages — compressed copies included — and the sweep's own debit came from resident.
- The direct measurement: sweep #24 at 14:16:31 debited **6.05 G with 15.1 G of compressed
  sitting right there**, and every byte came from the resident column (20.58 → 14.54 G);
  compressed moved −0.01 G.

So `mprotect NONE→RW` debits resident billing only, and **the subtraction is correct —
keep it**. A correlation at one tick is a lead; the other columns in the same tick are
what turn it into a finding. (The project's own debugging rules say exactly this. They
were not followed, and one measurement would have prevented the wrong advice.)

**Corollary that matters for design:** the only lever that clears *compressed* phantom is
the release path — free-page reporting and balloon capture, which discard pages outright.
This was visible in recovery: after the benchmark ended, footprint fell 44 → 28 G in three
minutes **with the sweep counter unchanged**, purely from the balloon re-inflating as the
guest freed.

## Finding 3 — the reclaim pool is not being compressed

Worth recording because it was the natural hypothesis and it is wrong. Across hours with
the balloon holding 13–15.6 G, `compressed` sat flat at ~1.3 G. It only rose after the
balloon collapsed and the guest filled the memory itself. Pool pages are being released
properly; the compression we see is the guest's own live working set, which a 24 G guest on
a 32 G host cannot avoid.

## Finding 4 (open defect) — the yield guard's holdoff is too blunt

The demand sweep judges its own yield: a sweep debiting under `DEMAND_SWEEP_MIN_YIELD`
(512 MiB) proves the gap was honest overhead rather than double billing, and arms a
holdoff for a full cadence period. The intent is right — the gap sensor cannot distinguish
honest overhead from phantom, so it measures instead of assuming.

The duration is wrong. Observed sequence:

1. 13:46 — demand sweep yields **449 MiB**, just under the floor. Correct reading: taken
   mid-benchmark, the resident residue really was small. Holdoff arms for 30 minutes.
2. 13:50–14:15 — the benchmark ends, the guest frees ~20 G, and the (properly subtracted)
   gap sits at **6.7–11.1 G** for the entire holdoff with demand sweeps suppressed.
3. 14:16 — the holdoff expires and the very next sweep debits **6.05 G**.

A low yield means "the gap is honest *right now*", not "for the next 30 minutes". Fix:
make the holdoff gap-aware — cancel it once the gap grows materially past the level at
which the low yield was measured — or simply shorten it to a few minutes.

## Finding 5 — the idle-scrub PSI gate is reachable

Checked against the live trace before trusting the design: 6,574 reports over 1.9 h with a
research session active were **100% idle-gate-eligible** (`some_avg10` = 0 in 79%, ≤ 200 in
all but 8; MemFree always reported). The binding constraints on idle scrubs are therefore
the 90-minute cadence and the ≥512 MiB capturable-residue test, not the PSI gate.

## Finding 6 — the graphics pool is *not* leaking: it tracks the guest's live set

The day's open question was whether the IOAccelerator pool's growth (1.90 G / 9,579 regions →
3.43 G / 13,710 over a session) is host-side retention or the guest genuinely holding more.
Neither the host `footprint(1)` curve nor "I closed two apps and it dropped 22%" could settle
it, because nothing measured the guest's own live set.

Settled on the dogfood VM by draining the guest side in two steps and sampling both ends on
the same clock (host `footprint`; guest `/sys/kernel/debug/dri/0/{virtio-gpu-host-visible-mm,
framebuffer,clients}`):

| guest state | host gfx | host regions | guest live blobs | guest fb | DRM clients |
|---|---|---|---|---|---|
| full session (compositor + apps) | 1929 MB | 12,296 | 67 (420 MB) | 8 | 14 |
| ~1 min after logout (teardown still draining) | 1056 MB | 3,840 | 31 (247 MB) | 5 | 6 |
| `systemctl stop gdm` | **6.2 MB** | **16** | **0** | 1 | 0 |
| gdm restarted, fresh greeter | 162 MB | 556 | 13 (130 MB) | 4 | 3 |

Read the middle row as *mid-teardown*, not as the greeter's cost: a freshly started greeter
(last row) sits at 162 MB / 556 regions, 7× lower. The logout sample was simply taken before
the session's clients had finished exiting.

Within 20 s of the last DRM client exiting, the pool returned to **16 regions** — the floor.
So the host retires everything the guest actually releases; there is no unbounded accumulation
and no orphaned-region class. The pool is a faithful (if amplified) shadow of the guest's live
set, and the day's growth curve is the *guest* holding more, not the host failing to let go.

Two things this does **not** absolve, and both are now the real questions:

- **Amplification.** 67 guest-live blobs / 420 MB stand behind 12,296 host regions / 1929 MB —
  ~180 host regions per guest blob. That ratio is where the memory actually goes, and it is a
  host-side property (Metal heaps and command-allocator retention, see
  `limina-vrend-gfx-region-leak`), not something the guest can bound.
- **Per-client retention inside a long-lived compositor.** Closing one app returned only
  ~267 MB of ~1.6 G and left ~9,000 regions standing; the full teardown then returned all of
  it. So the retention is scoped to the *compositor process's* lifetime, not the app's — which
  is plausibly correct behaviour, but it means an app-churning session ratchets until logout.

Method note worth keeping: the first soak run read "pool flat for an hour" and that was a
**lie** — the churn workload had died 50 minutes in (`drmModeAddFB2WithModifiers rc=-22
stride=0` at buffer 197,601) and nothing noticed. A soak whose workload can stop silently
measures an idle VM and reports a pass. `spikes/gpu-pool-soak/churn-keepalive.sh` now watches
and restarts it, and logs every restart — an allocation that fails after ~200k buffers is
itself a lead worth pulling.

## Steady-state composition, for reference

A settled reading (14:24, guest idle, balloon holding 16.79 G of 24 G):

- **Guest's view**: MemTotal 6.93 G, MemAvailable 3.06 G, MemFree 0.59 G. The guest
  genuinely believes it is a 7 G machine.
- **Host real pages** (`footprint`, each mapping once): 8.17 G footprint + 7.59 G
  reclaimable pool. Graphics 3.32 G (IOAccelerator) + 0.60 G (IOSurface) + 0.27 G
  (owned-unmapped); guest RAM live 2.91 G; malloc 0.97 G.
- **Host ledger** (what Activity Monitor and jetsam see): `phys_footprint` 15.41 G =
  7.97 G internal + 4.46 G compressed, with a 7.41 G reusable balance alongside.

Note the shape: **the largest single consumer of real memory is the graphics stack
(~4.2 G), not the guest (2.91 G)**. Same reading before the benchmark: 1.90 G across 9,579
IOAccelerator regions; after: 3.43 G across 13,710, still climbing at idle. Whether that
retires is a separate thread from the balloon work — see
`docs/design/gpu-memory-budget.md` and the region-retirement work it describes.

## Plan

Ordered by confidence, highest first.

1. **Make the demand holdoff gap-aware** (Finding 4). Cancel the holdoff when the gap
   grows materially past its level at the low-yield measurement, or shorten it to a few
   minutes. This is a measured defect with a measured cost: 30 minutes at 6–11 G of
   overstatement.
2. **Keep the compressed subtraction** (Finding 2). No change; recorded here so it is not
   re-litigated.
3. **MemFree-jump capture trigger.** Since the release path is the only lever that clears
   compressed phantom, a large rise in the guest's free list — a workload ending — should
   trigger an immediate Bounded capture rather than waiting for the inflation ladder's
   dwell. This is the "frontrun" idea in its highest-value form.
4. **Slope-based host pressure, not just level.** The policy acts on a three-state host
   level; host free fell 78% → 52% during the benchmark while still reading "normal". An
   *Elevated* band (or a falling-fast rule) could start a Bounded capture before the host
   has to compress anything, which is much cheaper than compress-then-decompress.
5. **Observability: the managed app logs at WARN**, so the demand-sweep and idle-scrub
   INFO lines never reach `supervisor.log`. Field attribution had to infer demand pacing
   from sweep-counter deltas. Give those events WARN-level lines or trace markers of the
   scrub's `"scrub":"start"` shape.

Constraint on everything in 3–5: aggression must stay **Bounded depth**. Capturing free
pages early is nearly free for the guest; capturing cache costs the measured 64× random-read
IOPS cliff (`spikes/mem-overhead-2026-07-02`, Run D), and the existing inelastic-hold exists
precisely because inflating during reclaim just feeds the balloon from page cache.
"Frontrun" must mean *earlier*, never *deeper*.
