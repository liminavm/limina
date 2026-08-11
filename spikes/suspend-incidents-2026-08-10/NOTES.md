# dogfood-mac / dogfood-guest suspend incidents — evidence bundle (2026-08-10, gathered read-only ~18:00)

Three distinct issues from the user's report, all reconstructed from logs. All times local (-03).
VM: Dev.liminavm on dogfood-mac, 10 vCPU / 24G, `on_window_close = "suspend"`, `on_host_sleep = "s2idle"`,
window fullscreen 3840x2160 (state.toml), notch=extend.

## Timeline (correlated host + guest)

- 15:33:31  guest boot -2 ends in a clean systemd POWEROFF (unrelated pre-story restart; vm.toml mtime 15:39)
- 15:39:59  guest boot -1 starts (normal boot from control center)
- 16:18:55  **USER SUSPEND** (the only suspend in boot -1): guest logs `systemd-logind: Suspend key pressed
            short` → `The system will suspend now!` → `PM: suspend entry (s2idle)`; journal freezes here.
- 16:19:0x  splash.png written to run/ — **the captured park splash is itself HALF-BLURRED**
            (top ~43% blurred wallpaper with no windows, bottom ~57% pixel-sharp: terminal with readable
            `ps` output + fern wallpaper). Copy: `splash.png` / `splash-small.png` in this dir. This is
            ISSUE 1 — the blur is baked into the capture, not a live-overlay artifact.
- ~16:19    user clicks the window close button; UI shows "resuming" (close-while-suspended resumes first,
            per on_window_close=suspend + parked state).
- 16:19:25  **ISSUE 2 — limina-vmm (pid 69310) SIGSEGV**, EXC_BAD_ACCESS KERN_INVALID_ADDRESS at 0x60
            (null+0x60), thread 23 "gpu worker":
            KK `end_subpass` ← `vk_common_CmdEndRenderPass2` ← `vk_common_CmdEndRenderPass`
            ← `vn_dispatch_vkCmdEndRenderPass` ← `vkr_context_submit_cmd` ← `vkr_renderer_replay_submit`
            ← `render_state_limina_replay_submit` ← `virgl_renderer_limina_replay_submit`
            ← `VirtioGpu::restore_gpu_payload` ← `Worker::work`.
            i.e. the venus journal REPLAY during resume-from-park hit CmdEndRenderPass with null render-pass
            state. `suspend-bracket` + `host-sleep` threads present; vCPUs in wait_for_interrupt.
            Full report: `limina-vmm-2026-08-10-161925.ips` (also on dogfood-mac
            ~/Library/Logs/DiagnosticReports/). The UI stayed on "resuming" with a dead worker — no
            crash surfaced to the user — so they quit the app.
