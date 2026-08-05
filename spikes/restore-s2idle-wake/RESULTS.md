# restore-s2idle-wake — CORRECTED: the wake was never broken; snapshot resume loses the compositor's vrend GL world (black window, both tiers)

**2026-08-05, two rounds.** Round 1 (morning) concluded "snapshot resume wedges on BOTH
tiers — the wake IRQ loses its wakeup-armed handling". **That conclusion is RETRACTED**: it
rested on an unverified premise about which suspend path the ad-hoc flow exercised. Round 2
(same day, production path, both tiers) found the real product bug, which is entirely
different. Both rounds are recorded here; read round 2 for the truth.

## The premise that fell

The round-1 flow suspended with `kill -USR1 <worker>`. **SIGUSR1 is the RAW snapshot seam**
(`crates/limina-vmm/src/snapshot.rs`): pause vCPUs, dump a *running* guest — the L1 test
vehicle, explicitly documented as "no thaw ⇒ nothing revives the transports". The
**production path is SIGTSTP** (`limina suspend` → supervisor → worker
`crates/limina-vmm/src/bracket.rs`): pulse the guest suspend button → wait for s2idle
quiesce (`is_quiesced`: every virtio device reset to INIT) → save → exit 126. It "NEVER
snapshots a non-quiesced guest".

So round 1 restored snapshots of guests that were **never suspended**. Every "contradiction"
it found is the expected state of a running guest:

- Wake IRQ serviced as a plain gpio-keys ISR (PL061 MIS→DATA), no wakeup-armed abort —
  nothing was armed; the guest wasn't in s2idle.
- The snapshot-carried pending IRQ serviced by the virtio-GPU ISR "inside s2idle" — device
  IRQs were never suspended.
- All six vCPUs at one WFI pc — the ordinary cpuidle idle loop of a seated desktop, not the
  s2idle loop (same-looking pc; the identification was assumed, not symbolized).
