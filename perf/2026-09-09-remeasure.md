# 2026-09-09 — virglrs `d30b8ce`: the stutter is fixed, and the battery cannot score it

limina `c4cdab53` / virglrs `d30b8ce` / libkrun `7c4ada05`. F44 enhanced CoW clone, 4 vCPU /
4 GiB, display **verified** `Virtual-1 1280x800 @ 1.0`, guest `7.1.8-limina16k.4` (16 KiB pages) /
mesa `26.1.8-11`, Firefox 150.0. Zero `refused: vrend`, zero poisoned contexts across the whole
session. Method: [`limina-profiling-playbook`]. Rows in `perf/ledger.csv`; raw stdout and fps crops
in `perf/evidence/2026-09-09/`.

## The result

**The mouse-pointer stutter under a heavy GL workload is gone.** Human verdict at 25 000 fish —
the same load, guest, display pin and human that produced *"still stutters as badly"* on 2026-09-08:
**"Smooth now."** The symptom has no counter and this is the only instrument that can score it.

Its cause was three stacked drains, all now fixed:

1. **The renderer compiled `-O0`.** virglrs is a path dependency compiled into the worker, so it
   inherits the worker's profile, and **nothing in any build output says which profile a compiled-in
   renderer got.** Fixed by `[profile.dev.package.virglrs] opt-level = 3`.
2. **A `glFinish` of every context on every classic fence** (`create_fence -> finish_all`, 75% of
   the `gpu worker` thread). Fixed in virglrs `4afa3ef`.
3. **`resource_sync_iosurface` calling `finish_all()` on the compositor's present** — every context,
   every sub-context, per page-flip. Fixed in virglrs `7c65f0f`, which finishes only the surface's
   own contexts.

A fourth, in the same family: **`[profile.dev.package.virglrs]` does not reach virglrs's
dependencies.** Fixed in `c4cdab53`. It still does not reach `krun_rutabaga_gfx`, which instantiates
37 out-of-line `rustc_hash` symbols at `-O0` on the path between the guest command and the renderer.
That is a count, not a cost — unmeasured, and named here so it is not mistaken for cleared.

## Numbers

**vkmark, guest idle** (n=3, distro `vkmark-2025.01-3.fc44`, `-s 1280x720`):

| pin | runs | median |
|---|---|---|
| 08-08, C renderer | 3146 / 3151 / 3155 | 3151 |
| 09-08, virglrs `c299aae` | 3980 / 3981 / 4051 | 3981 |
| **09-09, virglrs `d30b8ce`** | 4146 / 4201 / 4215 | **4201** |

Stated in its strongest available form: **the 09-08 and 09-09 bands do not overlap.** Not as a
percentage — see *Instruments* below for why vkmark does not support one.

**vkmark under load — a new workload, first of its kind, and the one future work will be measured
against.** With the aquarium at 25 000 fish running concurrently: **1450 / 1439** (0.8% apart).
This is the condition the user reported — vkmark at 30-45 fps under a heavy GL workload before this
work. It is **not comparable to any `vkmark-default-venus` row** —
those are all guest-idle, and the distance from 1450 to 4201 is contention, not a regression.

**WebGL aquarium** (n=3 single captures each, distinct `bright_frac`):

| fish | runs | median | 08-08 C |
|---|---|---|---|
| 25 000 | 45 / 51 / 43 | **45** | 42 |
| 30 000 | 38 / 43 / 39 | **39** | 39 |

25k clears the C renderer; 30k is parity. The "residual ~1.2x vs the C era" recorded on 09-08 is
**gone rather than explained** — which is the better outcome, and means the subtraction that produced
it should never have been quoted.

**Ledger battery** (n=4 clean; a fifth run was discarded, see *Discipline*):

| workload | median | spread | 08-08 C |
|---|---|---|---|
| `gl-replay-venus` (fps) | 56.93 | 0.2% | 47.60 |
| `gl-replay-llvmpipe` (CPU control) | 719.42 | 2.3% | 746 |
| `vk-replay-venus-headless` (fps) | 2175.43 | **13.2%** | 1974.7 |
| `glmark2-wayland-venus` (score) | 3054 | 1.0% | 2944 |

