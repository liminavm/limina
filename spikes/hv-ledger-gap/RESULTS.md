# Results — 2026-08-10, the dogfood Mac, Dev VM worker (pid 42923, 24 GiB guest, up ~7 h)

## CONFIRMED: the gap is one named ledger entry — `internal_compressed`

`ledger-dump` against the live worker (same-uid, no root needed; two samples ~15 min
apart, second taken simultaneously with a vmmap pass):

| entry | balance | note |
|---|---|---|
| phys_footprint | 46.5 → 45.6 G | peak 50.6 G; moves with compressor churn |
| **internal_compressed** | **42.1 → 41.0 G** | **THE GAP LIVES HERE** |
| internal | 1.5 → 1.6 G | resident anon — tiny |
| phys_mem | 3.7 → 4.0 G | actually-resident total |
| reusable | 2.2 → 2.3 G | cumulative credit 427 G = FRQ working over the VM's life |
| graphics_footprint (+compressed) | 1.0 + 2.0 G | the IOAcc/KK side, separately named |
| purgeable_nonvolatile (+compressed) | 0.16 + 0.29 G | IOSurfaces |

Cumulative credit/debit on internal_compressed: 1044.9 G / 1003.9 G over 7 h — the
compressor has cycled a terabyte through this task. footprint(1) itemizes regions;
it has no row for compressor-held pages of the hv-referenced object, which is why
26 G was all it could see.

## Round 2 (same day): the null model tested and refuted — but "pathological" confirmed

The user challenged round 1's "double charge" framing with the straightforward
reading: 41 G genuinely compressed, of which ~21 G's segments are on disk. Tested:

- `footprint --swapped`'s column is ALREADY compressor+swap combined (macOS swap
  segments are compressor segments written out; `vm.compressor.segment.swappedout`
  413,698 × 64 KiB ≈ 25.9 G ≈ vm.swapusage 26.06 G exactly). Summed attribution:
  ~25 G total, of which internal ~22.6 G (graphics 2.1 G and IOSurface 0.3 G bill
  to their own ledger entries and match). `--vmObjectDirty` attributes 18–20 G.
  Owned-unmapped: 44 MB. The supervisor: 18 MB footprint — exonerated.
- **The existence bound**: the task has only ~25 G of anon that could hold
  compressed copies (24 G guest RAM − 0.6 G resident + ~1.6 G malloc/stacks; blob
  window untouched). 40.6 G billed to internal_compressed therefore CANNOT be
  distinct live pages. ~18 G is orphaned from every view.