- The supervisor showed no "Suspending…" overlay (task #18 item 1) — the overlay keys on
  `supervisor::suspend_requested()`, and the supervisor never saw a suspend request.

The in-place s2idle A/B ("wake works in place") compared a genuinely suspended guest against
a never-suspended one — it discriminated nothing about restore. The 2026-08-03 pilot memo's
ad-hoc wedge was very likely this same raw-seam misuse, not a config mismatch and not the
round-2 bug below (unknown which flow it used; moot either way).

Lesson (again): **enumerate and verify premises before deep-diving.** "The guest quiesces
via s2idle" was written into the round-1 dossier from the design docs, not from the log. One
`grep SIGUSR1 crates/limina-vmm/src/` would have killed it in a minute.

## Round 2: the production path, both tiers — the REAL bug

Flow per tier: clone image → windowed EFI+venus boot (`--ssh-port 2299 --snapshot-file …`)
→ seated GNOME + live workloads (stock: glmark2 + Firefox WebGL; enhanced: + vkmark), human
oracle confirmed all animating → `kill -TSTP <supervisor>` → bracket: button pulse → guest
quiesced in ~250 ms → 3.5–3.8 GB snapshot → exit 126 → relaunch same command →
auto-restore (`take_pending_resume`) → census + human oracle.

What works (both tiers):

- Suspend UX: window dims + "Suspending…" overlay (human-confirmed) — #18 item 1 is
  **ad-hoc-flow-only**, not a product bug.
- Bracket, save, restore, wake: guest resumes s2idle correctly. Same boot_id, uptime
  continuous, **gnome-shell + every workload process survives**, zero new coredumps.
- Enhanced: the venus journal replays a real world (28.4 MB payload, 460 ms, 33 benign
  stale-reference drops).

What is broken (both tiers): **the window stays black forever** ("Resuming…" never lifts —
zero presented frames). Host worker log at the same time:

- `CmdSubmit3d → ErrRutabaga(ComponentError(22))` storm — **4.5 M rejections in ~8 min**
  (stock), ~100 k (enhanced, shorter observation).
- `SetScanout → ErrInvalidResourceId`, `ResourceFlush → ErrInvalidResourceId` — the guest
  flips to framebuffer resource ids the restored renderer never heard of.
- Stock restore staged a **6 KB** GPU re-creation payload for a whole seated desktop (vs
  28.4 MB enhanced) — the journal had essentially nothing for this guest.

### Root cause

**The M9.3 retain-and-replay journal covers venus (vkr) contexts only. Since the 2026-08-04
GL-ladder flip ([[limina-baseline-3d-plan]]: GL = virgl/vrend on BOTH tiers, guest zink
dropped), the compositor and all GL clients run on classic vrend contexts — which are not
journaled, not captured, and not replayed.** Restore rebuilds an empty vrend world; the
pre-snapshot contexts/resources are gone. Classic virgl submits are fire-and-forget, so the
guest never learns: gnome-shell keeps compositing into the void (DRM says display On/enabled,
`Created gbm renderer` — the shell maps stock `gallium-*.so` virgl, verified), glmark2 keeps
pumping frames, and every submit/flip dies host-side. No client aborts — unlike venus, vrend
has no dead-ring ~17 s abort — so "the session survives" while every pixel is lost.

When M9.3 shipped (2026-07), the compositor ran zink→venus, so the venus journal genuinely
covered the desktop; the GL flip silently un-covered it. The enhanced tier's 28.4 MB venus
replay is real but only carries the Vulkan-side clients (vkmark etc.) — which now composite
through a dead vrend compositor.

### Why every L2 is green

`venus_session_preserved` asserts boot_id / same-shell-PID / no-coredumps / venus-client
liveness, and its header explicitly defers visual fidelity ("P2's gate"). All of those pass
in the black-window state — process survival is exactly the thing this bug does NOT break.
**The missing oracle is a post-restore PIXEL check on the windowed present path** (and/or a
host-side assert on the rejected-submit storm: 4.5 M ComponentError(22) in minutes is not
subtle).

### Fix directions (task #19)

1. **Extend retain-and-replay to classic vrend** (mirror of the venus design: virgl wire
   commands are guest-id-keyed, so replay is id-faithful; record per-context classic streams
   + resource creates with latest-wins pruning). Key observation for scope: **live desktop
   clients redraw continuously**, so a *structural* replay (contexts + resources + objects,
   without full GL-object content capture) likely self-heals visually within one frame cycle;
   content capture may only matter for the scanout until the first full redraw.
2. Alternatively/complementarily: a guest-visible "GPU reset" signal forcing clients to
   re-create GL state — but stock mutter/cogl robustness handling is doubtful; violates the
   smallest-change ladder. Not the first choice.
3. Interim: with resume black on both tiers, M9.4's default `on_window_close = suspend` for
   managed VMs is a dogfood footgun (a close → suspend → resume → black). Decide with the
   user whether to flip the default or gate on the fix.

### Also retracted

Round 1's "NetWorker busy-spin post-restore" was a sample-filtering artifact — the thread's
leaf is `kevent` (idle) in both tiers' samples. The stock worker's 240% CPU = vCPUs running
glmark2 + the gpu worker churning millions of rejected submits.

## Artifacts

- `stock-prod-resume-netspin-sample.txt` — worker `sample` during the stock black-window
  state (the name predates the retraction; the net thread is idle in it).
- `sr2-enh-sample.txt` (scratchpad only) — enhanced-leg equivalent.
- `stock-prod-resume-err-counts.txt` — 4,525,062 ComponentError(22) / 65,824
  ErrInvalidResourceId at teardown.
- `stock-prod-resume-worker-lines.txt` — bracket/restore/replay log lines, stock leg.

## Repro (production path, either tier)

    cp -c Fedora-Workstation-44.accessible.raw /tmp/sr.raw    # or enhanced.raw
    RUST_LOG=info LIMINA_DISK=/tmp/sr.raw \
      LIMINA_EXTRA_ARGS="--ssh-port 2299 --snapshot-file /tmp/sr.snap" \
      spikes/venus-draw-probe/boot-enhanced-efi-kk.sh         # wait seated, start workloads
    kill -TSTP <supervisor pid>                                # bracket → save → exit 126
    # relaunch the same command → auto-restore → guest fine over ssh, window black,
    # worker log: CmdSubmit3d ComponentError(22) storm + SetScanout ErrInvalidResourceId
