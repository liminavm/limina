# M9 — the guest freeze/restore trigger — decision

> **STATUS: PROPOSED — decision recorded, gated on one small feasibility spike (F below).**
> Surfaced by the 2026-07-17 M9 design review as the biggest hole in `m9-suspend-resume.md`:
> the whole Strategy-A GPU story rests on the guest re-creating its GPU objects on restore, but
> **nothing in a host-side snapshot ever runs the guest code that does that.** This doc names the
> problem, lays out the options, and recommends one. Every non-obvious claim carries a `path:line`
> into `third_party/` or a source URL.

---

## 1. The problem

The host-side snapshot pauses the vCPUs **externally** (`hv_vcpus_exit` + the pause wiring M9.1
adds), dumps RAM + device + vCPU state, and kills the worker. On restore a fresh worker maps the
same RAM back and the guest **continues from the exact instruction it was paused on** — it has no
idea a snapshot happened. This is the whole appeal of host-side (GPU-agnostic, works on a stock
guest). But it means:

> **In a host-side snapshot the guest never suspends and never resumes, so none of its power-management
> callbacks run.**

The Strategy-A GPU plan (`m9-suspend-resume.md` §4) carries the **Dongwon Kim drm/virtio
freeze/restore series**, whose entire job is to re-create the guest's GPU resources against the fresh
host renderer on resume. That series works through the **kernel PM callbacks** (`.freeze`/`.restore`
+ a hibernation PM notifier). The venus tier's Mesa-level object-graph replay needs an even
higher-level trigger (the design already flags this as "the undesigned kernel-resume→userspace-venus
trigger"). **Both sit on the same missing foundation:** with an external pause, the kernel's
`.restore` never fires, so the resubmit never fires, so the fresh renderer stays empty and the guest's
GEM handles / VkObjects dangle. The design's GPU section is built on a mechanism with no invocation
path.

Three more things ride on the same gap, which is why closing it is high-leverage rather than a tax:

- **Clock.** If the guest runs its normal resume path, `timekeeping_resume()` re-reads the RTC and
  restores `CLOCK_REALTIME` for free — no bespoke port-123 consumer needed on the enhanced tier
  (given the already-scoped PL031 wallclock fix, [[limina-guest-clock]]). With an external pause,
  nothing re-reads the clock.
- **IRQ-in-service.** Spike #2 found `ICC_RPR_EL1` is read-only, so we must **quiesce to
  no-IRQ-in-service before snapshotting** (`spikes/m9-hvf-state-roundtrip/RESULTS.md`). A guest that
  has entered a sleep state has no interrupt in service — the edge dissolves.
- **In-flight virtio I/O.** A cooperative sleep runs every virtio driver's freeze/quiesce, draining
  the queues before we serialize device state — instead of us having to serialize queues caught
  mid-descriptor.

## 2. What we do NOT get to assume

- **The stock tier has no agent and no custom kernel.** Whatever we design as the enhanced trigger,
  the **stock floor is still a raw, non-cooperative external-pause snapshot** — no guest help. There,
  the honest promise is *save/restore of the machine with a GPU re-init blip*, and spike #3
  (`spikes/s4-hibernate/gpu-reset-live.md`, round 3) says the realistic outcome with live 3D clients
  is a **compositor crash / session loss that recovers** (boots-and-usable — satisfies the two-tier
  floor, but is not seamless). The design's "GPU re-init blip" wording oversells this; it is corrected
  in `m9-suspend-resume.md` §5.
- **s2idle is not S4.** Suspend-to-idle freezes userspace and runs device system-sleep callbacks but
  **does not** offline CPUs via PSCI or call `swsusp_arch_suspend` — so it does **not** hit the two
  spike-#1 blockers (PSCI `CPU_OFF` NOT_SUPPORTED; the `OSDLR_EL1` trap on the CPU-suspend path,
  `spikes/s4-hibernate/RESULTS.md`). That is the key reason a cooperative *shallow* bracket is viable
  where guest-side S4 was not. **But** s2idle uses the `.suspend`/`.resume` dev_pm_ops, whereas the
  Dongwon Kim series (as posted) implements the **hibernation** `.freeze`/`.restore` callbacks — see
  the callback-matching question in §5.

