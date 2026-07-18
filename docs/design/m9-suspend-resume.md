# M9 — Suspend / resume + full VM snapshots (host-side) — design

> **STATUS: DESIGNED — direction chosen, not yet started.** Rewritten 2026-06-28 (host-side pivot);
> **corrected 2026-07-17 after an adversarial premise-validation review.** **Decision: host-side VMM
> snapshot is the primary mechanism** (Parallels-style), which *also* unlocks full VM snapshots as a
> feature. Guest-side Linux S4 hibernation — the earlier draft's primary — is demoted to Appendix A after
> spike #1. Every non-obvious claim carries a `path:line` into `third_party/libkrun/`/`crates/`, or a URL.
> **VERIFIED** = read this round; **ASSUMED** = needs a spike.
>
> **The 2026-07-17 review** confirmed host-side-primary and the build order, but surfaced: (1) a **trigger
> gap** — nothing runs the guest's GPU freeze/restore in an external-pause snapshot → **companion decision
> doc `docs/design/m9-freeze-trigger.md`** (agent-coordinated suspend-to-idle bracket); (2) **venus
> device-local content** is the real long pole, not vCPU/GIC (retired by spike #2); (3) the **virgl
> ≈ transparent** premise is unverified (needs its own spike); (4) stale claims fixed inline — `reset_session`
> rutabaga fix **already shipped** (patch 0035), libkrun **has** a PL031 RTC, the FFI bindings all exist.

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
  GPU resources** — exactly as on a real-hardware power cycle. *Guest-backed* resource contents
  (`ATTACH_BACKING`/blob) ride the RAM snapshot; **GPU-generated content does not** — window backbuffers
  and FBO renders live host-side and come back stale, but compositors redraw every frame so on virgl this
  self-heals as a transient flicker. The real exception is **venus device-local memory** (textures that
  are never in guest RAM), which needs an explicit readback — see §4(b). "Contents ride the snapshot for
  free" is only *partly* true; §4 has the full justification.

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

The cost, eyes open: it's the **heavier upfront VMM build**, and **HVF has no first-class dirty-page log**
→ a stop-the-world full RAM dump (a multi-second stall for a 4–12 GiB desktop). Fine for suspend; a UX
note for "snapshot a live VM". (Not *impossible* to pre-copy: `hv_vm_protect` is bound
(`third_party/libkrun/src/hvf/src/bindings.rs:4667`), so a write-protect + permission-fault DIY dirty
log — QEMU's hvf accelerator does exactly this — is a future option. Stop-the-world is the right M9
scope; the earlier "no pre-copy is possible" absolutism was wrong and is dropped.) The multi-second
stall also means the §1 "in a second or two" headline needs lz4 + sparse writes to hold for a large
guest, or it should be read as the *small-guest* case — see §M9.4.

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
| **`CLOCK_REALTIME` (wall clock)** | libkrun **has a PL031 RTC** (`third_party/libkrun/src/devices/src/legacy/rtc_pl031.rs`) — it's just `Instant`-anchored (`:75,91`), not wall-clock-anchored; the fix is already scoped in [[limina-guest-clock]]. With the freeze bracket (`m9-freeze-trigger.md`) the guest's own `timekeeping_resume()` re-reads that RTC on wake → `CLOCK_REALTIME` restored for free, **no port-123 consumer needed** on the enhanced tier. libkrun's port-123 timesync (`timesync.rs`, `TSYNC_PORT = 123`, "long nap" path) remains as a fallback but has no guest consumer today. Stock tier degrades to NTP: honest bound — Fedora chrony's default `makestep 1.0 3` only steps in the first 3 post-boot updates, so after a multi-hour suspend the stock wall clock **slews** (≤~83 ms/s) and is wrong for hours; add a one-shot step on the stock resume path (§M9.4). |
| **Networking / gvproxy** | gvproxy relaunched (existing reboot path); guest NM re-DHCPs against the static `.2` lease; in-flight TCP lost (acceptable lid-close semantics). |
| **16 KiB-host / 4 KiB-guest pages** | RAM dump is host-page-agnostic (`hv_vm_map`'d host memory); restore re-maps the same IPAs. **Skip the 8 GiB GPU SHM window** (re-pointed at renderer memory) and optionally balloon-inflated pages. A snapshot restores into a worker configured **identically** (RAM size especially — `num_physpages`-equivalent). |
| **Stale host-visible blob mappings across restore** | At snapshot the guest holds live PTEs into the 8 GiB SHM window (`arch/src/aarch64/mod.rs:62`) for mapped blobs; on the fresh worker those IPAs are **unmapped until venus replay re-issues each `MAP_BLOB`** → a guest touch before then faults on a non-MMIO IPA (unrecoverable vCPU error today). Offsets are guest-chosen, so the device-state schema must **record the mapped-blob set (resource id, offset, size)** and the restore path must **re-establish (or placeholder-map) them before vCPUs run**, not after userspace thaws. rutabaga's own snapshot contract ("the VMM must re-attach backing and re-map memory after restore") is upstream confirmation this bookkeeping is mandatory. The freeze bracket (`m9-freeze-trigger.md`) narrows the window but does not remove it. |
| **Uniqueness / entropy** | Only a hazard if a snapshot is *cloned* (the full-snapshot feature). Ship a cheap VMGenID-style reseed notification; Linux ≥5.18 reseeds its kernel PRNG. |

---

## 4. The GPU: Strategy A, and why (the load-bearing section)

> ### ⚠️ M9.3 ADVERSARIAL REVIEW (2026-07-18, Fable agent) — the SHM-window correction
> A source-level review (verified against `third_party/` + the guest Mesa tree at `/Volumes/mesa-cs`)
> found the M9.3 plan rests on a **false premise** and reshaped the build order. Load-bearing findings:
> - **"Guest-backed GPU contents ride the RAM snapshot for free" is FALSE for venus, and worse than
>   false.** `dump_ram` skips every region at `gpa >= shm_start_addr` (`lib.rs:483-490`). Guest **venus**
>   sets *both* `shmem_blob_mem` *and* `bo_blob_mem` to `VIRTGPU_BLOB_MEM_HOST3D`
>   (`vn_renderer_virtgpu.c:1556,1626`), so its ring, reply shmem, and every mappable `VkDeviceMemory`
>   are host allocations mapped into **exactly the skipped window**. The snapshot therefore captures
>   **live guest PTEs into a window the fresh worker has nothing mapped at → first GPU touch on restore
>   is an unrecoverable vCPU fault, before any replay code runs.** A **fault-safe SHM remap on restore
>   is priority-zero** (the §3 "mapped-blob set" row), not bookkeeping. (Modern virgl has a HOST3D-blob
>   path too; classic virgl resources are guest-shadowed and do ride the snapshot.)
> - **"virgl ≈ transparent" is REFUTED.** vrend keeps a per-context hash of guest-handle-keyed
>   sub-objects (shaders/CSOs/surfaces) built *only* by decoding `CREATE_OBJECT` in `SUBMIT_3D`
>   (`vrend_decode.c:1959`); the kernel never sees them and can't resubmit, and guest Mesa creates each
>   CSO exactly once (`virgl_context.c:345-352`). So a fresh vrend context has the **same
>   host-object-graph-gone problem as venus** — smaller, but real. Escape hatch: vrend has GL robustness,
>   so the natural stock outcome may be "GL clients crash, gdm respawns" rather than "guest wedges."
> - **Dongwon-Kim series: real but narrower than assumed.** v5 (Oct 2025, 3 patches), **UNMERGED**, we
>   carry none. It fires only on **S4 hibernation** — *not* our s2idle bracket — so "carry the series" is
>   really "fork an unmerged, S4-scoped series and re-trigger it ourselves." It restores BO identity
>   (`RESOURCE_CREATE` + `ATTACH_BACKING`) only — not contents, streams, or host context state.
> - **Biggest unknown = the honest floor:** (i) guest death (stale SHM-window PTEs fault) vs (ii) session
>   crash-and-recover (gdm respawns). Decided next step (**user, 2026-07-18**): **run the floor spike
>   first** — the existing M9.2 bracket on a *windowed* guest (vrend/4k + venus), restore into a fresh
>   worker, and observe survival / faulting GPA (≥ `shm_start_addr`) / gnome-shell recovery. ~1 day, no
>   kernel patches. **Venus v1 target (user): SEAMLESS** (full retain-and-replay + device-local content
>   readback) — the floor spike still comes first, but the eventual venus goal is transparent survival,
>   not "apps restart."
> - **Revised M9.3 order:** floor spike → **fault-safe SHM remap on restore** → carry DK patch-1
>   (freeze/`del_vqs`, adapted to s2idle; deletes the M9.2 GPU exception) → DK patches 2-3 + our s2idle
>   trigger (BO resubmit) → Mesa-virgl CSO recovery (retain-replay *or* robustness/reset-notify) → venus
>   retain-and-replay + content readback (seamless target, its own sub-project).
> - **Retire the Parallels comparison as evidence for our virgl tier** — whatever it does, it is not
>   "stock kernel resubmit + stock Mesa," because stock Mesa demonstrably cannot rebuild vrend sub-objects.
>
> ### FLOOR SPIKE — round 1 result (2026-07-18, `spikes/m9-freeze-trigger/m93-floor-windowed.sh`)
> Ran the M9.2 bracket on a **windowed venus** enhanced guest (Fedora-44.enhanced.test, 4 vCPU/4 GiB),
> instrumented with the new libkrun 0066 SHM-window-fault oracle.
> - **✅ Phase 1 — windowed venus SUSPEND+SNAPSHOT works.** The seated GNOME/venus desktop quiesced in
>   ~1 s on the GPIO suspend button (GNOME did **not** inhibit it) and snapshotted cleanly (4.3 GB,
>   worker exit 126). So a GPU-bearing suspend is reachable at the snapshot level — the M9.2 mechanism
>   composes with a live GPU (virtio-gpu stays `DRIVER_OK`, excepted by the oracle; everything else
>   quiesces).
> - **❌ Phase 2 — the restored guest does NOT come back.** Fresh worker restored RAM+vCPU+GIC+GPIO and
>   injected the wake, but: **black window**, **SSH never returns** (gvproxy "no route to host"), worker
>   **0.4 % CPU / 83 MB RSS** (guest idle/asleep, not spinning), and — critically — **the SHM-window
>   fault oracle NEVER FIRED.** So the guest stalls **earlier** than Fable's predicted "faults on first
>   GPU-window touch": it never reaches a userspace GPU access at all. Contrast the **headless** M9.2
>   restore, which SSHes back in ~3–6 s — so the GPU/display config specifically breaks resume.
> - **Open question (round 2):** *where* does it stall? Candidates: (B) the wake didn't wake it (still
>   frozen in s2idle — but headless wake works, same mechanism) vs a **kernel-resume hang** (dpm_resume
>   blocked on a device with the GPU/display present) that parks the vCPUs. Near-zero CPU + tiny RSS
>   favours "idle/asleep, never really ran." **Next:** re-run the restore with `RUST_LOG=krun_vmm=info`
>   (the per-vCPU "resumed from snapshot at pc=" lines) + a captured guest console to pin the stall PC —
>   before deciding whether the first M9.3 build item is the wake/GIC-with-GPU path or the SHM remap.
>
> ### FLOOR SPIKE — round 2 (2026-07-18): the stall was a latent **≥4 GiB snapshot-format bug**, now FIXED
> Chain of controls (headless, `m93-headless-control.sh`, so the GPU is out of the picture) isolated the
> round-1 "black restore" to **RAM size, not the GPU and not the enhanced kernel**:
> - enhanced 2 vCPU / **1024** MiB → **restores** (SSH back, same boot_id)
> - enhanced 2 vCPU / **4096** MiB → **stalls**  ← vCPU held at 2, only RAM changed → RAM is the variable
> - enhanced 4 vCPU / **4096** MiB → **stalls**
>
> Round 1's "headless SSHes back in ~3–6 s" baseline was run at **≤2 GiB** — so the headless path was
> *never actually a working 4 GiB control*; the GPU was wrongly implicated. **Root cause:** the snapshot
> file format (`vmm/src/snapshot.rs`) wrote every byte-section length as a **u32**. A guest-RAM region at
> 4096 MiB is `0x1_0000_0000` bytes; `len as u32` truncated it to **0**. `write` still appended the full
> 4 GiB (so the file was ~4.3 GB and the trailing CRC matched — a silent pass), but on restore `bytes()`
> read length 0 → an **empty** region → the restore loop wrote **zero bytes back** → the guest resumed
> into blank RAM and idled (WFI, ~0 % CPU, no SSH). Triggers strictly at region length ≥ 2³², so every
> prior M9.1/M9.2 test (all ≤2 GiB) masked it. **Fix:** widen the length prefix to **u64** (VERSION 2→3,
> fail-closed on the old version), libkrun **patch 0067**. **Verified GREEN:** headless enhanced 4 vCPU /
> 4096 MiB now restores, SSH back in ~12 s, **SAME boot_id** (resumed). Snapshot grew by exactly 12 bytes
> (3 byte-sections × 4) — confirms the u64 code ran. **A hard M9.1/M9.2 prerequisite that had to land
> before M9.3 GPU work was meaningful** (real VMs are ≥4 GiB).
>
> **Now the windowed-restore stall is genuinely the M9.3 GPU question** (headless 4 GiB works; only the
> GPU/display path still breaks) — the SHM-window HOST3D blobs Fable flagged. That is the next build item
> (fault-safe SHM remap on restore), no longer confounded by the RAM-size bug.

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

> ⚠ **The trigger gap (architectural, resolved in a companion doc).** This series works through the guest's
> **kernel PM callbacks** — but a host-side snapshot pauses the vCPUs *externally* and the guest **never runs
> its resume path**, so `.restore` (hence the resubmit, hence the venus replay one level up) **never fires**.
> The resubmit mechanism the whole GPU story rests on has no invocation path unless we build one. Decision:
> an **agent-coordinated shallow-sleep (suspend-to-idle) bracket** on the enhanced tier makes the guest run
> its own PM freeze/resume around the snapshot — which *also* restores the wall clock (`timekeeping_resume`),
> dissolves the `ICC_RPR_EL1` mid-IRQ-service edge, and drains virtio I/O. The stock tier keeps the raw
> non-cooperative snapshot (GPU re-init blip). Full analysis, options, and the gating feasibility spike F in
> **`docs/design/m9-freeze-trigger.md`**.

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
  - **(b) resource *contents*, not just objects — the under-scoped part, and the true long pole.** Object-
    graph replay (a) re-creates every `VkObject` **empty**. Where do their *bytes* come from?
    - *Host-visible / zero-copy blobs* (bytes in host GPU memory, mapped into the guest): have the **guest
      GPU** `TRANSFER_FROM_HOST` them into guest-RAM staging at snapshot time so the RAM snapshot captures
      them; re-upload on restore. Coherency-correct (dodges #28), no opaque-Metal serialization. This is the
      narrow "zero-copy exception" the earlier draft named.
    - *Ordinary device-local `VkDeviceMemory`* (**the miss**): most textures, render targets, and the
      compositor's cached GPU surfaces are **non-mappable device-local** memory living in Metal heaps — not
      a virtio-gpu resource, so **captured by nothing.** Replay brings the objects back empty and apps that
      upload once at startup return with **garbage that never self-heals** (unlike framebuffers, which
      redraw). Closing this needs a **snapshot-time venus readback sweep** — `vkCmdCopyImageToBuffer` /
      `CopyBuffer` every live device-local allocation into guest-RAM staging (which is essentially what
      gfxstream's snapshot does), re-uploaded on restore. Real cost in quiesce time + staging memory, and
      it is why **the venus tier — not the vCPU/GIC machinery — is M9's long pole and its headline risk**
      (an accelerated desktop *surviving* suspend). The shippable M9 may well be: **virgl tier seamless +
      venus tier "re-enumerates fresh, GPU apps restart"** as the honest floor, with seamless venus as a
      follow-on. Flag this to the user before committing to seamless-venus scope.

**Restore uses a fresh worker, not an in-process reset.** A real restore cold-boots a brand-new worker
(empty rutabaga) and the guest resubmits/replays into it — so there is **no host renderer to reset in
place** and no stale-context collision. (Aside, spike #3: an *abrupt in-process* device reset is **not**
clean — `reset()` keeps the renderer alive by design (`device.rs:379`). The orphaned-context half of this
is **already fixed**: `reset_session()` now calls `rutabaga.reset_session_state()` to drop leaked
contexts/resources (`third_party/libkrun/src/devices/src/virtio/gpu/virtio_gpu.rs:715`, carried as
`patches/libkrun/0035-limina-virtio-gpu-drop-leaked-contexts-resources-on-.patch`, shipped 2026-06-29).
So this is no longer M9 work — it's a done side fix for the reboot/rebind path, **off the restore
critical path**. libkrun 0022 / `venus_reset` — the renderer surviving a reset — is an *intra-process*
property, relevant only to that in-place path, **not** to restore.)

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
(transparent, no guest cooperation). Note spike #3's lesson, stated honestly: without guest resubmit support
a live session does **not** survive GPU re-init — round 3 crashed gnome-shell + every 3D client. So the
stock floor's real promise is **save/restore of the machine, and on resume the GPU comes back via a fresh
renderer with a visible disruption that, with live 3D clients, may cost the session (it recovers — boots and
is usable, per the two-tier guarantee).** Not the earlier doc's rosy "blip." Monotonic clock continuous
(host `CNTVOFF`); wall clock stepped once on resume then NTP (§3). If quiesce/snapshot can't proceed,
suspend falls back to a clean power-off.

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
2. ✅ **GREEN (2026-07-01, `spikes/m9-hvf-state-roundtrip/RESULTS.md`).** HVF round-trips full vCPU +
   in-kernel-GICv3 state into a **fresh same-process VM** and the guest continues identically
   (checksum-verified, 5/5 runs; a timer armed pre-snapshot fires correctly post-restore). Key facts:
   **118/120 EL1 sysregs read OK and ALL accepted `set_sys_reg` post-run — zero rejections** (the 2
   get-failures are EL2 regs, expected); the GIC blob (126 KB binary plist) `set_state`s into a fresh VM
   and reads back **byte-identical**; ICC CPU-interface regs are NOT in the blob (save/restore via
   `hv_gic_get/set_icc_reg`; only `ICC_RPR_EL1` refuses set — read-only running-priority → **quiesce to
   no-IRQ-in-service before snapshotting**); same-process `hv_vm_destroy`→`hv_vm_create` works;
   `hv_vcpu_set_vtimer_offset` accepts sets (the CNTVOFF continuity mechanism is confirmed); with the
   in-kernel GIC **`VTIMER_ACTIVATED` never fires** (the vtimer mask dance is a userspace-GIC artifact).
   Restore order that worked: vm → gic create → map RAM → vcpu create → **MPIDR first** → `gic_set_state`
   → ICC → sysregs → GP/PC/CPSR/SIMD → vtimer offset+mask → pending lines. Non-gating deltas left to
   M9.1/M9.2: multi-vCPU, SPIs pending at snapshot, snapshot mid-IRQ-service, MMU-on (Linux is the real
   test). **M9.1/M9.2 are unblocked; the userspace-GicV3 fallback is not needed.**
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
`Resume`; `resume_vcpus` is a no-op today — `vmm/src/lib.rs:304`). **The pause must also wake vCPUs parked
in `wait_for_event`** — that's a plain channel `recv()` (`vmm/src/macos/vstate.rs:506`) which
`hv_vcpus_exit` does **not** kick, so a WFI/WFE-idle vCPU is a distinct pause path. Then
**`HvfVcpu::save_state`/`restore_state`** (the FFI already exists — `hv_vcpu_get/set_sys_reg`,
`hv_vcpu_set_simd_fp_reg`, `hv_vcpu_set_vtimer_offset` are all bound in `bindings.rs`; this is wrappers +
plumbing, not new FFI); memory dump/reload (skip the GPU SHM window) with a two-file split (small vmstate +
sparse mem) + CRC; `CNTVOFF` set on restore (**never step `CNTVCT` backward even one tick** — the
dogfood-guest year-2119 wrap, [[limina-guest-clock]], is the failure mode); a `--restore` boot mode + the
"snapshotted" exit disposition.
**Multi-vCPU from the first RED test** — every real desktop VM is multi-vCPU and per-vCPU **ICC/MPIDR
ordering** is exactly where spike #2 (single-vCPU) stopped; don't defer it.
**Done test (RED→GREEN):** a **multi-vCPU** software-2D guest with a tight-loop counter is snapshotted; the
worker exits 126 and a file appears; `--restore` resumes and the counter continues from (not before) its
value, monotonic time hasn't leapt, the desktop redraws.

### M9.1.5 — Spike F: the guest freeze/restore trigger (gates M9.2's serialization contract)
Before designing the device-state schema, run **spike F** (`docs/design/m9-freeze-trigger.md` §5): does an
enhanced guest inside libkrun enter suspend-to-idle (`echo freeze > /sys/power/state`) and wake cleanly
*without* tripping a spike-#1-style HVF gap, and does the virtio-gpu resubmit path fire on that transition?
The answer decides whether M9.2 serializes a **quiesced** guest (Option 1 — drained queues, no IRQ in
service) or a **mid-flight** one (raw stock path) — a materially different schema.

### M9.2 — virtio device state + GIC
libkrun: the versioned device-state schema (serialize each virtio device — **shaped by spike F**: quiesced
vs mid-flight queues); **the mapped-blob set** (resource id/offset/size, re-established before vCPUs run —
§3); in-kernel GIC snapshot (the userspace `GicV3` fallback is **not needed** per spike #2); reopen host
resources on restore (block/fs fds, net unixgram → fresh gvproxy, vsock re-handshake). **Fold in the
virtio freeze/thaw hardening** spike #1 surfaced (`update virtio queue in invalid state 0x8f`, net wedged
after thaw) — it's **on-path** the moment Option 1's bracket runs the guest's freeze callbacks.
**Done test:** a full-device no-GPU guest snapshots and restores; virtio-block survives, agent re-HELLOs;
with the freeze bracket, network recovers after thaw.

### M9.3 — GPU via Strategy A (guest-side rebuild; restore = fresh worker)
**virgl tier first (the achievable baseline):** carry the **Dongwon Kim drm/virtio freeze/restore series**
(kernel resubmit of resource/context creates) in `patches/linux/`; host-side, just ensure the snapshot
**quiesces** the guest GPU (drain fences) before the worker dies — restore brings up a **fresh worker /
fresh renderer**, so there's no in-process renderer reset to build. **venus tier (the hard half, gated on
the venus-resume spike):** a **Mesa-venus object-graph replay / `DEVICE_LOST` re-create** so the fresh host
render-server rebuilds the VkObject graph; **plus a snapshot-time venus readback sweep for device-local
resource contents** (§4b — the long pole) and the host-visible blob copy-back for zero-copy blobs. The
resubmit/replay is invoked by the **freeze bracket** (`m9-freeze-trigger.md`) on the enhanced tier; the
stock tier accepts the raw-reset blip. *(The `reset_session` rutabaga-context drop is **already shipped** —
patch 0035, `virtio_gpu.rs:715` — not M9 work.)*
**Done test:** a GPU-enabled enhanced guest snapshots + restores; the seated **virgl** desktop rebuilds
(baseline); then the **venus** desktop rebuilds with correct texture contents (not just empty objects), no
parked-fence hang, pixel-verified (`iosdump` + human).

### M9.4 — Full-snapshot feature + suspend/resume UX
Named snapshots (save / restore / **clone** / roll back / delete); VMGenID reseed on clone; one-click
Suspend; capability probe; **stock wall-clock one-shot step on resume** (not just slew — see §3
CLOCK_REALTIME); **lz4 + sparse RAM writes** to make the "second or two" headline hold for a large guest;
docs. **Point-in-time disk capture** — a live snapshot-then-keep-running diverges the disk from the frozen
RAM/device state, so restoring later corrupts the fs; take an **APFS `clonefile()`** of each data disk at
the pause point (cheap, CoW) and bind it into the snapshot manifest (§8 already scopes disk-*set* identity;
this adds disk-*contents* identity). Until built, gate live snapshots to a clean pause.
**Done test:** human-verified suspend→resume and snapshot→restore→clone on both a stock and an enhanced
image; clock correct after a real multi-hour suspend; a cloned snapshot's disk is independent of the
original; window survives; host RAM freed while suspended.

### Summary — net-new vs libkrun
| Step | Net-new limina | libkrun patches |
|---|---|---|
| M9.1 pause+RAM+vCPU | snapshot file format/CRC, `--restore` wiring, monotonic policy | multi-vCPU pause/quiesce (incl. WFE-parked wakeup); `save_state`/`restore_state` (wrappers over existing FFI); mem dump + GPU-window skip; `CNTVOFF`; `--restore` mode |
| M9.1.5 spike F | freeze-bracket feasibility (agent + enhanced kernel) | — (guest-side; see `m9-freeze-trigger.md`) |
| M9.2 devices+GIC | versioned device schema (+ mapped-blob set), host-resource reopen | device (de)serialize; GIC state blob; virtio freeze/thaw hardening (`invalid queue state 0x8f`) |
| M9.3 GPU (Strategy A) | carry `patches/linux` Dongwon-Kim (virgl tier); Mesa-venus object-graph replay + **device-local content readback** + blob copy-back (venus tier); freeze-bracket trigger | snapshot-time GPU quiesce (drain fences) — restore = fresh worker. *(`reset_session` rutabaga fix already shipped, patch 0035.)* |
| M9.4 feature+UX | named-snapshot manager, clone, VMGenID, UX, stock resume clock-step, APFS `clonefile` disk capture, lz4/sparse RAM | (none) |

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
venus-on-Metal, and A is what the market leaders do — though note gfxstream *does* implement rutabaga
snapshot/restore, so GPU serialization is not strictly "unprecedented"; it's just closed to virgl/venus-on-
Metal); B (record/replay) explicitly deferred.

**Risk reframing (2026-07-17 review):** spike #2 **retired** the vCPU + GIC round-trip as the top risk — it
is now *low* (118/120 sysregs, byte-identical GIC blob, timer-across-the-gap), and the FFI bindings all
already exist, so M9.1's libkrun surface is wrappers + run-loop pause wiring, not new plumbing. **The actual
long pole is the venus tier's device-local content capture** (§4b) — an accelerated desktop *surviving*
suspend with correct texture contents. **Second:** the "virgl ≈ transparent" claim (§4) is an *unverified
inference* — unlike venus it never got a source spike; run a **virgl-tier source spike alongside M9.1**
(are vrend `CREATE_OBJECT` handles guest-assigned + per-context, replayable from the resubmitted stream?
does Mesa virgl retain enough, or lean on GL robustness/reset-notify?). If it collapses, both tiers become
Mesa-replay-shaped and M9.3 roughly doubles. **Third:** the freeze-trigger gap (`m9-freeze-trigger.md`),
now decided but spike-F-gated.

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