**None of these four resolves anything measured here.** That is the pass's second finding.

## Instruments: the battery cannot score the work it is being run on

Our battery was assembled to catch **large regressions**. It is being used to attribute **small
improvements**. Those are different jobs, and four of five instruments fail the second one — each
silently, and three of them while looking trustworthy.

| instrument | resolution floor | how it fails |
|---|---|---|
| `glmark2-wayland-venus` | **±10% between boots** (±1-3% within) | looks precise |
| `vk-replay-venus-headless` | **one low outlier per set, 9-15%** | looks precise |
| WebGL aquarium | **~15%** | looks precise |
| `gl-replay-venus` | insensitive at any precision | looks *stable* |
| vkmark | 0.3% to 8.3% **depending on condition** | looks fixed |

- **`glmark2`'s ±10% was already recorded** (`perf/2026-07-27-replay-regression-ab.md`, with the
  explicit rule *"never attribute from single-boot glmark2 deltas"*) and was not applied on 09-08.
- **`vk-replay` drops one low outlier in every multi-sample set in the ledger** — `b671482` 1701
  among 1927-2031; `ce19a95b` 1607; `b095253d` 1679; `095e2856` 2154; `c4cdab53` 1907. With n=3 you
  either catch the outlier or you don't, and **if you don't, the surviving pair looks like a tight
  instrument.** That is not a noisy estimate of the spread, it is a confident and wrong one, erring
  toward being believed.
- **`gl-replay-venus` has never moved for anything** — 56.9 across three pins and thirteen runs
  spanning a 5-6x renderer change. It runs `eglretrace --headless` (`perf-ledger.sh:120`) and so
  never presents; something guest-side, most likely zink's GL→Vulkan translation, binds it.
  **Its ±0.2% is the cleanest number in the ledger and the least informative.**
- **vkmark's precision is condition-dependent**, not a property of the instrument: 0.3% at 08-08,
  8.3% at the `-O0` pin, ~1.7% at the healthy pins. "vkmark is ±0.2%" was itself a figure carried
  from one condition to others.

That the battery cleared the `-O0` → `-O3` step (+28% on `vk-replay`) and the profile regression
(−23% on `glmark2`) is **not evidence it works.** It is evidence those effects were large enough to
clear a floor nobody had measured.

## The rule this pass exists to record

**A cost measured behind a larger cost is not measured**, and its general form:

> **A measurement that cannot respond to the treatment reports a confident null.**

Every anomaly of the last two days is an instance. The `-O0` renderer saturating whatever sat behind
it. `resource_sync_iosurface` profiling at 0.1%, and again at 0.7-3.0%, only because a larger drain
sat ahead of it — it went 12 → 460 samples once that drain was removed, and the human verdict above
is the positive evidence it was real. A needle pinned at 56.9. Instruments whose needles do move,
but by less than their own noise. The one case that came out the other way is the shape to copy:
`f00ec03` refused to conclude until its control armed, and then it did.

**Operational form — before believing a null, name the result that would have shown the effect, and
check the instrument has produced a result of that size for a known cause.** vkmark passes (2140 →
3151 → 3981 for known causes). `gl-replay-venus` fails, and always has.

**Tightness is not that check.** Stability and sensitivity are independent properties, and an
instrument chosen only for the first is a dead needle: it delivers its null with confidence.

## Discipline

- **Only the control is entitled to say the host was quiet.** One run was discarded outright — not
  labelled — because `gl-replay-llvmpipe` read 630.6 against a healthy 717-734 while two 1052 MB GPU
  replays from another session overlapped it. Contention that is GL-heavy distorts the *shape* of a
  comparison and not only its level, so a labelled row would still be read as a sample. Both
  sessions had checked "is a VM running" and called that "is the host idle". The control knew first.
- **Spotlight types our disk images as camera RAW.** `mdls` reports `kMDItemContentType =
  "com.panasonic.raw-image"` for a 40 GB `.raw`, so image importers run over it — on a fresh `cp -c`
  clone and on writes. Observed as a burst here that settled without moving the control, so it is a
  hazard observed, not a contamination measured. Mitigation needing no system change: clone into a
  directory whose name ends in `.noindex`.
