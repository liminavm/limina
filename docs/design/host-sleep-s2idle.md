# Host sleep → guest s2idle (in place), and session-preserving thaw for stock guests

*Design agreed with the user 2026-07-20. Two parts: a libkrun-side fix that makes in-place guest
s2idle preserve the venus session for ANY guest (stock included), and the limina-side host-sleep
bracket that rides on it. The guest-kernel PM-ops fix is on the roadmap as a separate low-priority
item — it is an upstream-facing improvement, not a prerequisite (see §5).*

## 1. Motivation

- **Close the last stock clock gap.** The suspend/resume clock is verified correct (2026-07-20:
  libkrun 0088 PL031 → `rtc-efi` → kernel sleeptime injection, +0.058 s after a 122 s gap, guarded
  in `managed_vm_suspends_and_resumes`) — but only if the guest actually suspends. A guest that
  keeps "running" through host sleep gets a frozen CNTVCT and a wrong wall clock; on the stock
  tier nothing corrects it (enhanced has agent TimeSync). s2idle'ing the guest around host sleep
  makes the *same verified path* fix the clock with zero guest components.
- **Honest suspend semantics.** Guest apps see a real suspend/resume instead of a mystery time
  jump; network daemons, timers, and leases re-sync through their normal resume paths.
- **Possible glitch dodge.** If the CNTVCT backward-glitch (the 2119 wrap, source unfound) is
  triggered by host deep-sleep, a guest sitting in WFI through host sleep may simply never
  observe it.

Constraint (two-tier doctrine): the mechanism must work for **stock guests** — host-side only.

## 2. The state of the substrate (all verified in source / measured)

- **The thaw reset is guest-side and unavoidable on stock.** `virtgpu_drv.c` registers no PM ops
  (verified at 7.1.2 + `/proc/kallsyms` live), so the virtio core's bus fallback resets the GPU on
  every s2idle thaw: reset → re-negotiate → DRIVER_OK with **zero queue writes** (M9.3 MMIOTRACE:
  every other device re-programs queue geometry; the GPU writes status only). Upstream-broken for
  any spec-faithful device; QEMU guests should wedge identically.
- **The transport half is already solved for in-place resume.** libkrun 0072's sticky queue re-arm
  (`mmio.rs rearm_queues_from_stash`) reconstructs queues from the previous activation's register
  file when DRIVER_OK arrives with none programmed — its own doc comment names "in-place resume"
  as a supported case (`mmio.rs:383`).
- **The session-wipe is OUR policy, one call.** On queue teardown (`InnerExit::Deactivate`) the GPU
  worker unconditionally runs `virtio_gpu.reset_session()` (`worker.rs:228`): resources, sw2d,
  scanouts, journal, cursor, fence state, rutabaga per-session state (patch 0035's dirty-reset
  hardening). *This* is what loses the seated session on in-place s2idle — not the reset itself.
- **Both wake and quiesce plumbing exist.** The GPIO sleep-button pulse s2idles a stock F44 GNOME
  guest (~1–4 s; proven in `managed_vm_suspends_and_resumes` and the 2026-07-20 flat-run clock
  verification); the worker's `is_quiesced` oracle detects s2idle entry (every virtio device at
  INIT, GPU excepted); `wake.rs` pulses the `KEY_WAKEUP` GPIO (the only line that wakes an s2idle
  guest) and carries a SIGWINCH test seam for driving an in-place wake without a restore.

## 3. Part 1 — defer-and-classify the GPU session reset (libkrun; the load-bearing piece)

**Insight: the wipe's two justifications don't apply to a thaw.** Per `reset_session`'s own doc
comments (`virtio_gpu.rs:1282-1299`): (a) in-flight fence descriptors index the freed queue — but
an s2idle guest froze userspace before device suspend, so fences drain to ~zero, and
`drain_fences()` exists to guarantee it; (b) dirty-reset id collisions from a crashed/rebooted
guest — but a thawing guest is the *same live kernel with the same beliefs*: its context ids,
rings, and blob mappings in guest RAM all still match the live world in the same worker process.
Nothing is stale on either side. Keep the world and the session survives **with no replay at all**.

**Design — defer the wipe, classify at the next activation:**