So the clean reading is arithmetically impossible — but so was round 1's implicit
claim that the attributable 22 G is "what's really compressed" and the rest mere
phantom. The excess is either (a) REAL stale compressed slots (multiple
generations per page surviving — these occupy actual compressor segments and
swapfile space; the near-full 26 G swapfile makes this the OPERATIONALLY worse
branch) or (b) a ledger debit leak (slots die, billing doesn't). Round 1's
"per-mapping hv double charge" is demoted to one hypothesis among equals — nothing
yet implicates hv specifically.

## Candidate mechanisms (NONE confirmed)

Simultaneous sample: guest-RAM shm cluster (192×128 M anon-shared regions @
0x7000000000) shows **21.93 G swapped** at the vm-object level; other anon swap
(MALLOC etc.) ~0.6 G. Yet internal_compressed holds **41.0 G — 1.87× the
object-level total, ~18.5 G of excess** ≈ the original "ledger gap".

Two candidate mechanisms — do NOT anchor on the first:

1. **Per-mapping double charge**: a compressed guest page billed once for the
   worker's mmap reference and once for the HVF/guest-side reference, while
   RESIDENT guest pages bill once (internal is only 1.6 G with 0.6 G of guest RAM
   resident). The non-clean ratio could mean only pages guest-pmap-resident at
   compression time carry the second charge.
2. **Stale compressor slots surviving MADV_FREE_REUSABLE churn**: the ledger shows
   427 G cumulative reusable credits and 1044/1004 G compress/decompress churn over
   7 h — if a REUSABLE reclaim fails to debit an existing compressed copy, phantom
   internal_compressed accumulates with *churn*, not with hv. 1.87×-not-2.0× is
   mildly MORE consistent with this accumulation story than with a clean
   per-mapping charge.

Discriminating experiment (next step, NOT run yet): standalone probe — anon-shared
alloc + hv_vm_map + touch + force compression (memory_pressure, dev Mac) — A/B
legs: no-hv / MAP_PRIVATE / hv_vm_unmap-after-touch / MADV_FREE_REUSABLE (the
REUSABLE leg is what separates mechanism 2 from 1). Scaffolding:
spikes/balloon-madvise (entitlements + measurement harness).

## Round 4 (same day, dev Mac): churn-probe legs — resident-segment REUSABLE is CLEAN

`churn-probe` full-combination leg (4 GiB anon MAP_PRIVATE|MAP_NORESERVE, hv_vm_map'ed,
3× dirty → ballast-compress → MADV_FREE_REUSABLE → re-dirty; log
`churn-full-*.log`): **no leak**. Every cycle: REUSABLE debited internal_compressed
to 0 and system "stored" dropped by EXACTLY the buffer's compressed page count;
post-exit residue +3,879 pages (~59 MB, noise) vs a ≥165k-page leak signature.
Two hard-won instrument lessons are in the probe comments (COW-ref fork makes
REUSABLE silently no-op; VOID gating). Cycles only reached 21–23 % compressed
(the dev Mac's swap was full and the 0x5A ballast compresses to ~nothing — no
byte pressure), which is precisely what the leg CAN'T see: every slot it freed
lived in a RESIDENT segment (`segswap` frozen all run).

**Secondary finding, systematic**: pages re-dirtied in place while
reusable-marked (production's guest-re-touch after FRQ, since nobody calls
MADV_FREE_REUSE) stay OFF phys_footprint until the pageout scan reprocesses
them — 4 GiB dirty resident billing 0.9 G. The dogfood worker therefore also
UNDER-counts recently re-touched ballooned pages. Real accounting skew, not the
leak.

**Where the leak must live if it reproduces at all**: freeing slots inside
SWAPPED-OUT segments (the dogfood residue sits in swapped segments; dead slots
there wait for compaction/swapin). The `-x -X -w` leg (incompressible fills for
byte pressure + an aging gate that waits for segswap growth before madvising)
targets exactly that path. If that leg is ALSO clean, the remaining
production-only ingredients are: guest-side stage-2 faulting (toy re-dirties
via host writes; the hvf-graceful trap-probe is the vehicle), FRQ's thousands
of small overlapping sub-range madvises, and scale/duration.

### Rounds 4b/4c: swapage and sparse legs — ALSO CLEAN. The toy's verdict is a tight negative

- **swapage leg** (`-x -X -w 300`, incompressible fills): byte pressure worked
  (segswap +162k segments ≈ +10 G, swapfiles grew on demand), but the buffer's
  own slots kept freeing from RESIDENT segments (occ dropped by exactly the
  freed count at every madvise) — the swapped-slot-free condition never
  constructed. Post-exit residue: noise.
- **sparse leg** (`-r`, alternating 16 KiB sub-range madvises + ballast drip
  during aging): segments fragmented, major compaction demonstrably ran (segs
  708k→523k mid-run), live slots survived compaction with EXACT accounting —
  every sparse free debited ic and stored by the precise page count, retained
  live slots showed as exactly the elevated post-cycle baseline. Post-exit
  residue: noise.
- **Scan-resistance lead** (one confounded observation, LRU position differs):
  a buffer re-dirtied in place while reusable-marked was nearly untouchable by
  the pageout scan under severe pressure (5 of 262k pages taken; a second
  VOID cycle took 10k). Recorded as a lead, not steering.

**The toy's deliverable sentence: every host-reachable
MADV_FREE_REUSABLE-on-compressed transition — whole-range, sparse, limbo
re-dirty, post-compaction, resident segments — accounts EXACTLY. The defect
requires an input class the toy cannot generate: guest-pmap-mediated state
(stage-2 faulting is the one untested input to the accounting state machine),
or scale/duration.** Next vehicle: the real-VM reproducer on the balloon-bench
scaffolding (squeeze/release under LIMINA_HOST_PRESSURE while a guest workload
re-touches memory, ledger-dump on the worker as readout).

### The dev Mac has the graveyard too (second field data point)

Post-legs sweep on the dev Mac: queryable net compressed = 22.0 G vs stored
44.9 G — ~20 G+ unowned here as well, on a box whose HVF exposure is dozens of
short-lived TEST-SUITE VMs daily (each dying, each leaving crumbs) rather than
one long-lived worker. Resident compressor was nearly empty at the time
(occ 596 pages): the entire graveyard sits in swapped segments. Two machines,
same unusual workload (HVF VMs + FRQ REUSABLE), same silhouette. NOTE the legs
themselves also grew this box's swapfiles 17 G→39 G with DEAD-but-unreclaimed
ballast segments (accounting clean, disk deferred — reboot to reclaim; avoid
casual byte-pressure legs).

### Live field instrumentation (running)

A read-only sampler is running on the dogfood Mac
(`/tmp/limina-ledger-sampler.sh`, nohup; kill:
`pkill -f limina-ledger-sampler`): 5-min CSV rows (worker
internal_compressed/internal/reusable/pf + system stored/occ/segments/swap) to
`/tmp/limina-ledger-trace.csv`, hourly `vmmap --summary` attributable to
`/tmp/limina-ledger-vmmap.log`. It discriminates the branches in the field
(ledger+stored ratcheting in lockstep = real slots live; ledger alone = debit
leak) and timestamps the trigger against pressure/reusable-credit slopes.

## Round 5: bench S9 (real-VM reproducer) — NULL at 45-min scale; the swap axis is still unconstructed

S9 (`crates/limina-test/tests/balloon_bench_s9.rs`, run
`spikes/balloon-bench-2026-08-10/s9enh-1786394701`, enhanced 16k, 3..12 G VM,
pool-sized incompressible ballast, 2 G in-guest toucher, 5 cycles): cycle 0
passed the worker-ic gate (ic 0→1.05 G, REUSABLE storm on compressed +
guest-stage-2-faulted pages, then debited clean); cycles 1–4 VOID because the
guest's cold content was already ballooned away (the construction needs a
re-dirty phase between cycles). The net-sweep oracle stayed flat the ENTIRE
run: unattributed 0.018–0.036 G mid-run, 0.137 G at teardown — inside the
~0.2–0.3 G root-slack noise floor. **No orphan reproduction.**