## 3. Options

### Option 1 — Agent-coordinated guest sleep bracket (RECOMMENDED)

Host asks the `limina-agent` to put the guest into a shallow, cooperative sleep; host snapshots the
quiesced machine; on restore host injects the wakeup and the guest runs its normal resume path.

```
suspend:  host → agent: PrepareSnapshot
          agent → kernel: enter suspend-to-idle (echo freeze > /sys/power/state, or an agent PM call)
          kernel: freeze userspace → each virtio/gpu driver's system-sleep callback quiesces + records intent
          agent → host: quiesced        (or host polls the vCPUs to a WFI/idle)
          host: pause vCPUs, drain GPU fences, serialize, kill worker
restore:  fresh worker maps RAM + device + vCPU state, injects the wakeup IRQ
          kernel resume path: virtio drivers re-negotiate; virtio-gpu resubmits (Dongwon Kim);
                              timekeeping_resume re-reads RTC; userspace thaws; venus replay fires
```

Wins: fires the GPU resubmit/replay through the guest's own PM machinery (the mechanism the whole
design assumes), fixes the wall clock with no port-123 consumer, dissolves the `ICC_RPR_EL1`
mid-service edge, and drains virtio I/O before serialization. One design change, four problems solved.

Costs / risks: (a) **the spike-#1 virtio-mmio freeze/thaw breakage moves on-path** — the guest WARNed
at `virtio_config.h:276` and the host logged `update virtio queue in invalid state 0x8f` with the
network wedged after thaw (`spikes/s4-hibernate/RESULTS.md`); that hardening is now required, not
optional. (b) Enhanced-tier only (needs the agent + the resubmit-carrying kernel). (c) The
callback-matching question (§5).

### Option 2 — Host-injected virtio-gpu device reset on restore

On restore, the fresh worker drives a virtio-gpu **reset** (config-space `DEVICE_NEEDS_RESET` /
`RESOURCE_UNREF` storm) that the guest driver observes and re-inits from, like a hotplug/FLR — no
prior guest cooperation.

Rejected as the primary: spike #3 is decisive that an **abrupt** device reset against a **live**
session crashes gnome-shell + every 3D client (DRM hot-unplug backtrace) and wedges the greeter on
orphaned contexts. To make it non-abrupt you must quiesce the guest first — at which point you have
re-invented Option 1's bracket. It also does nothing for the clock or for venus userspace replay.
Keep it only as the mechanism the **stock floor** falls back to (accepting the blip/crash-recover).

### Option 3 — A dedicated, PM-decoupled "reinit GPU" trigger in the enhanced kernel + agent

Carry a custom guest path (a limina virtio-gpu ioctl / uevent / sysfs knob) that resubmits GPU
objects on demand, decoupled from Linux PM. The agent pokes it on restore.

Rejected as primary: it re-implements, out-of-tree and bespoke, exactly what the PM freeze/restore
callbacks already do — and it still needs the userspace half (compositor/Mesa) to re-create its state,
which PM resume gives us for free via the normal wake. More custom surface, less upstreamable, and it
throws away the clock/IRQ/virtio-quiesce wins. Worth remembering only if §5's callback-matching
turns out to be a wall.

## 4. Decision

**Enhanced tier → Option 1 (agent-coordinated shallow sleep bracket).** It is the only option that
drives the GPU resubmit/replay through the guest's own machinery (which the Strategy-A design already
depends on) and it collapses the clock, IRQ-in-service, and virtio-drain problems into the same
bracket. It stays inside the project doctrine (mechanism in the guest kernel/agent we own; the
libkrun side is just "pause a quiesced machine").