- **Do not ask a human to exercise the pointer during a measurement run** — a 09-08 reading was
  discarded because the capture caught the GNOME overview open. Captures first, then the human.
- Per-scene vkmark is now recorded (`perf/evidence/2026-09-09/vkmark-run{1,2,3}.txt`). Every
  historical vkmark row is an aggregate, in which a real effect can cancel against a flat one.

## Attribution, and what stays open

**The +5.5% window is 22 virglrs commits (`c299aae..d30b8ce`) plus limina's `c4cdab53`**, and this
pass does not narrow it further. `c299aae` predates the fence fix `4afa3ef` by 7 commits, so the
whole fence rework, `7c65f0f`, and the command-path work are all inside it. Two facts do bound it:

- **virglrs `886f0d8` cannot contribute.** It adds `[profile.dev]` to virglrs's own `Cargo.toml`,
  and **Cargo ignores `[profile.*]` in any non-root package** — only the workspace root's profile
  applies. It governs virglrs-as-root builds only.
- **`c4cdab53` was not in the 09-08 build.** That vkmark leg ran 20:39 local; the commit landed
  23:32 local.

`7c65f0f` is the mechanism most likely to explain a *guest-idle venus* gain: vkmark is a windowed
Wayland client, so it presents, and the fix removes `finish_all()` work from the gpu worker thread
venus queues behind. **The gain is banked, not attributed** — no separation run. These are not the
right instruments for fine attribution, and the environment control that would make them so does not
exist yet; the large changes are what this pass is for. If it is ever wanted, `7c65f0f` against its
parent `f00ec03` with vkmark n=3 is the run, because vkmark can resolve it and nothing else can.

**Correctness of the command path is scored: rs == c, byte-identical, nine classic corpora**
(virglrs's harness), each leg replaying its corpus in full. Its own boundary: that proves *no
regression against the C*, not that either leg is right.

The route to it carries a rule worth more than the result. An apparent divergence on `vrend-webgl`
(45 343 of 82 916 commands, `failed 0`) was first cleared as "environmental" because **both legs
lost the same half identically** — and that reasoning was wrong. Both legs had been handed the same
wrong invocation: `vrend-replay` replays one virgl context by default, and the fixture was recorded
with `--ctx 2,9`. With the flag, both legs replay all 82 916 and match the pinned fixture exactly.
**A control that both arms share is not a control**: two identically-misinvoked runs agreeing with
each other says nothing about the thing they were supposed to be testing. The output had announced
it — `replay: no --ctx given, picking ctx 2 (45343 commands)` — and was read past while an
environmental theory was constructed to explain the number that line was naming.

**The fence fix's ordering hazard is scored.** `vrend::waiter::tests::
a_cpu_reader_sees_the_render_the_fence_waited_for` (virglrs `f00ec03`) passes, and **its control
armed at the first size tried** (`FIRST_PASSES = 400`): the unwaited read came back `0x00`, the
waited read `0xff`, the colour the render wrote. The needle was demonstrated live before the
assertion meant anything — which is what makes the pass evidence rather than a dead needle. Real
stack, not a fallback: KosmicKrisp ICD, `render_pass_starts=1`, a Metal render pass.

It scores the *mechanism* — an unordered consumer (`IOSurfaceLock` + load) reading a surface a GL
context rendered into, with the wait taken as the waiter takes it, on a second context of the share
group on another thread. That is the relationship a venus compositor has to a vrend client's render.
It is not a synoik boot and says nothing about that compositor as a whole.

**Still owed:** the multisample cap with a positive control; the `IOAccelerator (graphics)`
closed-to-closed ratchet (the 08-08 regression is neither confirmed nor cleared); host wakeups; boot;
disk; memory floor; the zinkvenus aquarium arm (Firefox does not launch under the zink env in the
benchmark unit — probe `eglinfo` under that unit's env first). A `krun_rutabaga_gfx` opt-level A/B,
with a positive control.