The earlier VOID control run also produced the oracle lesson: raw
stored-after is polluted by bystander processes squeezed during a run (0.67 G
across 606 live tasks, durable, attributed, benign) — the leak signal is
specifically the UNATTRIBUTED delta (`net-sweep.sh`, now bracketing every
bench scenario).

**What S9 still hasn't constructed — the ranked remaining trigger space:**
1. **Swapped-out segments**: S9's whole run had vm.swapusage == 0 — no
   segment ever reached disk, while every observed orphan pool sits in
   swapped segments on swap-full hosts. Next variant: ballast past the
   compressor pool budget (gate on seg_swapped rising) + re-dirty phase.
   POLICY CORRECTION (from the user): the pathological 43 G dogfood run was
   running the LIGHT profile — S9's light choice already matches the field;
   a moderate leg is optional ablation, not field-shaped. NOTE the field
   confound this creates: the new dogfood boot was switched TO moderate, so
   the paired window now watches a different policy than the one that
   produced the pathology — a non-reproducing field trace is ambiguous
   (policy change vs missing pressure episode) until a light-profile capture
   window is run again.
2. Accumulation rate: dogfood averaged ~100 MB/min of excess over 7 h; a
   uniform process would have shown ~4 G in this run. It didn't ⇒ the leak is
   bursty or state-dependent, consistent with (1).
3. Seated-desktop graphics traffic (venus/IOSurface; dogfood carried 2 G
   graphics_compressed) — untestable in this harness; the field trace covers it.
4. Scale (24 G guest vs 12 G).

If the swap-axis variant is also null, the reproducer parks and the dogfood
paired window (balloon-trace.jsonl + ledger sampler, both live since the
reboot) is the discriminator.

## Round 6 (final): the triple constructed on the field profile — FLAT. The reproducer parks

Run `spikes/balloon-bench-2026-08-10/s9enh-1786401350` (940f94f): LIGHT profile
(the pathological field profile), per-cycle cache re-warm in the trough →
ballast until the WORKER's own pages compress (ic gate; peak ic 3.69 G, final
1.79 G) and segments spill to disk (peak seg_swappedout 82,763 ≈ 5 G) → full-room
REUSABLE storm → guest re-faulting. 4/5 cycles non-VOID, swap regime in all 5
storms, 0 OOM. **Oracle: unattributed 0.135 G before → 0.133 G after — flat to
2 MB.** (Mid-run negative unattributed values are the documented churn skew;
bracket samples carry the verdict.)

**FORMAL VERDICT of the reproduction campaign (toy rounds 4–4c + bench rounds
5–6):** every shape we can construct — whole-range and sparse REUSABLE, resident
and spilled segments, host-write and guest-stage-2 re-touch, light full-dumps
and moderate dribbles, compaction survivors, the exact
worker-compressed+swapped+stormed triple on the field profile — **accounts
exactly, in every ledger, every time**. A 30-minute harness cannot reproduce
what accumulated 20–35 G on both long-running hosts. The defect is therefore
gated by something not in the construction space: accumulation over
days/weeks, a rare race (the churn skew shows slots move between segments
under concurrency — a lost debit would need exactly such a window), or an
interaction with a workload class the bench lacks (venus/IOSurface graphics
traffic is the leading one: the dogfood worker carried 2 G
graphics_footprint_compressed).

**The investigation now rests on the field instruments**: the dogfood paired
window (balloon-trace.jsonl + the 5-min ledger sampler, both live since the
post-reboot boot) with `net-sweep.sh` as the system-wide oracle, plus the
light-profile capture A/B on the dogfood VM when the user chooses to run it.
S9 stays one env-var away (`LIMINA_BALLOON_BENCH=1 … balloon_bench_s9`) if the
field ever names a new shape, and its netsweep brackets are now the standard
leak oracle for all balloon bench runs.

### Post-verdict addendum (same day)

- **A uniform per-churn loss is EXCLUDED, not merely unreproduced.** The final
  run churned **21.89 GiB** of ic credit through the worker with a bracket
  delta of |2 MB| → per-churn loss rate **< 0.009%**. The field excess (~21 G
  against ~280 G compression churn) implies **~7.5%**. Three orders of
  magnitude apart under matched mechanics ⇒ the defect cannot be a constant
  leak-per-byte-compressed; it must be gated by workload class, host state, or
  a rare window.