1. **On Deactivate:** run a bounded fence drain (reuse the snapshot bracket's `drain_fences`
   shape). If it drains to zero → **park** the session (skip `reset_session`; keep resources,
   sw2d, scanouts, journal — which keeps recording, the session continues — cursor state,
   rutabaga session state, present-fence plumbing; unbind only the transport, `active = None`).
   If the drain times out → run `reset_session` now (fail-closed: exactly today's behavior).
2. **At the next activation, classify by 0072's own signature:**
   - Re-arm path fired (DRIVER_OK, **no queues programmed** this cycle) **and** a parked session
     exists → **thaw**: adopt the parked world unchanged. Session preserved by construction.
   - Queues **were** programmed (reboot, driver rebind, firmware→kernel hand-off, kexec) →
     run the deferred `reset_session` first, then proceed — semantics identical to today.
3. **Disjoint from the snapshot path by construction:** staged-replay only exists on a fresh
   worker (`pending_restore`), parked-session adoption only on a live worker that Deactivated;
   the two cannot coincide. The fresh-worker restore leg is untouched.
4. **Failure ladder:** anything ambiguous (drain timeout, feature re-ack mismatch, re-arm
   validation failure) falls back to the deferred wipe → today's session-restart floor. The fix
   can only improve outcomes, never worsen them.

**Premises to re-verify empirically before building (each is cheap):**
- The GPU thaw re-acks an identical feature set (same kernel/driver; `mmio.rs:360` already
  handles a smaller re-ack — assert equality as the adopt precondition anyway).
- In-place s2idle loses the session **solely** because of the wipe (this is the RED test).
- Scanout: the thawed guest never re-issues SET_SCANOUT (it doesn't know a reset happened), so
  parked scanouts must be kept — confirm the display path re-lights from the kept binding when
  the compositor's first post-thaw page-flip arrives.

**Tests:**
- **RED L2 `venus_session_survives_inplace_s2idle`:** seated venus guest, in-guest
  `systemctl suspend`, wake via the SIGWINCH seam, assert same gnome-shell PID + same boot_id +
  zero new coredumps + presents flowing. RED today (wipe), GREEN with defer-and-classify.
- **Headless in-place L2 first** (pin the substrate, likely already green via 0072): s2idle +
  seam wake + same boot_id + the ±5 s wallclock guard after a deliberate gap — this also pins
  the stock-tier clock benefit that motivates the whole feature.
- The reboot-after-thaw leg (classify → wipe) rides the existing reboot guards.

## 4. Part 2 — the host-sleep bracket (limina)

- **On host `willSleep`** (NSWorkspace notification; use `IORegisterForSystemPower`'s root port to
  hold the sleep ack): if the guest is awake, pulse the GPIO **sleep** button (existing bracket
  mechanism, but with **no snapshot leg** — a new worker command that pulses + waits on
  `is_quiesced`, i.e. `suspend.rs` minus the save). Hold the ack until quiesce, capped (~10 s);
  on timeout release the ack anyway — the guest just experiences today's frozen-CNTVCT behavior,
  no worse. Record a `slept_by_host` flag.
- **On host `didWake`:** pulse the wake key (`wake.rs`) **only if `slept_by_host`** — never wake a
  guest the user suspended themselves, and never pulse the *sleep* button at an asleep guest (the
  latch trap: a latched pulse re-suspends the guest unwakeable).
- **Clock:** stock = kernel sleeptime injection via the 0088-anchored RTC (the verified path);
  enhanced = agent TimeSync also fires on supervisor-detected oversleep — both are idempotent
  (the agent steps only ≥1 s of error; the injection already fixed it → no-op).
- **Config:** `vm.toml [power] on_host_sleep = "s2idle" (default) | "ignore"`. Per-VM; every
  supervisor handles its own VM, windowed and headless alike, so multi-VM works for free.
- **Testing:** host sleep is not automatable in CI. Unit/seam-test the bracket pieces (a faked
  willSleep drives pulse+quiesce; SIGWINCH drives wake); the s2idle+wake cycle itself is covered
  by the Part-1 L2s; final validation = manual lid-close eyeball, then dogfood.

## 5. Phasing

1. **Headless in-place s2idle+wake L2** — pin the substrate + clock benefit (expect green today).
2. **Defer-and-classify** (libkrun) + the RED venus L2 → GREEN. This alone also fixes plain
   in-guest `systemctl suspend` of a windowed VM, independent of host sleep.
3. **Host-sleep bracket** in limina + `[power]` config; manual eyeball; dogfood.
4. **Later, low priority (roadmap):** the guest-kernel virtio-gpu PM-ops fix (carry/refresh the
   Dongwon-Kim series in `patches/linux`, enhanced tier) + upstream-report the core gap. Makes
   enhanced guests take the clean path and strengthens the upstream story; stock guests need the
   host-side fix regardless, and it is sufficient on its own.