**Stock tier → raw external-pause snapshot (Option 2's reset as the involuntary fallback).** No agent,
no bracket; the machine saves/restores and the GPU comes back via a fresh renderer with a visible blip
that, with live 3D, may cost the session (recoverable). This is the two-tier floor, stated honestly.

**Detect additively:** agent present? resubmit-capable kernel? venus-replay Mesa? Light up the
seamless path only when its own prerequisite is there; fall back to the raw snapshot per missing piece
(consistent with the granular capability doctrine in CLAUDE.md).

## 5. The one thing that gates this — spike F (do before M9.2)

**Which guest sleep state, and does the resubmit hook its callbacks?** Two coupled unknowns:

1. **Reachability in libkrun.** Can a guest inside libkrun enter suspend-to-idle (`freeze`) and be
   woken, *without* tripping a libkrun HVF gap the way S4 did? s2idle should avoid PSCI `CPU_OFF` and
   the `OSDLR_EL1` debug-suspend trap (§2), but that is inferred, not observed — confirm it.
2. **Callback matching.** s2idle drives `.suspend`/`.resume`; the Dongwon Kim series (as posted)
   implements the hibernation `.freeze`/`.restore` (`spikes` review; the v4 cover notes it is
   S4-scoped, [dri-devel v4](https://www.mail-archive.com/dri-devel/msg566767.html)). So either (a)
   pick a sleep state whose callbacks the series already implements, or (b) have our carried patch
   also wire `.suspend`/`.resume` (or the shared `SET_SYSTEM_SLEEP_PM_OPS`) to the same resubmit, or
   (c) use a custom PM state. Determine which is least invasive on our carried kernel.

**Spike F shape:** on an enhanced-image clone, `echo freeze > /sys/power/state` from the agent with a
wakeup source armed (a vsock/timer IRQ), confirm the guest quiesces virtio + GPU and wakes cleanly;
instrument whether the virtio-gpu driver's resubmit path is invoked on that transition. Vehicle:
`boot-seated-kk.sh` + `LIMINA_DISK=<enhanced clone>` (same rig as spike #3). Gate: Option 1 is only
real if (1) is yes; if (2) needs work, it is bounded patch work on a series we already carry.

## 6. Impact on the M9 plan (feeds back into `m9-suspend-resume.md`)

- **M9.1 unaffected** (no-GPU/sw-2D guest, no bracket needed) — start there, as planned, but with
  **multi-vCPU in the first RED test** (per-vCPU ICC/MPIDR ordering is exactly where spike #2 stopped)
  and the **WFE-parked-vCPU wakeup** handled: a paused vCPU may be blocked in `wait_for_event` on a
  channel `recv()` (`third_party/libkrun/src/vmm/src/macos/vstate.rs:506`), which `hv_vcpus_exit` does
  **not** kick — the pause must wake it too.
- **Insert spike F before M9.2.** Its result decides what M9.2's device-state serialization must
  handle: a **quiesced** guest (Option 1 — drained queues, no IRQ in service) vs a **mid-flight** one
  (raw stock path — queues caught mid-descriptor). That is a materially different serialization
  contract, so decide it before building the schema.
- **M9.3 GPU** keeps its virgl-tier-first / venus-tier-second shape, but the resubmit/replay is now
  triggered by the bracket (Option 1) on the enhanced tier, and the stock tier explicitly accepts the
  Option-2 blip.
- **Virtio freeze/thaw hardening (spike #1's `invalid queue state 0x8f`) is on-path** for Option 1 —
  fold it into M9.2, not "someday."

## 7. Related

Design: `docs/design/m9-suspend-resume.md` (§4 GPU, §5 two-tier, §M9.2/M9.3). Spikes:
`spikes/s4-hibernate/RESULTS.md` (#1, the S4 blockers this sidesteps), `.../gpu-reset-live.md` (#3,
the abrupt-loss evidence behind rejecting Option 2 as primary), `spikes/m9-hvf-state-roundtrip/RESULTS.md`
(#2, the `ICC_RPR_EL1` quiesce requirement). Memory: [[limina-m9-suspend-resume]], [[limina-guest-clock]].
</content>
</invoke>