- **First post-reboot field readout (dogfood Mac, ~3 h under moderate): CLEAN.**
  Worker ic balance 5.98 G vs vmmap swapped_out 6.6 G — fully attributable;
  credit − debit = balance exactly (40.77 − 34.79 = 5.98); zero host swap, zero
  swapped segments. And reusable credit is already **117.7 G**, so the REUSABLE
  churn is running at full field rate and accounting perfectly. The ratchet has
  not opened; the "waiting for a pressure episode" framing stands. Snapshot in
  `field-2026-08-10/post-reboot/`.
- **The long-horizon gate is weaker than it looks.** The dev-Mac ~20 G
  graveyard was built by *minutes-lived* HVF test-suite VMs — accumulation
  within one long task can't be the whole story there. What the daily suite has
  that S9 lacks is the **venus/GPU L2 workload**. **Queued next vehicle**:
  bracket one full `cargo xtask test` with `net-sweep.sh` on the now-clean dev
  Mac (~28 min, zero construction). Non-flat → the leak sits in a bisectable
  harness (rerun by test group). Flat → the graveyard evidence needs re-dating
  and graphics loses its best support. (Traps: quiesce other sessions first;
  never cargo-build while the HVF suite runs.)
- **Instrument durability**: the sampler's 5-min cadence keeps its /tmp files'
  atime fresh (safe from the 3-day reaper), but a host reboot kills the watch
  *silently* — on resuming a session, first check the sampler is alive
  (`pgrep -f limina-ledger-sampler`) and re-fetch the CSV into
  `field-*/post-reboot/` for durability.

## Round 7 (same evening, LIVE): the field constructs the missing shape — scan-compressed limbo

The "field readout clean" line above lasted ~40 minutes. At ~1786404193 the
dogfood host came under real memory demand (free fell to ~280 M; guest IDLE —
trace PSI 0, balloon flat at 16.4 G, graphics flat) and the pageout scan
processed the worker's standing REUSABLE stock. Window 1786403893→1786406295
(~40 min, sampler rows in `field-2026-08-10/post-reboot/`):

- reusable balance **7.12 → 1.88 G** (debits +5.73 G; +0.49 G new marking)
- internal_compressed credits **+4.57 G**, debits +0.40 G → balance
  6.02 → 10.19 G; credit − debit = balance exact throughout
- phys_footprint **16.68 → 20.49 G**; internal resident −0.53 G;
  system stored +~3.4 G ≈ all-worker; **swap 0.00 throughout**

**⇒ ~80% of the REUSABLE pages the scan processed were COMPRESSED, not
discarded.** Two readings, identical arithmetic:

1. **Deferred honesty (benign-ish)**: these are the round-4 *limbo* pages —
   marked REUSABLE by the FRQ path, later re-dirtied by guest stage-2 writes
   (production never calls MADV_FREE_REUSE), sitting off-footprint. The scan
   sees dirty → must compress (discarding would corrupt the guest) → the
   hidden pages re-enter the ledger as ic. The pf jump is under-counted
   memory finally being billed, not new consumption.
2. **Wasted compression (pathological)**: the scan compresses *stale* reusable
   pages whose content the guest has abandoned — garbage stored at full cost.

Either way, **this is the shape the entire construction campaign failed to
build**: round 4 found limbo pages "near-immune to the pageout scan" (the toy
could never get the scan to take them); the field just did it, needing real
demand plus ~3 h of LRU aging. And S9 ran the *other* order
(dirty→compress→REUSABLE→refault); this is
**REUSABLE→re-dirty→scan-compress→(future refault)** — the final step is the
open race window: when the guest next stage-2-faults a scan-compressed slot,
is the slot freed (ic debit) or orphaned (the graveyard recipe)? A lost debit
needs exactly this kind of window, and per the round-6 addendum the loss must
be gated — "scan processed limbo under host pressure" is a rare, state-gated
event that a 30-min harness would essentially never hit.

**Also amends round 6**: the dogfood moderate window is NOT quiet — the ratchet
may open under moderate too, just slowly. The light-vs-moderate A/B remains
informative but moderate is no longer a presumed-clean control.

**Natural experiment in flight** (predictions falsifiable from the sampler
alone): (a) ic growth should FLATTEN when the reusable balance exhausts
(~1.9 G left at the last row; pf ceiling ~22 G) — continued ic growth past
exhaustion would kill the limbo explanation; (b) over the following
hours/days, as the guest re-touches scan-compressed pages, ic *debits* should
track — if instead ic balance ratchets while attribution thins, the leak is
being watched live. Then the discriminating questions: does net-sweep stay
flat (still attributed) and does vmmap swapped_out still cover ic?

