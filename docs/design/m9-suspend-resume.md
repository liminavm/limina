# M9 — Suspend / resume + full VM snapshots (host-side) — design

> **STATUS: DESIGNED — direction chosen, not yet started.** This doc was rewritten 2026-06-28 after a
> spike + a GPU-prior-art research pass. **Decision: host-side VMM snapshot is the primary mechanism**
> (Parallels-style), which *also* unlocks full VM snapshots as a feature. Guest-side Linux S4 hibernation
> — the earlier draft's primary — is demoted to a documented, deprioritized alternative (Appendix A) after
> spike #1 showed it's the more fragile path. Every non-obvious claim carries a `path:line` into
> `third_party/libkrun/`/`crates/`, or a URL. **VERIFIED** = read this round; **ASSUMED** = needs a spike.

**Goal in one line:** pause the VM, persist its entire state to a host-side file, free the machine
(process gone, host RAM reclaimed); later restore it to the exact state — same apps, windows, clock,
working accelerated display — *and* let the user keep, name, clone, and roll back to multiple such
snapshots.

---

## 1. Goal & motivation

Two user-visible features, one machinery:

1. **Suspend / resume** — Parallels-parity "Suspend": freeze the guest to a file in a second or two,
   **tear the worker process down** (reclaim host RAM, the GPU/Metal/IOSurface graph, gvproxy), and a
   later "Resume" that restores the same desktop with a correct wall clock and working 3D.
2. **Full VM snapshots** — snapshot a *running* VM to a named file, then **restore / clone / branch /
   roll back** later. This is the strategic multiplier: a marquee Parallels/VMware feature category
   (save-state-before-the-risky-thing, throwaway dev environments, golden images) that a guest-side
   suspend can *never* provide. Since the suspend floor needs VMM-snapshot machinery anyway, building it
   as the primary and exposing snapshots as a first-class feature is high-leverage.

Lifecycle/snapshot is the last uncovered headline Parallels feature (`docs/research/GAPS-and-verification.md`).
The hard constraint remains the **two-tier guarantee** (CLAUDE.md): a stock guest must keep working,
enhancements only improve it.

---

## 2. The decision

**Host-side VMM snapshot, with the GPU handled by Strategy A (quiesce + guest-driven re-init).**