- 16:25:59  dogfood-mac HOST REBOOT (kern.boottime; presumably the user, per the hv-ledger reboot plan)
- 16:27:25  guest boot 0 — normal boot from control center (user's "normal boot" observation)
- 16:40:54  (incidental) WebKitWebProcess SIGSEGV coredump in guest — probably unrelated, noted only
- 17:41:02  dogfood-mac display off → host idle sleep. Supervisor log at 17:41:21-22 shows a storm of
            `update virtio queue in invalid state 0x0` for net/vsock/block/input/rng/i2c/balloon
            (guest device-freeze during s2idle entry). Guest kernel: `PM: suspend entry (s2idle)` 17:41:22.
- 17:44:41  host wake (display on 17:44:41). **ISSUE 3** unfolds:
            - supervisor 17:44:41: `gpu: DRIVER_OK with no queues programmed this cycle (no-PM-ops driver
              resume); re-armed queue register file from the previous activation`
            - supervisor 17:44:41: `balloon: The device is not yet activated. Spurious event received: 78`
            - supervisor 17:44:42: `gpu worker: reset drain timed out after 1.5079265s` →
              `device reset with the session NOT quiescent (fences: 0 outstanding; present: 0 parked,
              1 parked-flush-cookies, 0 guest-holds, 13894 awaiting-shown, 0 retired-unprocessed) —
              wiping (fail-closed)` → ALL GPU contexts destroyed (synoik ctx 3+5, ghost, Xwayland,
              firefox, chrome…).
            - guest kernel 17:44:42: `PM: suspend devices took 0.873s` / `resume devices took 0.164s` /
              `suspend exit` — the guest completed its s2idle entry AND exit at wake time (3m20s after
              entry began; vCPUs were paused mid-suspend when the host actually slept).
            - guest kernel 17:44:43+: `[drm:virtio_gpu_dequeue_ctrl_func] *ERROR* response 0x1200/0x1203
              (commands 0x207, 0x201, 0x209, 0x102)` — every guest GPU op refused.
- 17:44:58  **synoik (pid 2855, /usr/local/bin/synoik --session) SIGABRT**, coredump present
            (`coredumpctl info 2855`, 31.9M zst): venus ring dead → `vn_relax` / `vn_ring_wait_seqno`
            / `vn_ring_submit_command` abort under `vn_CreateImage` ← ash `Device::create_image`
            ← `VulkanRenderer::create_buffer` ← `LockScreen::render_prompt` (it was rendering the
            lock-screen prompt after wake). systemd restarted it 17:45:09 (current pid 238181).
- 17:45:01  host worker logs the post-mortem: gpu journal 0 live ops (recorded=66318 pruned=66318),
            rutabaga 0 contexts; CTX_DESTROY/ResourceUnref storms with InvalidContextId (guest cleaning
            up contexts the host already wiped).

## The three issues, distilled

1. **Half-blurred park splash** (cosmetic, capture-side): the 16:19 splash.png has blur over only the
   top ~43% of the 3840x2160 frame; bottom is sharp. Blur applied to the capture is incomplete —
   race between blur pass and save, or partial-extent blur (fullscreen 4K + notch=extend strip?).
2. **Resume-from-park replay SIGSEGV** (worker-fatal): venus journal replay dereferences null (+0x60)
   in KK end_subpass on a replayed CmdEndRenderPass. Likely replay-ordering/pruning bug: a
   CmdEndRenderPass replayed without its matching CmdBeginRenderPass (journal recorded mid-renderpass
   at park?). Also a UX bug: worker death during "resuming" is not surfaced; UI hangs in "resuming".
3. **s2idle wake wipes the GPU session** (guest 3D dies, compositor crashes): guest virtio-gpu driver
   has no PM ops, so after host sleep the driver does a device reset + DRIVER_OK without reprogramming
   queues; the worker re-arms from previous activation but then a reset with a non-quiescent session
   (13894 awaiting-shown!) times out its 1.5s drain and wipes fail-closed → all guest contexts die →
   venus ring fatal in every client → synoik aborts (and anything Vulkan-based must recreate devices).
   The 3m20s "suspend devices" gap (guest s2idle entry sliced by host sleep) and the 13894
   awaiting-shown backlog look load-bearing.

## Evidence locations (originals)

- dogfood-mac: `~/Library/Logs/DiagnosticReports/limina-vmm-2026-08-10-161925.ips` (persists)
- dogfood-mac: `~/Library/Application Support/Limina/VMs/Dev.liminavm/logs/supervisor.log` — **VOLATILE:
  truncated on next VM start** (already only covers boot 0). Copy here.
- dogfood-mac: `.../Dev.liminavm/run/splash.png` — **VOLATILE: overwritten on next suspend**. Copy here.
- dogfood-mac: `.../Dev.liminavm/logs/balloon-trace.jsonl` (not copied; balloon behavior across the s2idle
  window is in there if needed)
- dogfood-guest: `coredumpctl info 2855` → `/var/lib/systemd/coredump/core.synoik.1000.f7fb…2855…zst` (present)
- dogfood-guest: journal boots: -1 = 15:39:59→16:18:55 (the park), 0 = 16:27:25→ (the s2idle wipe)
- Older, possibly-related retired reports on dogfood-mac: limina-vmm 08-04 15:57, limina-vmm 08-07 14:22,
  limina 08-06 19:49, limina 08-09 21:46 (not examined)

Copies in this directory: limina-vmm-2026-08-10-161925.ips, supervisor.log, splash.png, splash-small.png,
synoik-binary-2026-08-10 (the exact /usr/local/bin/synoik the coredump matches — preserved because the
user rebuilds it frequently; pair it with the core for symbolization).

## Verification pass (advisor-driven)

- **16:25:59 host reboot: NO kernel panic report** in dogfood-mac /Library/Logs/DiagnosticReports (only a
  stale 2025 `.contents.panic`), and no "Previous shutdown cause" line in the unified log (message not
  emitted/retained on this macOS). Presumed user-initiated (matches the agreed hv-ledger dual-machine
  reboot plan) — confirm with the user, but no evidence of a panic.
- **Unified log is a dead end for the crashed run**: limina[pid] entries are only AppKit/xpc framework
  noise; the app's own tracing goes to the per-run file (truncated). The .ips is the sole host-side
  record of the 16:19 run.
- **The 16:40:54 WebKitWebProcess SIGSEGV is GPU-adjacent but a DIFFERENT bug**: Epiphany's web
  process crashed inside guest zink — recursive `zink_set_framebuffer_state → zink_flush_clears →
  zink_batch_rp → zink_render_attachment_shadow → util_blitter_blit_generic → do_blits →
  zink_set_framebuffer_state …` (stack-overflow shape, dies in _debug_printf). Pre-sleep, 13 min into
  a fresh boot, venus session healthy at the time (synoik frame logs clean until 17:41). Does not
  weaken the "issue 3 needs no accumulated state" claim; file as its own guest-zink lead
  (mesa 26.1.5-6.limina).
- **awaiting-shown arithmetic (lead for issue 3)**: 13894 awaiting-shown over boot-0's 16:27:25→17:41:22
  awake window (4437 s) ≈ 3.1/s — right at synoik's logged idle cadence (~2-3 fps) integrated over the
  WHOLE session. That reads as the shown-ack queue never draining from session start (state.toml:
  window is fullscreen — cf. the #24 off-glass ack-gating history), i.e. host sleep may have merely
  exposed a session-long ack leak, not created the backlog.
- **Close-while-suspended → resume**: code-supported. session.rs:357-476 — park exits the worker
  (WORKER_EXIT_SNAPSHOT persists `[suspended]`), any resume spawns a FRESH worker that consumes
  snapshot.bin (`restore_gpu_payload` replay); the "resuming" state + relaunch loop live there
  (session.rs:407-409 logs the play-click path). The crashed pid 69310 was that fresh resume worker.
  Exactly which UI event mapped close→resume remains to be read out of window/mod.rs (inference from
  the UI label + on_window_close=suspend semantics).

## Local-only artifacts (untracked — public repo)

`splash.png` / `splash-small.png` (screenshots of the user desktop) and
`synoik-binary-2026-08-10` (548M) are kept next to this file but gitignored.
If this clone is lost, re-pull the splash from the dogfood machine only if it has
not suspended since 2026-08-10 16:19 (overwritten on every park).

## Issue 1 VERDICT (2026-08-10, investigation): NOT a limina bug — faithful capture of a mid-slide lock curtain

- limina applies **no blur anywhere** (no blur/gaussian code in any of our crates); the splash save
  is a raw IOSurface dump of the last-shown buffer (`window/mod.rs` exit path →
  `diag::capture_iosurface`), taken after the worker is dead, when nothing can write the surface.
- The guest compositor (the user's gnome-shell replacement) locks on suspend and its lock curtain
  **slides down over 250ms** (`SLIDE_TIME`, EaseOutQuad; `src/ui/lock_screen.rs`), the whole group —
  blurred backdrop included — translating as one (`src/synoik.rs` shield render: backdrop drawn at
  `origin=(0, slide)`; `slide = -curtain_progress * H`). Backdrop = wallpaper blurred at
  BLUR_RADIUS=90 logical px and dimmed by BLUR_BRIGHTNESS=0.65 — hence the dark blur.
- **Pixel proof** (`seamtest.py`): seam at row 939 (curtain 43.5% down). The strip above the seam
  correlates with the blurred+dimmed wallpaper's BOTTOM 43% (translation hypothesis, i.e. a real
  mid-slide frame) at **NCC 0.9993**, vs 0.75 for the same-coordinates (torn-capture) hypothesis.
  Calibration control: the sharp region matches the cover-scaled wallpaper at the exact centered
  crop, NCC 0.9994. Also consistent: no clock in the strip (mid-slide the clock is off-screen above;
  a torn fully-covered frame would show it at rest position inside the strip).
- Mechanism: suspend key → compositor `prepare_for_sleep` → `lock` → `activate` (the SLIDING path;
  its `curtain_instant` variant is only used on the idle/fade path) → sleep inhibitor released once
  the authenticator is ready (model state, not animation state) → kernel enters s2idle the same
  second → guest frozen ~62ms into the slide (progress 0.248 ⇒ coverage 0.435 via EaseOutQuad).
- **Any fix belongs in the compositor** (the user's project, their call): use the instant curtain on
  the about-to-suspend path (same rationale as the fade path: nobody sees an animation the machine
  sleeps through), or hold the logind delay inhibitor until the curtain settles + the frame is
  presented. limina behaves correctly; resume will show the finished lock screen and the splash
  merely mirrors the guest's frozen mid-animation state.

## Issue 3 VERDICT (2026-08-10 local repro + fix): stranded present bookkeeping is not in-flight work

**Reproduced end-to-end on the dev Mac** (suspend-repro.raw clone, windowed, synoik + zink env +
ghost translucent — the dogfood parity environment): `sudo systemctl suspend` in the guest, then a
SIGWINCH to the worker (the KEY_WAKEUP test seam, `crates/limina-vmm/src/wake.rs`) →
`device reset with the session NOT quiescent (… 1 parked-flush-cookies …) — wiping (fail-closed)` →
synoik SIGABRT + ghost SIGABRT (coredumps in the guest), the session torn down — the dogfood
incident, minus host sleep. A GNOME control run on the same image was clean (drain quiesced in
~11µs, in-place adopt worked): GNOME quiesces rendering before s2idle; a continuously-rendering
Vulkan compositor hits the freeze mid-stream every time. Fullscreen A/B (6.5 min idle): still
`0 awaiting-shown` — the dogfood 13894 backlog did NOT reproduce and is filed separately.

**Root cause** (all in the libkrun fork, `virtio_gpu.rs`): `present_quiescent()` counted
`flush_parked_cookies` and `awaiting_shown` against quiescence. Both are forward-looking host
bookkeeping that never references the virtio queue a reset frees:
- `flush_parked_cookies` is per-flush scratch — `flush_resource` CLEARS it at entry and only the
  same flush's trailing FLAG_FENCE consumes it. An unfenced final flush (or one whose trailing
  fence the guest's freeze-time reset dropped — the no-PM-ops virtio-gpu driver resets the device
  on freeze, dropping in-queue commands) always leaves exactly 1 cookie parked. Hence the
  deterministic "1 parked-flush-cookies" on both machines.
- `awaiting_shown` waits on supervisor "shown" acks, which cannot be delivered during a reset.
Neither can drain once the activation is gone, so the classify path burned its full 1.5s timeout
and wiped fail-closed on every wake → every guest venus ring fatal → compositor SIGABRT.

**Fix** (libkrun fork `limina` branch, commit 8ec965d, RED-first): quiescence counts only the trio
that indexes the activation's queue (parked flushes, guest holds, unprocessed retirements); the
park path discards the stranded bookkeeping with a log line (a stale cookie left behind would turn
the next activation's first trailing fence into a spurious guest hold; a stale awaiting-shown entry
skews the pop-to-first ack drain forever). Unit tests in `virtio_gpu.rs` cover both directions;
`facf7eb` fixes the pre-existing edid test-import breakage that blocked `cargo test --features gpu`.

## Issue 2 VERDICT (2026-08-10 local repro + fix): replay dispatches the tail of a broken recording

**Reproduced deterministically on the dev Mac** with `vkr-hazard.c` (this dir): record a cmd_buf
(Begin → CmdBeginRenderPass(FB) → CmdEndRenderPass → End), destroy the framebuffer, keep the
process alive, then park (SIGTSTP) + click-resume. One cycle on the unpatched dylib = the exact
dogfood crash: `EXC_BAD_ACCESS KERN_INVALID_ADDRESS at 0x60`, `end_subpass` ←
`vk_common_CmdEndRenderPass` ← `vn_dispatch_vkCmdEndRenderPass` ← `vkr_renderer_replay_submit`.
0x60 = `&((struct vk_render_pass *)NULL)->subpass_count` — the mesa runtime derefs
`cmd_buffer->render_pass` with no active render pass.

**Root cause** (virglrenderer fork, venus journal): RECORDING entries are keyed by their cmd_buf
and do NOT pin the objects their payload references (CREATE entries do). Destroying a referenced
object (dogfood: a compositor framebuffer that died before the suspend) leaves a replayable
recording whose CmdBeginRenderPass names a dead handle. On resume the Begin fails decode and is
FATAL-recovered — but the rest of the recording still dispatched, and CmdEndRenderPass walked into
the driver with no render pass bound.

**Fix** (virglrenderer fork `limina` branch, commit 283aeea4): poison the cmd_buf at the first
failed RECORDING dispatch; skip its remaining RECORDING entries for the rest of the replay
(Begin/Reset lifts the poison; a failing Begin re-poisons via the post-dispatch hook). Verified
GREEN in one cycle: `replay: poisoning cmd_buf 12`, resume completes (first frame 4.5s), hazard
process survives, zero coredumps. Bonus: skipped entries are not re-journaled, so the NEXT snapshot
is clean of the broken recording — one cycle self-heals.

**UX half** (limina, window/mod.rs + present.rs + session.rs): the dogfood "Resuming…" forever was
the timer's dead-worker exit branch gating on `ParkPhase::Live` — a worker dying during `Resuming`
was never handled. Detection must not race the swap (the OLD worker's `exited` flag stays set until
`mark_worker_running`), so death = `resume_dead || (exited && worker_epoch > epoch_at_click)`
(`resume_worker_died`, unit-tested). On death: log, alert ("failed to resume … starting it again
will boot fresh"), save state, exit — verified live in the RED cycle (alert shown, OK exits).

**Close-while-parked UX asks: both already hold in the normal flow** (verified live): closing a
parked window just quits keeping the snapshot; relaunching the same flat command auto-resumes from
it (first frame 4.3s). Dogfood's cold boot happened because the CRASHED resume had already consumed
the snapshot (one-shot rename at spawn, by design — anti-crash-loop). Residual hazard, by design:
the whole parked content is a play target, so a click aimed at the auto-reveal close button that
lands short resumes instead — flagged for the user, not changed.

**Residual (correction to the 283aeea4 commit message): a poisoned cmd_buf is left
mid-recording, not empty.** The replayed vkBeginCommandBuffer succeeded and was re-journaled
before the CmdBeginRenderPass failed, so across snapshot generations the cmd_buf converges to a
host-side recording that was Begun and never Ended (the hazard's second-generation journal is
exactly Begin-only). A guest that resubmits that cmd_buf without re-recording pushes an un-ended
command buffer into KK — invalid usage, same trust-boundary class as the empty-clear-rect
incidents. Low severity (a client whose referenced object died re-records before reuse), but if a
future KK crash implicates a resumed cmd_buf, start here. The alternative — synthetically ending
or resetting poisoned cmd_bufs at replay end — has its own hazards (the pool may lack
RESET_COMMAND_BUFFER_BIT) and was deliberately not taken.