→ **Prediction (a) CONFIRMED** (peer's closing samples, ~1786407796): ic
growth decelerated 2.7 → 1.5 → 0.46 G per interval; reusable settled at a
~0.56 G churn equilibrium (credits and debits moving together); **pf peaked
at 21.13 G and ticked DOWN to 20.98 G** — under the ~22 G ceiling; swap never
left zero. The re-bill was bounded by the limbo pool exactly as the model
predicts. Prediction (b) — ic debits tracking guest re-touch over the coming
days — is what the standing instruments now watch.

**Queued source check**: xnu `vm_pageout_scan` handling of reusable-marked
dirty pages (does the compress path consult the reusable bit; what frees a
compressed slot when a zero-fill-eligible reusable page is re-faulted) — read
the actual code before theorizing further. → DONE, round 8.

## Round 8 (same evening): the xnu source check — round 7 confirmed benign; a per-pmap double-credit lead REOPENS round 1

Source: `third_party/xnu` (shallow clone of apple-oss-distributions/xnu,
**xnu-12377.1.9** ≈ macOS 26.0 — host is 26.5, minor-version drift possible;
gitignored research clone per convention).

**Round 7 mechanics CONFIRMED in code — the "compress instead of discard" is
deferred honesty, not kernel misbehavior** (mechanics stand; the *framing* is
corrected in round 9 — this is race-conditioned defensive handling, not a
contract)**:**

- `vm_pageout_scan` reads the pmap refmod bits and sets the page dirty if any
  mapping modified it (`osfmk/vm/vm_pageout.c:3570-3581`); then, if referenced
  OR dirty, `VM_PAGEOUT_SCAN_HANDLE_REUSABLE_PAGE` clears the reusable state
  via `vm_object_reuse_pages` (vm_pageout.c:3583-3586, macro at 1572) — this
  is the reusable-ledger debit burst the sampler caught. The compress path
  itself never consults the reusable bit.
- The reusable ledger lives in the **pmap layer**: when a reusable page is
  compressed, the disconnect explicitly credits ic AND phys_footprint with the
  comment "**was not in footprint, but is now**"
  (`osfmk/arm/pmap/pmap.c:5081-5087`) — the limbo re-billing, verbatim in
  Apple's own comment. Dirty limbo pages compress; genuinely-clean ones
  discard. The 80/20 split measures how much of our REUSABLE stock the guest
  had re-touched.

**THE REOPENED LEAD — internal_compressed is per-pmap-MARKER accounting, and
the pv walk credits EVERY mapping:**

- On compression, `pmap_page_protect_options(…PMAP_OPTIONS_COMPRESSOR)` walks
  the physical page's **pv list — every mapping in every non-kernel pmap** —
  and for each internal mapping plants an `ARM_PTE_COMPRESSED` marker and
  credits **that pmap's ledger** with internal_compressed
  (`osfmk/arm/pmap/pmap.c:5040-5110`). Debits happen when a marker is
  replaced (fault-back, pmap.c:6635) or removed (`pmap_remove_range_options`
  counts markers, pmap.c:4389).
- ⇒ **Two internal mappings of one physical page = two ic credits for ONE
  stored slot.** A mapping flagged alt-acct credits ic + alternate_compressed
  together (pmap.c:5074-5077) — self-correcting in the net formula. **This is
  the Parallels/Vz decoder ring with an exact citation**: their guest mappings
  are tagged/alt-acct, ours are not (alt_comp 0.47G on a 45G bill).
- If the hv stage-2 mapping is (a) an xnu pmap using the task's ledger and
  (b) internal-non-alt, then every guest-touched page double-bills ic while
  compressed. **The field ratio was 45.4G billed / 22.3G freed = 2.04 : 1.**
  Under lazy stage-2 population this unifies every result: field ≈ 2:1 (weeks
  of uptime → full stage-2 coverage), round 7 ≈ 1.2:1 (partial coverage —
  cold/ballooned pages lack stage-2 PTEs), churn-probe -H leg NULL (no vCPU →
  no stage-2 faults → no second mapping), S9 ≈ null (fresh VM, the compressed
  set was mostly cold pages). It also reinterprets the mid-run NEGATIVE
  unattributed values we filed as "timing skew": an inflated live net_sum
  exceeding stored is exactly the double-credit signature.
- **Hard caveat**: `PMAP_CREATE_STAGE2` is **0** on arm64 in the public tree
  (`osfmk/vm/pmap.h:726`) — the real stage-2 handling is closed (AppleHV/
  SPTM). Whether stage-2 mappings appear on the pv list with internal
  accounting is NOT confirmable from source. The experiment decides.
- **What this does NOT explain**: the post-death 35G of REAL stored slots
  (round 3). Double-crediting inflates a live ledger; it cannot mint slots
  that survive the task. Either two coexisting defects, or the graveyard has
  a different owner. Keep them separate.

**THE DISCRIMINATING EXPERIMENT (crisp, an afternoon with an existing
vehicle):** extend the trap-probe (see `limina-hvf-graceful` — it already runs
a real vCPU) or churn-probe -H: map a buffer into an hv VM, **touch it from
the vCPU** (populates stage-2), then force compression and read the ledger.
Prediction if the lead is real: ic credit ≈ **2×** the system stored delta;
control (same buffer, no vCPU touch) ≈ 1×. A positive also hands us the fix
lever: ledger-tagged guest memory (alt-acct, the Parallels pattern) makes the
double-entry self-correcting — mechanism already on our fix list for jetsam
semantics.

### Round 8b: the guest-truth reconciliation (user's challenge — "12G live, 21G billed = overbilling")

Correct challenge, and it forces a precision: **"deferred honesty" is xnu-
internal consistency (the kernel bills what is host-DIRTY), not a claim the
content is live.** Host-dirty ≠ guest-live: pages the guest wrote and then
FREED stay dirty at the host until free-page reporting re-covers them — and
Linux FPR only reports batched high-order free blocks entering the free
lists, so low-order/fragmented frees and reuse-then-free between passes are
**never re-reported**. The host retains and compresses stale copies of
guest-abandoned content: dead-but-dirty garbage billed at full price. So the
compressed stock decomposes into (1) phantom double-credit (ledger-only, if
round 8 holds), (2) dead-but-dirty retention (real slots, dead content — the
FPR coverage gap), (3) truly-live tail + worker overhead.

**The round-3 reinterpretation this enables:** the worker billed 45.4G and its
death freed 22.3G of real slots — and 2 × 22.3 = 44.6 ≈ 45.4 (residual ≈ the
0.47G alt-acct + slack). If double-credit is real, the 2.04:1 ratio IS the
whole excess and **this worker may have orphaned NOTHING at death** — the
phantom half evaporated with the ledger, and the 35G graveyard's provenance
reopens (accumulated earlier: weeks of restarts, test-suite VMs, or another
owner entirely). Round 3's "~21G of real slots stayed" inference silently
assumed billed = real; that assumption is now the very thing under test. The
vCPU-touch A/B discriminates (1); with (1) sized, (2) falls out by
subtraction against guest meminfo. Fixes differ per mechanism: (1) →
ledger-tagged guest RAM; (2) → host-page-aware reporting in the enhanced
kernel + balloon policy that keeps the dead-dirty gap small; (3) is the
honest cost of business.

### Round 8c: CONFIRMED BUG (user's synthesis) — we offer the guest pages we told the OS we're done with; fix = unmap released ranges

The user named it, and it is a clear-cut bug independent of every open ledger
question: **on FRQ release we mark the backing pages disposable to xnu
(REUSABLE) while continuing to OFFER those same physical pages to the guest
for writing through the live stage-2 mapping.** The virtio free-page-reporting
spec explicitly permits the guest to reuse reported pages at any time without
notice — trampling is the CONTRACT, not guest misbehavior — so the device side
is obligated to handle the reuse, and we have no path that ever does: a
trampled page sits as live guest data off-footprint (limbo), scan-immune for
hours, then burst-billed under host pressure. Wrong regardless of whether the
double-credit phantom is real or how big the dead-dirty stock is. The ledger
investigation was the detector; the bug stands on its own.

On Linux/KVM this bug cannot exist: MADV_DONTNEED zaps host PTE + KVM stage-2
via mmu notifiers and re-touch faults back in through GUP. macOS has no
notifier for hv_vm_map; REUSABLE-without-unmap was the cheap no-fault path and
limbo is its hidden price.

**The fix (mechanism in libkrun), justified unconditionally:** on balloon/FRQ
release, `hv_vm_unmap` the GPA range (unmap from the GUEST; the worker-VA
madvise stays as today) — guest re-touch → data abort → VM exit →
`hv_vm_map` the surrounding chunk back. **MADV_FREE_REUSE is NOT required, and there is no
bookkeeping to skip (user's model, adopted as THE design mental model):**
nothing ties guest pages to the specific physical pages that used to back
them. The only association we hold is GPA ↔ worker-VA — fixed arithmetic,
not state. VA ↔ physical belongs to the OS at every moment. So
FREE_REUSABLE on a released range means "freed; forget them": the OS
discards at its leisure (that IS how they return to it), and on the guest's
next touch it backs the VA with whatever pages it pleases — same or fresh,
neither we nor the guest can tell. Calling REUSE would require REMEMBERING
released ranges just to annotate the ledger sooner than the scan would
anyway — inventing bookkeeping in order to delegate it. Complete design:
release → hv_vm_unmap + FREE_REUSABLE + forget; fault → map chunk → OS
supplies pages. Stateless beyond the fault handler. (Release-TIME safety
stays: the 16 KiB coalescing filter that only madvises fully-guest-free
host pages — a check at release, not state that outlives it.) Re-touch
safety is xnu's own contract — re-dirtied pages are never discarded,
re-billed lazily at scan (round 7 WAS that reconciliation) [→ OVERSTATED,
corrected in round 9: survival is the guest winning a race against the scan,
and hinges on stage-2 dirty-bit visibility]; discarded-first
pages read back as zeros the guest forfeited. For classic balloon-INFLATED pages the unmap is bonus
hardening: the guest must not touch those, so a fault becomes a true bug
detector. Data-abort handling in the vcpu loop is proven (trap-probe
vehicle). **Doing it right, not gating it**: light-profile full-room re-fill
= a stage-2 fault storm, so remap must chunk and the S9 scaffolding measures
the overhead. **What it does NOT fix** (still real, separately tracked):
never-reported low-order frees (dead-dirty stock outside the reported set)
and double-credit on in-use compressed pages (→ ledger-tagging; the fixes
compose). The vCPU-touch A/B is DEMOTED from gate to sizing instrument: it
answers whether tagging is also needed and reinterprets the graveyard — the
unmap fix proceeds regardless.

**Code-history check (user asked "did we change this recently?"): NO.** The
balloon device was last touched 2026-07-20 (DEFLATE_ON_OOM advertising knob)
and 07-18 (F_REPORTING masked by default — the s2idle freeze oops; enhanced
VMs re-enable per-VM). The REUSABLE-only design dates to June (bd908c8), and
there are ZERO MADV_FREE_REUSE calls in the device — never were. The 50G
pathological run and the clean post-reboot boot ran the same code. Bonus
found while checking: TWO REUSABLE producers exist (FRQ reporting AND
balloon inflate, device.rs:340/:413). For the inflate half we already HAVE
the reuse event — deflate goes through our queue handler — so prompt
re-billing there is a one-liner today, no unmap machinery; only FRQ needs
the unmap+fault to get any event at all.

### Round 9 (2026-08-10): the race-model correction (user's) — the scan keeps a trampled page by LUCK, not contract; fundamentals restated

**The challenge**: rounds 7–8c drifted into treating xnu's scan behavior as a
guarantee ("deferred honesty", "re-touch safety is xnu's own contract —
re-dirtied pages are never discarded"). The virtio FPR spec cannot override
xnu semantics — it binds guest↔device expectations only, and obliges xnu to
nothing. After `MADV_FREE_REUSABLE`, what xnu promises *us* about those
frames is NOTHING.

**What the source actually says** (re-read against apple-oss-distributions/
xnu `main`; same line numbers as the round-8 xnu-12377.1.9 read):

- The scan's keep-or-discard decision is a **race-time inspection**, not a
  standing rule: `pmap_get_refmod` (a pv-list walk over the physical page's
  mappings) at `vm_pageout.c:3572-3581`, and only if referenced OR dirty does
  it invoke `VM_PAGEOUT_SCAN_HANDLE_REUSABLE_PAGE` (3583-3586; macro at
  1572) — whose own comment reads: *"If a 'reusable' page somehow made it
  back into the active queue, it's been re-used and is not quite
  re-usable"* — a **"rogue" page**. Touching REUSABLE memory without
  `MADV_FREE_REUSE` is off-protocol from xnu's side; keeping the page is
  defensive handling of a squatter, not a return policy.
- **Two race branches, decided purely by timing:**
  1. Guest touch lands first → scan later finds dirty → keeps + re-bills.
     This is everything round 7 watched ("limbo re-billing"). The guest
     **won the race** — that is the only reason we still hold the frame.
  2. Scan lands first → the frame is **freed and goes to whoever needs a
     page next** — another process, file cache, anyone. The guest's later
     touch stage-2-faults inside the closed AppleHV path and receives a
     zero-fill page; we observe none of it.

  "The scan gave the page back to us" was always branch 1 observed from the
  inside. Nothing earmarks the frame for us after REUSABLE; re-billing is
  the *outcome of the guest winning*, not the OS returning our page.

**The residual hazard this makes explicit**: branch 1's safety hinges on the
guest's stage-2 write being **visible to `pmap_get_refmod`** (stage-2
mappings participating in the pv list). Evidence says it holds — round 8's
2.04:1 per-mapping ic billing is exactly that signature, and field guests'
RAM demonstrably compresses and swaps, which requires the same visibility —
but the stage-2 pmap is closed (SPTM; `PMAP_CREATE_STAGE2` = 0 in the
public tree, round 8's caveat), so this is inference from behavior, not
readable source. If visibility ever failed, the scan would see
clean+unreferenced and **discard live guest data — silent corruption**, not
an accounting bug. The round-8c unmap fix removes the dependence on this
unverifiable assumption entirely: after `hv_vm_unmap` there is no live
mapping through which a rogue page can be minted.

**FUNDAMENTALS RESTATED (user's, adopted)**: this campaign went deep enough
into ledger forensics to briefly lose sight of the basics. Even the
*mildest* branch of REUSABLE-with-live-stage-2 — no corruption, no
double-credit, no graveyard — is live guest data sitting in **limbo**: off
phys_footprint, invisible to every accounting view, scan-immune for hours,
then burst-billed under host pressure. That alone is already really bad and
justifies the unmap fix with no further ledger question answered. The
double-credit and graveyard threads stay open as *sizing* questions
(vCPU-touch A/B, ledger tagging); none of them is a gate.

## Operational consequences (true regardless of mechanism)

- jetsam acts on phys_footprint, so the worker looks ~19 G bigger to the killer
  than the memory it actually holds. The double-charge only exists for COMPRESSED
  guest pages — keeping the host from compressing guest RAM (balloon squeeze +
  FRQ, i.e. the reclaim-policy thread) shrinks the inflated number, not just the
  real one.
- If the controlled spike confirms per-mapping double-billing, that's an xnu
  accounting bug → Apple feedback with the minimal repro; we cannot patch the hv
  kext side.

## Round 3 (same day): the restart experiment — the excess is REAL, ORPHANED, and OUTLIVES the task

Sequence of read-only checks, each one narrowing the branch:

1. **System-wide compressor cross-check** (`vm_stat` "Pages stored in compressor",
   uncompressed accounting): 5.6 M pages = 85.6 G — comfortably above the worker's
   43.5 G bill, and self-consistent with real storage (23.6 G resident compressor +
   24.9 G swapped segments = 48.5 G at 1.76:1). No quick kill for either branch.
2. **Full 68-entry ledgers of every VM process on the box** (worker, Parallels
   `prl_vm_app`, a Virtualization.framework VM) found the decoder ring:
   `alternate_accounting_compressed` + `tagged_footprint_compressed`.
   **Parallels and Vz are exonerated**: their guest memory is ledger-*tagged*
   (the `mach_memory_entry_ownership`-style API), double-entered as
   internal_compressed + tagged_footprint_compressed with alternate_accounting_compressed
   as the anti-double-count correction — xnu's footprint formula subtracts it. Net bills
   match their guest sizes (prl: 12.43 G gross − 12.20 alt + 5.12 tagged ≈ legit for its
   guest; its raw internal_compressed even exceeds its whole 9.6 G writable VA — gross
   values are NOT existence-bounded, net values are). **Our worker is un-tagged**
   (alternate 0.47 G, tagged 0): its 43.6 G is NET and the existence bound stands.
3. **The suspect list re-inverted**: Parallels churns compression at our scale
   (280 G credit) with **no pathology and 0.076 G lifetime reusable** — a natural
   control that puts `MADV_FREE_REUSABLE`-on-compressed (FRQ; our reusable credit
   is 430 G) back as prime suspect.
4. **The restart** (user-initiated, worker pid gone, measured 58 s apart):
   - stored: 5,347,723 → 3,883,655 pages = **−22.3 G — exactly the attributable
     amount, NOT the 45.4 G billed** (43.57 internal_compressed − 0.47 alt +
     2.06 graphics_c + 0.29 purgeable_c).
   - The drop was almost all resident compressor (occupied −14.8 G ≈ 22.2 G
     uncompressed at ~1.5:1); **swap barely moved** (used −0.33 G; swappedout
     segments −6,848 ≈ 0.4 G).
   - **Post-death sweep of every queryable task** (net formula: internal_compressed
     − alternate_accounting_compressed + tagged_footprint_compressed +
     purgeable_nonvolatile_compress + graphics_footprint_compressed): **22.7 G
     accounted vs 57.7 G still stored** → ~**35 G of compressed data owned by no
     queryable task**, parked in 23.3 G of swapped-out segments. The swapfile shows
     25.1 G used with NO VM running.

**Reading**: the worker's ~21 G excess was real compressed slots that had ALREADY
detached from the guest-RAM object (hence invisible to every attribution view)
while still billing the task's ledger — per-slot billing follows the owner ledger
until the slot is freed, and these slots are never freed. At task death the object's
live slots released cleanly; the orphans stayed, and with the ledger gone they became
unattributed system-wide garbage. Accumulated across VM restarts, this is why the
26 G swapfile is chronically full: it is a graveyard of leaked compressor slots that
only a host reboot reclaims (open question: does major compaction EVER collect them —
worth watching stored/swapusage on the idle box).

**Sharpened toy-spike signature** (better than lockstep-watching): run
dirty → compress → REUSABLE → re-dirty cycles, then EXIT the process — if
system-wide stored does not return to the pre-toy baseline, the leak is reproduced
and the residue measures it. Control legs (no-REUSABLE; MAP_PRIVATE) must return
to baseline. Predicted effect size if REUSABLE-on-compressed is the trigger:
~buffer-size of orphan per cycle, unmistakable vs noise.

**Fix directions** (after the toy confirms the trigger): (i) adjust the FRQ/balloon
madvise pattern in libkrun to avoid the leaking transition; (ii) adopt ledger-tagged
guest memory like Vz/Parallels — correct accounting semantics for jetsam either way;
(iii) Apple feedback with the minimal repro (the leak itself is xnu's, unpatchable
from our side).

## Tool notes

- `ledger(2)` works unprivileged against same-uid tasks on macOS 26.5.2; 68
  entries in the task template. No task port needed (this is why it sees what
  footprint(1)'s region walk cannot).
- Entry set worth re-checking per OS release (the "re-confirm OS-specific
  behavior" rule): `swapins` (1.07 G here), `est_reclaimable`, and the
  tagged_* entries were absent/zero on this run.