- **Snapshot mechanism:** pause all vCPUs, quiesce the virtio worker threads, serialize vCPU + device
  state + guest RAM to a file, kill the worker. Restore = relaunch the worker with `--restore <file>`,
  reload RAM + device + vCPU state, resume. This is Firecracker's `Pause → CreateSnapshot → LoadSnapshot
  → Resume` and crosVM's "freeze vCPUs, then devices, then serialize." It is **GPU-agnostic** — the GPU
  is made a clean participant, not a blocker.
- **GPU strategy = A (quiesce + guest re-init), NOT host-side GPU-state serialization.** On snapshot,
  drain fences and tear the host renderer context down cleanly; on restore, present the virtio-gpu as
  **freshly initialized** and let the guest's virtio-gpu/DRM driver + Mesa + compositor **re-create their
  GPU resources** — exactly as on a real-hardware power cycle. Resource *contents* ride along because
  virtio-gpu resources are **guest-backed** (`ATTACH_BACKING`/blob) → captured by the RAM snapshot for
  free; generated framebuffers are re-rendered. (§4 has the full justification.)

**Why this and not guest-side Linux S4 hibernation (the earlier primary):**
- **Fewer moving parts / failure points.** Spike #1 (`spikes/s4-hibernate/RESULTS.md`) showed the
  guest-side S4 path is a parade of fragile pieces — PSCI gaps, an unhandled debug sysreg, SELinux swap
  labeling, swap provisioning, `resume=` plumbing, virtio freeze/restore breaking the network — each a
  thing that rots across a distro update. A host-owned file has one owner: us.
- **It unlocks full snapshots; S4 can't.** S4 only ever gives suspend-resume-once.
- **It sidesteps spike #1's blockers.** A host-side snapshot **pauses the vCPUs externally and never runs
  the guest's suspend path**, so `disable_nonboot_cpus()` and `swsusp_arch_suspend` never execute → the
  PSCI `CPU_OFF` gap and the `OSDLR_EL1` trap that blocked S4 **cannot bite here.** We trade them for
  different (and more valuable) libkrun work: real pause, full vCPU save/restore, device serialization,
  GIC state.
- **Transparent to the stock tier.** A stock guest can't be relied on to hibernate; a host snapshot
  doesn't care what the guest is.

The cost, eyes open: it's the **heavier upfront VMM build**, and **HVF has no dirty-page log** → a
stop-the-world full RAM dump (a multi-second stall for a 4–12 GiB desktop). Fine for suspend; a UX note
for "snapshot a live VM" (no invisible pre-copy is possible without a dirty log HVF won't give us).

---

## 3. The hard parts and how the host-side snapshot handles them

| Hard part | Handling |
|---|---|
| **Accelerated GPU host state** (the canonical blocker) | **Not serialized — Strategy A.** Quiesce + guest re-init; guest-backed resource contents ride the RAM snapshot, the rest is re-rendered. See §4. |
| **In-flight GPU fences** | Drained to fence-quiescent before the dump; the restored guest starts from a fresh first frame. |
| **vCPU state** | Full save/restore via HVF's register API (X0–X30, PC, PSTATE, SP/SPSR/ELR, the EL1 sysreg set incl. debug regs, Q0–Q31+FPCR/FPSR, pending interrupt, **vtimer offset**). HVF round-trip completeness is **ASSUMED** → M9.0 spike #2. NB: reading these via `hv_vcpu_get_sys_reg` is *different* from the guest *trapping* on them (the spike-#1 `OSDLR_EL1` trap) — host save/restore doesn't trap. |
| **In-kernel GIC state** | `hv_gic_state_create`/`get_size`/`get_data` + `hv_gic_set_state` (unused today). Highest-risk item → spike #2. Fallback: a userspace `GicV3` (plain serializable fields). |
| **virtio device state** | A versioned device-state schema (CBOR; magic + semver; fail-closed on mismatch — Firecracker's "no cross-version migration" pragmatism; QEMU-subsection-style optionality since the two tiers present different device sets). Serialize each device's `Queue`/`MmioTransport`/features/config. |
| **`CLOCK_MONOTONIC`** | Set `CNTVOFF_EL2` via `hv_vcpu_set_vtimer_offset` on restore → continuous (default for a desktop; the Firecracker "resumed on another day" policy choice, made explicitly). |
| **`CLOCK_REALTIME` (wall clock)** | No `/dev/rtc` on macOS → push from host. libkrun's **port-123 timesync already fires on long-sleep detection** (`timesync.rs`, `TSYNC_PORT = 123`, the "long nap" path) — needs an enhanced-agent consumer. Stock degrades to NTP slew. |
| **Networking / gvproxy** | gvproxy relaunched (existing reboot path); guest NM re-DHCPs against the static `.2` lease; in-flight TCP lost (acceptable lid-close semantics). |
| **16 KiB-host / 4 KiB-guest pages** | RAM dump is host-page-agnostic (`hv_vm_map`'d host memory); restore re-maps the same IPAs. **Skip the 8 GiB GPU SHM window** (re-pointed at renderer memory) and optionally balloon-inflated pages. A snapshot restores into a worker configured **identically** (RAM size especially — `num_physpages`-equivalent). |
| **Uniqueness / entropy** | Only a hazard if a snapshot is *cloned* (the full-snapshot feature). Ship a cheap VMGenID-style reseed notification; Linux ≥5.18 reseeds its kernel PRNG. |

---

## 4. The GPU: Strategy A, and why (the load-bearing section)

**Decision: quiesce the GPU and let the guest re-init on restore. Do NOT serialize live host GPU state.**
This is backed by a dedicated research pass (the GPU-suspend prior-art workflow, 2026-06-28) and the
user's own primary-source data from a Parallels box.

### What the market leaders actually do

- **Parallels** presents a **stock virtio-gpu** + **Mesa virgl** (copy model) to Linux guests —
  user-VERIFIED on a Parallels M4 Pro guest: `lspci` → `Virtio 1.0 GPU [1af4:1050]`, `glxinfo` → `virgl
  (Apple M4 Pro)`, Mesa 26.0.8; the device set (virtio_gpu/balloon/net/virtiofs/vsock) is the same shape
  limina builds. Suspend-with-3D works **transparently to running apps** (user-VERIFIED: a live
  `glxgears` rode straight through suspend→resume). The exact reconstruction mechanism (guest-driven
  re-init vs a *proprietary* host-side virgl GL-state serialization) is **not publicly determinable** —
  but it doesn't matter for us (see below). What *is* clear: Parallels' suspend-to-disk kills the host
  process, so the host GPU context **is** destroyed and reconstructed on resume.
- **VMware Fusion/Workstation** also suspend/snapshot virtual-3D, via **guest-backed objects (MOBs)** —
  the canonical copy of every surface/texture lives in **guest RAM**, the host GPU object is a derived
  cache; an ordinary memory snapshot captures the resource graph, the MKS rebuilds host objects on
  resume. **So Parallels is NOT unique** (correcting the working assumption). The decisive pattern:
  **suspend-with-3D works iff the resource graph is guest-backed** (MOBs; virtio-gpu `ATTACH_BACKING`/
  blob = our case) and fails when host-side-only (Hyper-V GPU-PV explicitly *disables* checkpoints;
  VirtualBox can't suspend with 3D).

### Why we can't (and needn't) serialize host GPU state

True live-GPU-state serialization for our stack is **unprecedented and infeasible**:
- QEMU hard-blocks it: `migrate_add_blocker("virgl is not yet migratable")` — fires for virgl, **venus**,
  and drm-native-context alike. crosVM/rutabaga's `VirglRenderer::snapshot()`/`restore()` are explicit
  `Err(Unsupported)` stubs; venus inherits that. virglrenderer's public header has **zero**
  save/restore/migrate entry points. Cuttlefish snapshots only with the **software renderer**. **Even
  Apple's `VZVirtualMachine.saveMachineStateTo` refuses the virtio GPU** used by GUI Linux. (all VERIFIED)
- Where GPU live-migration *does* exist (NVIDIA vGPU, AMD SR-IOV, MS GPU-P DDIs) it's a **vendor opaque
  blob via a defined HW save/restore interface dumping dirty-trackable VRAM** — **Metal exposes no such
  interface**, so that family is closed to us.
- For venus specifically, host-CPU readback of GPU-written content is **not coherent** (the #28 wall: the
  write lives in Apple's SLC beyond the guest mapping's PoC) — so we couldn't reliably read the bytes out
  host-side even if we wanted to.

### Why Strategy A is ours to own — and how much work, split by tier

The thing that makes A work is **owning the guest GPU driver**. But *which layer* and *how much* depends on
the tier — a distinction spike #3 (`spikes/s4-hibernate/gpu-reset-live.md`) sharpened the hard way (a live
guest does **not** survive losing its GPU device without this work; seamless survival is not free).

**Foundation (both tiers): the kernel virtio-gpu DRM driver.** Carry the **Dongwon Kim drm/virtio
freeze/restore series** ([dri-devel v5](https://www.mail-archive.com/dri-devel@lists.freedesktop.org/msg567003.html)):
`.freeze`/`.restore` + a PM notifier; on resume re-create the virtqueues and **resubmit** the
resource/context creates (`RESOURCE_CREATE_3D`/`ATTACH_BACKING`/`CONTEXT_CREATE`) so the fresh host
renderer's tables match what the guest believes exists, keeping guest-side GEM handles valid under running
apps. **Only the kernel owns the guest-id↔host-resource mapping, so only it can replay these** — this is the
primary locus for both tiers. (Needed by any suspend design, so not pivot-specific cost.)

**virgl (GL) tier — the kernel driver ~suffices.** virglrenderer rebuilds GL state from the resubmitted
command stream; resource *contents* are guest-backed (ride the RAM snapshot); Mesa's virgl driver is
≈ transparent (its handles are kernel GEM handles, valid once the kernel rebuilds under them). This is the
**Parallels/VMware-proven** path and the achievable snapshot/resume **baseline**.

**venus (Vulkan) tier — kernel driver necessary but NOT sufficient; needs Mesa venus + the host
render-server.** The kernel knows nothing about Vulkan objects, but the canonical
`VkDevice`/image/pipeline/memory graph lives in the **host venus render-server** (→ Metal) and is **gone on
a fresh worker**. Two distinct venus problems:
  - **(a) object-graph rebuild** — re-create every `VkObject` the guest still holds against the fresh
    render-server (a venus-level **replay** of the creation stream). **venus-resume spike DONE
    (2026-06-28, `spikes/s4-hibernate/venus-resume.md`): feasible & ours to build.** Key findings: the
    **host render-server is already replay-ready** — a fresh `vkr_context` is empty and its object table is
    keyed by **guest-assigned object ids** (`vkr_context.h:217–226`), so re-issued creates with the same ids
    rebuild the graph; and a **VM snapshot preserves those ids in restored guest RAM** (so the "non-deterministic
    id" worry dissolves — it only applies to a *process restart*, not our case). The work is concentrated in
    the **guest venus driver**, which today **discards create-infos** (`vn_buffer.c:303–328` et al.) and has a
    one-shot ring — so it must be taught to **retain create-infos + replay them in dependency order + re-establish
    the ring**; plus host hardening (graceful id-collision vs the fatal `assert` at `vkr_context.h:223`) and a
    **kernel-resume→userspace-venus trigger** (undesigned). Heaviest single piece of M9; schedule as its own
    sub-project after the virgl baseline.
  - **(b) host-visible blob contents** — for venus zero-copy blobs whose bytes live in host GPU memory (not
    guest RAM), have the **guest GPU** `TRANSFER_FROM_HOST` them into guest-RAM staging at snapshot time so
    the RAM snapshot captures them; re-upload on restore. Coherency-correct (dodges #28), no opaque-Metal
    serialization. (This is the narrow "zero-copy exception" the earlier draft named — real, but only half
    of the venus story; (a) is the bigger half.)

**Restore uses a fresh worker, not an in-process reset.** A real restore cold-boots a brand-new worker
(empty rutabaga) and the guest resubmits/replays into it — so there is **no host renderer to reset in
place** and no stale-context collision. (Aside, spike #3: an *abrupt in-process* device reset is **not**
clean — `reset()` keeps the renderer alive by design (`device.rs:379`) and `reset_session()` clears the
device maps but **not** the rutabaga context table (`virtio_gpu.rs:698`), so an un-quiesced reset orphans
contexts. Real for the reboot/rebind path, but **off the restore critical path**. So libkrun 0022 /
`venus_reset` — the renderer surviving a reset — is an *intra-process* property, relevant only to that
in-place path, **not** to restore.)

**Two-tier mapping:** the **virgl tier** is the achievable baseline (kernel driver). The **venus tier** is
the premium, research-flavored feature (kernel + Mesa venus replay + render-server). A **stock** guest gets
whatever upstream virtio-gpu suspend gives (degraded / black-then-recover is acceptable per the guarantee).
**A degrades gracefully; B wouldn't even start.**

### Strategy B is a *later optional upgrade*, scoped out for now

If we ever want a non-cooperating guest to restore pixel-identical, the only credible B is **protocol-level
record/replay at the virtio-gpu boundary** (limina sees the whole create/transfer/submit stream; venus
resources are already device-independent Vulkan-object descriptions) — an *upgrade over A*, not Metal-state
freezing. Out of scope for M9.

---

## 5. Two-tier mapping

**Stock guest (must never break — the floor):** suspend/resume + snapshots via the host-side VMM snapshot
(transparent, no guest cooperation). Note spike #3's lesson: without guest resubmit support a guest doesn't
*survive* GPU re-init seamlessly — so the stock floor's honest promise is **save/restore of the machine
state with a GPU re-init blip** (the desktop may flash/recover), not seamless 3D continuity. Monotonic clock
continuous (host `CNTVOFF`); wall clock by NTP slew. If quiesce/snapshot can't proceed, suspend falls back
to a clean power-off.

**Enhanced tier (the full experience), by sub-tier:** the **kernel Dongwon-Kim resubmit** makes the
**virgl** GPU re-init seamless under running apps; the **venus** tier additionally needs the **Mesa-venus
object-graph replay** + the host-visible blob copy-back; the agent consumes port-123 timesync to
`clock_settime` the wall clock on resume; VMGenID reseed for cloned snapshots. Detect each prerequisite
**additively** (agent? freeze/restore kernel? venus replay?) — light up each refinement when its own
prerequisite is present.

---

## 6. Reuse of existing machinery

The spine exists — this is why it's tractable:
- **Reboot = relaunch-the-worker** (`crates/limina/src/supervisor.rs`: `WORKER_EXIT_REBOOT = 125`,
  `should_relaunch`, the relaunch loop). Snapshot/suspend adds a **third worker exit disposition**
  ("snapshotted/suspended", e.g. 126) and a `--restore <file>` boot mode.
- **Windowed fd-swap into a live `WorkerConn`** (`crates/limina/src/main.rs`, `spawn_windowed_worker`) —
  re-points display/input/surface plumbing at a fresh worker **without tearing down the NSWindow** — the
  exact precedent for restoring across a snapshot.
- **Surface-port receiver outlives workers** (`crates/limina/src/window.rs`); the new worker republishes
  the IOSurface over the existing port (runtime display-resize already exercises this).
- **Control plane is reconnect-tolerant**; agents re-`HELLO`; resume needs ~no new control-plane work.
- **gvproxy recycle** (`crates/limina/src/gateway.rs`) is the existing reboot path; **port-123 timesync**
  already fires on long-sleep.
- **`limina-proto`** (`crates/limina-proto/src/lib.rs`, `enum Message`): add `Snapshot`/`Restore`/`TimeSet`
  + a GPU-quiesce request. **M6 lesson:** a new variant breaks both guest binaries' matches — update together.

---

## 7. Milestone breakdown — M9

> Bisectable: edit → `scripts/apply-libkrun-patches.sh` → build → codesign → `scripts/test-boot.sh`.
> RED-first: each Done test starts as a failing `crates/limina-test` test driving the shipped binaries.

### M9.0 — Founding spikes
1. ✅ **DONE** — guest-side S4 inside libkrun (`spikes/s4-hibernate/RESULTS.md`): the *guest-side* path is
   blocked by libkrun PSCI/sysreg gaps. **Bearing on this plan:** it's *why* we pivoted to host-side
   (which sidesteps those gaps) and demoted S4 (Appendix A).
2. **Can HVF round-trip full vCPU + GIC state?** Reuse `spikes/hvf-trap-probe`: get/set all
   GP/SIMD/EL1-sysreg/PSTATE/vtimer-offset into a fresh vCPU and continue identically; does any EL1 sysreg
   reject `set_sys_reg` post-run? Confirm `hv_gic_state_get_data` → `hv_gic_set_state` round-trips. **Gates
   M9.1/M9.2.**
3. 🟡 **PARTIAL (2026-06-28, `spikes/s4-hibernate/gpu-reset-live.md`).** A real `virtio_gpu` unbind/rebind
   on a *live* seated venus desktop, three rounds (clean stop-gdm; under heavy load — glxgears 59.5 FPS +
   vkcube + Firefox WebGL; and raw unbind with the session **live**). **Proven:** the host worker is
   **robust** (survives the reset under any load, keeps serving new contexts), and the **clean** path
   cold-rebuilds a correct desktop (pixel-verified). **The decisive finding (round 3):** a **running guest
   session does NOT survive abrupt loss of its GPU device** — gnome-shell + glxgears + vkcube all crash —
   so seamless survival is **not free; it requires guest-side resubmit support.** **Source root cause:**
   `reset()` keeps the renderer alive by design (`device.rs:379`) and `reset_session()` clears device maps
   but **not** the rutabaga context table (`virtio_gpu.rs:698`), so an un-quiesced reset orphans contexts →
   `invalid context id` collision wedges the greeter. **Corrected scope (supersedes the earlier
   "renderer-reset hook" framing):** that collision is a **same-worker artifact** — a real restore uses a
   **fresh worker (empty rutabaga)**, so there's no in-process renderer to reset. The true gate is
   **guest-side**: the kernel Dongwon-Kim resubmit (lights up virgl), and for the venus tier a **Mesa-venus
   object-graph replay** (the venus-resume spike). **So #3 is a green light to *start* M9.3 (guest-side),
   not a host hook, and not a sign-off.** **Gates M9.3.**

### M9.1 — Pause + RAM + vCPU snapshot (no-GPU / software-2D guest first)
libkrun: real HVF **pause/quiesce** (kick vCPUs via `hv_vcpus_exit`, wire the inert `VcpuEvent::Pause`/
`Resume`; `resume_vcpus` is a no-op today); **`HvfVcpu::save_state`/`restore_state`**; memory dump/reload
(skip the GPU SHM window) with a two-file split (small vmstate + sparse mem) + CRC; `CNTVOFF` set on
restore; a `--restore` boot mode + the "snapshotted" exit disposition.
**Done test (RED→GREEN):** a software-2D guest with a tight-loop counter is snapshotted; the worker exits
126 and a file appears; `--restore` resumes and the counter continues from (not before) its value,
monotonic time hasn't leapt, the desktop redraws.

### M9.2 — virtio device state + GIC
libkrun: the versioned device-state schema (serialize each virtio device); in-kernel GIC snapshot (or the
userspace `GicV3` fallback); reopen host resources on restore (block/fs fds, net unixgram → fresh gvproxy,
vsock re-handshake).
**Done test:** a full-device no-GPU guest snapshots and restores; virtio-block survives, agent re-HELLOs.

### M9.3 — GPU via Strategy A (guest-side rebuild; restore = fresh worker)
**virgl tier first (the achievable baseline):** carry the **Dongwon Kim drm/virtio freeze/restore series**
(kernel resubmit of resource/context creates) in `patches/linux/`; host-side, just ensure the snapshot
**quiesces** the guest GPU (drain fences) before the worker dies — restore brings up a **fresh worker /
fresh renderer**, so there's no in-process renderer reset to build. **venus tier (the hard half, gated on
the venus-resume spike):** a **Mesa-venus object-graph replay / `DEVICE_LOST` re-create** so the fresh host
render-server rebuilds the VkObject graph; plus the **host-visible blob copy-back** (guest
`TRANSFER_FROM_HOST` at snapshot) for zero-copy blob contents. *(Side fix, not on the restore path:
`reset_session` should also drop rutabaga contexts so the in-process reboot/rebind reset is clean —
`virtio_gpu.rs:698`.)*
**Done test:** a GPU-enabled enhanced guest snapshots + restores; the seated **virgl** desktop rebuilds
(baseline); then the **venus** desktop rebuilds, no parked-fence hang, pixel-verified (`iosdump` + human).

### M9.4 — Full-snapshot feature + suspend/resume UX
Named snapshots (save / restore / **clone** / roll back / delete); VMGenID reseed on clone; one-click
Suspend; capability probe; stock wall-clock NTP backstop; docs.
**Done test:** human-verified suspend→resume and snapshot→restore→clone on both a stock and an enhanced
image; clock correct after a real multi-hour suspend; window survives; host RAM freed while suspended.

### Summary — net-new vs libkrun
| Step | Net-new limina | libkrun patches |
|---|---|---|
| M9.1 pause+RAM+vCPU | snapshot file format/CRC, `--restore` wiring, monotonic policy | pause/quiesce; `save_state`/`restore_state`; mem dump + GPU-window skip; `CNTVOFF`; `--restore` mode |
| M9.2 devices+GIC | versioned device schema, host-resource reopen | device (de)serialize; GIC state blob |
| M9.3 GPU (Strategy A) | carry `patches/linux` Dongwon-Kim (virgl tier); Mesa-venus object-graph replay + blob copy-back (venus tier) | snapshot-time GPU quiesce (drain fences); `reset_session` rutabaga-context fix (side path) — restore = fresh worker, no in-process renderer reset |
| M9.4 feature+UX | named-snapshot manager, clone, VMGenID, UX, NTP backstop | (none) |

---

## 8. Open questions & risks

**Verified (incl. spike #3, 2026-06-28):** HVF has no dirty-page log → stop-the-world dump; guest RAM is
VMM-owned `hv_vm_map`'d host memory + an 8 GiB GPU SHM window to skip; pause/register-get-set/GIC-state are
unwired in libkrun today; true GPU serialization is unprecedented for virgl/venus and infeasible on Metal;
Parallels/VMware both do guest-backed re-init; the reuse spine (relaunch, fd-swap, surface-port, gvproxy,
port-123) exists. **And:** the host worker robustly survives virtio_gpu device resets; the clean path
cold-rebuilds the desktop; a **running guest session does NOT survive abrupt GPU-device loss** (so the
guest-side resubmit work is *required*, not optional); `reset()` keeps the renderer alive across a reset by
design and `reset_session` doesn't drop rutabaga contexts (orphaned-context bug on un-quiesced reset — side
path, since restore = fresh worker).

**Assumed / spike-gated (do NOT build on blind):** HVF full vCPU + GIC round-trip (M9.0 #2);
re-mmap+`hv_vm_map` at original IPAs behaves like first boot; the Dongwon Kim series applies cleanly to our
kernel (not confirmed upstream-merged — carry out-of-tree); **on a fresh-worker restore the guest's
kernel-driver resubmit makes virgl resources/contexts valid again (virgl tier)** — inferred, the
unbind/rebind test was *not* a faithful restore. **venus object-graph replay — ANSWERED by the venus-resume
spike (2026-06-28, `spikes/s4-hibernate/venus-resume.md`): feasible & ours to build.** The host render-server
is replay-ready (empty fresh `vkr_context`, keyed by guest object-ids) and a VM snapshot preserves those ids
in restored RAM; the work is guest-venus **retain-and-replay** (it discards create-infos today) + ring
re-establishment + host id-collision hardening. Remaining undesigned: the **kernel-resume→userspace-venus
trigger**, and dependency-ordered replay.

**Decisions made this round:** host-side snapshot over guest-side S4 (fewer failure points, unlocks
snapshots, sidesteps the spike-#1 libkrun gaps); GPU via Strategy A, not serialization (infeasible for
venus-on-Metal, unprecedented, and A is what the market leaders do); B (record/replay) explicitly deferred.

**M10 cross-dependency — the multi-disk manifest (filed here from M10 Phase 2, 2026-06-30).** A
snapshot must record the **disk set** it was taken with, because the device set is part of the VM's
identity: disks attach mid-stack (balloon→…→block→vsock→net), so adding/removing a `--disk` renumbers
the trailing vsock/net devices and would mis-restore against the versioned device-state schema (§M9.2).
Before named snapshots support multi-disk VMs, M9 must (a) persist the disk set (paths + per-disk `:ro`
+ the positional `block_id`, now the stable virtio serial — M10 patch 0038) in the snapshot, and (b) on
`--restore`, **fail closed** if the attached `--disk`/`--cdrom` set doesn't match (or, for clone, CoW
the data disks per M9.4). Until then, gate named snapshots to single-disk VMs. M10 deliberately did *not*
build this on Phase-2 argv — it has no consumer until M9 starts. See `docs/design/m10-multiple-disks.md`
§6.2.

---

## Appendix A — Guest-side Linux S4 hibernation (considered, demoted)

The earlier draft's primary. Mechanism: the agent runs `systemctl hibernate`; the guest writes its own
image to swap on its virtio-blk disk, releases the GPU via its DRM driver, PSCI-powers-off; resume
cold-boots the same kernel with `resume=`. Attractive because it needs no VMM state serialization and the
GPU is handled entirely guest-side.

**Why demoted (spike #1, `spikes/s4-hibernate/RESULTS.md`, 2026-06-28):** the guest-side path is a parade
of fragile, distro-coupled pieces, and inside libkrun it is **blocked by two HVF gaps**: PSCI
`CPU_OFF`/`AFFINITY_INFO` unimplemented (aborts multi-vCPU at `disable_nonboot_cpus()`) and an unhandled
EL1 debug sysreg `OSDLR_EL1` on the CPU-suspend path (halts single-vCPU). It also needs disk-backed swap
(stock Fedora is zram-only), `resume=` plumbing, SELinux swap labeling, and hits rough in-place virtio
freeze/restore. It only ever yields suspend-resume-once (no snapshots). The host-side snapshot **sidesteps
the libkrun gaps** (it never runs the guest suspend path) and unlocks far more, so S4 is parked. swsusp
mechanics reference kept at `spikes/s4-hibernate/swsusp-notes.md`; the no-GRUB initramfs-`resume=` trick is
recorded there too, should S4 ever be revisited as an optional alternative.

*Primary file touch-points:* `crates/limina/src/supervisor.rs` (exit disposition); `crates/limina/src/main.rs`
(fd-swap / restore relaunch); `crates/limina-vmm/src/krun/mod.rs` (snapshot save/restore hooks);
`crates/limina-proto/src/lib.rs` (Snapshot/Restore/TimeSet — update both guest binaries);
`third_party/libkrun/src/hvf/src/lib.rs`+`bindings.rs` (pause, save/restore, GIC, `CNTVOFF`);
`third_party/libkrun/src/devices/src/virtio/vsock/timesync.rs` (port-123 consumer in the agent).
