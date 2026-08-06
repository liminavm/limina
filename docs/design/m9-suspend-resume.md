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

> ### ⛔ CORRECTION (Fable round-2 review, 2026-07-18) — the "unrecoverable fault" / "SHM remap" premise is FALSE
> A second source-level review + a direct code check **refute the two starred claims above**. Strike them:
> - **The SHM window is NOT unmapped on a fresh restore worker.** The GPU window is created and appended to
>   `arch_mem_regions` (`builder.rs:1730-1743`), and `Vm::memory_init` `hv_vm_map`s **every** `guest_memory`
>   region — the window included — on *every* boot, **restore included** (`macos/vstate.rs:111-131`). So on
>   restore the window is mapped to **fresh anonymous zero pages**; a guest touch there **does not fault, does
>   not exit, is invisible to the host** (it reads zeros / writes into memory nobody consumes). ⇒ "first GPU
>   touch on restore is an unrecoverable vCPU fault" (lines 115-116) is **wrong**, and **"fault-safe SHM remap
>   is priority-zero" (lines 116-117, 136) targets a fault that cannot occur** — option (a) "remap the window"
>   is already the accidental status quo and is demonstrably not enough (zeros ≠ contents + a live ring
>   consumer). The *contents-are-gone* half of the original review stands; the *fault-mechanics* half does not.
> - **The libkrun 0066 SHM-window-fault oracle is structurally dead code** — it only observes MMIO exits, and
>   the window never produces one. Round-1's "SHM fault never fired" was **guaranteed silence**, not evidence
>   of "stalls before any GPU touch." Re-point it at the only real hole (`resource_unmap_blob` `hv_vm_unmap`
>   without rebacking, `virtio_gpu.rs:2126-2136`) or delete it; its comments (`vstate.rs:223,379`) encode the
>   false premise. (Also noted: `EC_DATAABORT` decoder never checks `isv` — `hvf/src/lib.rs:1024-1035` — a
>   pre-existing decode hazard for a real window hole; add to the upstreaming/security list.)
> - **The REAL first wall is a virtio-gpu device-state mismatch, not a fault.** M9.2 excepts virtio-gpu from
>   quiesce (no s2idle PM ops, `lib.rs:423-424`), so the restored guest kernel believes the GPU is `DRIVER_OK`
>   with configured virtqueues, while the fresh worker built a **fresh device in `INIT` with no queues**. The
>   guest never re-reads `device_status`; its first frame submission kicks an unconfigured device → the kick is
>   dropped, fences never signal, the vn ring seqno (read from zero-backed window) never advances → the
>   compositor parks in a **silent ring/fence wait forever**. No fault, no crash, no log line.
> - **Two cheap snapshot-hardening items surfaced (do while cheap):** (i) **no memory-layout identity check on
>   restore** — `builder.rs` validates only vCPU count; restoring a 4 GiB snapshot into an 8 GiB worker
>   succeeds silently but `shm_start` moves → split-brain corruption. Embed ram-size/`shm_start`/firmware-mode
>   in the header, fail closed. (ii) **~2×-RAM peak host allocation** — `dump_ram` copies every region into
>   `Vec`s and `snapshot::write` serializes the whole file into a second `Vec` before one `fs::write`; a 4 GiB
>   guest ≈ 8.6 GB transient, an 8 GiB guest thrashes a 32 GB host. Stream sections with an incremental CRC.
> - **Corrected M9.3 ladder (supersedes line 136):** **Step 0** snapshot hardening (host-only: layout-identity
>   check + streamed CRC; fix/kill the dead oracle) → **Step 1** corrected round-3 floor spike (§round-2 below;
>   drop the SHM-fault oracle from the verdict, add guest-side wedge probes) → **Step 2** v1 floor =
>   [REVISED r3 — see below] host-side virtio-gpu TRANSPORT-STATE restore (libkrun-only, STOCK-compatible),
>   which triggers a self-driven venus-watchdog crash→gdm-respawn chain; agent unbind/rebind demoted to
>   fallback → **Step 3** DK series adapted to s2idle (guest kernel:
>   freeze/`del_vqs` deletes the M9.2 GPU exception, then BO resubmit) for virgl-tier seamlessness → **Step 4**
>   seamless venus = Mesa retain-and-replay + ring re-establishment + host `vkr` id-collision hardening
>   (graceful, not `assert` at `vkr_context.h:223`) + device-local content sweep. Host-only: Step 0 + Step 2.
>   Guest-agent: Step 2 fallback only. Guest kernel: Step 3. Guest Mesa + host virgl: Step 4.
>
> **↪ Step 2 REVISED (Fable r3) — ✅ CONFIRMED by round 5 (see the round-5 block + snapshot-v4 spec below;
> unbind/rebind FALLBACK now judged LIKELY-DEAD, since unkillable `vram_mmap` waiters block driver removal):**
> the spike showed the fault is a **dead virtio-gpu transport** (fresh
> worker's device never re-driven to `DRIVER_OK`), not a missing scanout or an idle compositor. So v1 floor =
> **host-side GPU transport-state restore** — serialize the GPU device's MMIO register file (queue
> desc/avail/used addrs, `ready`, features, `device_status`) + per-queue processed indices at snapshot (the
> rings themselves already ride the RAM snapshot), restore before the vCPU gate (`builder.rs:1234-1243`, beside
> GIC/GPIO; VERSION bump). The recovery chain then fires with **zero guest changes**: commands process again →
> `SET_SCANOUT` vs the vanished resource errors but its fence completes → the parked kernel commit unparks →
> mutter renders through venus → the dead venus ring trips Mesa's ~3 s ring-ALIVE watchdog `abort()`
> (`vn_common.c:281-289`, `vn_ring.c:456-457`) → gnome-shell dies → **gdm respawns → fresh session re-inits
> venus against a now-live device → desktop back (~10-20 s "screen blinks")**. Stock-tier (no agent/kernel/Mesa
> patch). FALLBACK = agent stop-gdm→unbind→rebind→start-gdm (enhanced) if the respawn chain stalls. ELIMINATED:
> bare `systemctl restart gdm` / display-only kick (device dead for *any* client → D-state hang at init).
> **"Seamless-lite" (session preserved) is structurally impossible on stock Mesa** — venus ring-death is a
> hardcoded `abort()`, not a recoverable `DEVICE_LOST`, and the compositor is itself a zink→venus client. What
> survives the floor: VM, disks, network, ssh/CLI, all non-GPU processes.
>
> ### FLOOR SPIKE — round 3 (2026-07-18, post-u64-fix, corrected oracles) — the honest floor is a SOFT idle, not a hard wedge
> Re-ran `m93-floor-windowed.sh` after patch 0067, with the dead SHM-fault oracle demoted out of the
> verdict and guest-side wedge probes added (gnome-shell `ps`/`wchan`/`/proc/PID/stack`, dmesg, journal
> over SSH; human eyeball on the restore window). Enhanced venus, 4 vCPU / 4 GiB, windowed.
> - **OS survived:** SSH back ~12 s, **boot_id SAME (resumed)**. Snapshot 4.30 GB, worker exit 126.
> - **Desktop: BLACK** (human eyeball, restore window).
> - **gnome-shell is NOT hard-wedged.** State `Ssl`, `wchan=hrtimer_nanosleep`; kernel stack is a plain
>   `clock_nanosleep` main-loop timed sleep (`hrtimer_nanosleep → __arm64_sys_clock_nanosleep → el0_svc`),
>   S (interruptible) — **not** a `D`-state virtio-gpu/dma-fence park. `gnome-shell-cal` in
>   `poll_schedule_timeout` (normal). ⇒ **Fable's r2 "compositor parks in a ring/fence wait forever"
>   mechanism is REFUTED at the kernel level.** The process is alive and unblocked; the screen is just black.
> - **dmesg clean:** virtio_gpu init fine at boot (`Host memory window 0x180000000 +0x200000000`, +virgl
>   +resource_blob +host_visible +context_init, 1 scanout); **no post-restore drm/fence/timeout/gpu-hang.**
> - **journal:** only benign restore churn — `limina-agent: channel error (…107); reconnecting` (fresh-worker
>   vsock re-handshake), `virtio_blk` queue re-init — no compositor crash, no venus/vulkan error.
> - **Present-path telemetry: MISSING** — ran at `krun_vmm=info`, so no per-frame `FLUSH2`/`FENCEPRESENT`
>   DIAGs (those are `trace`). Worker log had only benign vrend format-probe noise. So "guest not rendering"
>   vs "guest renders but fresh worker has no scanout resource / present target" is **not yet disambiguated.**
 > - **⚠️ r3 read CORRECTED (Fable r3 addendum):** "gnome-shell in `nanosleep`" is **consistent with a
>   stalled pipeline, not exculpatory.** Mutter's frame clock waits for flip-done in *userspace* (its normal
>   poll/timer loop — the `S`-state `clock_nanosleep`), so a permanently-stuck nonblocking atomic commit looks
>   exactly like a healthy idle loop. The r3 process-state oracle refuted the *D-state* detail, not the
>   stalled-pipeline model. **Revised mechanism:** on resume, logind `PrepareForSleep(false)` → mutter
>   re-enables the CRTC with the pre-suspend FB via a **nonblocking atomic commit** (no new render, hence no
>   venus activity/log line); that commit's `commit_work` runs in a **kernel worker that parks forever** on a
>   virtio-gpu fence that can't arrive, because the fresh worker's **virtio-gpu transport is unconfigured** —
>   the guest never re-drives `DRIVER_OK` (M9.2 GPU quiesce exception, `lib.rs:423-424`), so the fresh
>   `MmioTransport` never activates its queues (`devices/src/virtio/mmio.rs:499-509,545`) and kicks are
>   swallowed. The guest virtio-gpu driver has **no fence-timeout/hang-check**, so **clean dmesg is the
>   EXPECTED signature of this failure, not evidence against it**; the virtio_gpu "Host memory window…" lines
>   are boot-time, riding the RAM snapshot. So it's not idle-compositor and not "renders-but-no-scanout" — it's
>   one layer earlier: **commands aren't processed at all (dead transport)**; the missing scanout resource is
>   real but not yet reached.
> - **Disambiguating experiment (one re-run, machine oracles — no eyeball):** phase-2 at
>   `RUST_LOG=limina_vmm=debug,krun_vmm=debug` + over root SSH `ps axo pid,stat,wchan:32,comm | awk '$2 ~ /^D/'`
>   (predict a `kworker` in D-state on `commit_work`/fence while gnome-shell stays S) + `cat
>   /sys/kernel/debug/dri/0/state` (stuck commit) + host log shows **zero** virtio-gpu queue activity. That
>   combination = transport-dead confirmed. (Alt branch: if the guest shows *no* pending commit, the
>   resume-notification/output-re-enable path is the gap — an even cheaper fix.)
>
> ### FLOOR SPIKE — round 4 (2026-07-18, debug confirmation) — NEITHER theory cleanly; needs a clean time-series
> Host confirms the restore mechanics: **`restore: injecting guest wake (KEY_WAKEUP) to resume from s2idle`**,
> all 4 vCPUs resume. At **t≈12 s** post-restore (clean machine oracles):
> - **NO GPU `kworker` in D-state** (sweep header, zero `D` rows; one transient `D` proc was in
>   `anon_pipe_read`, unrelated). ⇒ **Fable r3's "commit_work parked on a virtio-gpu fence" is ABSENT at 12 s.**
> - **DRM CRTC DISABLED, not stuck:** `dri/0/state` → `crtc-0 enable=0 active=0`, planes `crtc=(null) fb=0`,
>   connector `Virtual-1 crtc=(null)`. Display pipeline **off**; mutter did NOT re-enable the output.
> - **Host: ZERO virtio-gpu command traffic** post-restore (debug). Guest isn't submitting GPU commands.
> - gnome-shell: normal `nanosleep`; dmesg clean.
>
> **BUT by t≈3 min the load climbed to ~23 and `ps` itself D-hung** (reading `/proc` of many D-state procs
> blocks) — something D-piles up over minutes that was absent at 12 s. **CONFOUNDED:** the run's own
> `vulkaninfo` liveness probe (self-D-hangs on the GPU) + my concurrent SSH poking contaminate the 3-min
> sample. Honest state = **AMBIGUOUS**; two live hypotheses:
> - **(A) resume-notification gap** — display never re-enabled (logind `PrepareForSleep(false)` / mutter output
>   re-enable never fires); no GPU touch ever. Cheap fix (drive the resume / re-enable). Favoured by the clean
>   12-s data (CRTC off, no GPU touch).
> - **(B) delayed transport-dead** — display re-enables later, first GPU touch then D-hangs (Fable r3, just
>   after 12 s). Favoured by the 3-min D-pileup (if it's real and not self-inflicted). → transport-restore.
> - **NEXT (round 5, clean time-series):** one automated run, NO manual poking, NO `vulkaninfo` probe (remove
>   it — it self-confounds); a single persistent SSH samples at t=5/15/30/60/120 s: CRTC `active=`, D-state
>   count + wchan, host GPU-traffic. Decides A vs B — CRTC stays off + D-count 0 = (A); CRTC flips `active=1`
>   or a virtio-gpu-fence `kworker` appears = (B).
>
> ### ✅ FLOOR SPIKE — round 5 (2026-07-18, clean time-series, Fable-run) — RESOLVED: (B) delayed transport-dead
> A non-perturbing time-series (`spikes/m9-freeze-trigger/m93-round5-timeseries.sh`; no `vulkaninfo` probe, one
> ssh per tick) + D-safe `/proc/*/stat` forensics settles the A/B question decisively as **(B)**, and proves
> the whole causal chain with kernel stacks.
> - **CRTC DID re-enable** — flipped `enable=1 active=1` between t=15 s and t=30 s (so hypothesis (A)
>   "display never re-enabled" is DEAD). But it was **fbcon/fbdev**, not a mutter commit.
> - **The parked commit worker is REAL and captured verbatim:** a `kworker` in
>   `virtio_gpu_queue_ctrl_sgs → virtio_gpu_cmd_resource_flush → virtio_gpu_primary_plane_update →
>   drm_atomic_helper_commit_tail → drm_fbdev_shmem_helper_fb_dirty` — a display flush waiting forever for free
>   slots in the GPU **control virtqueue that nothing drains**. It holds the DRM modeset lock (so the t=60 s
>   debugfs read itself wedged in `drm_modeset_lock → __drm_state_dump`).
> - **Three successive gnome-shell instances** (gdm respawn-loop) each D in
>   `virtio_gpu_vram_mmap → drm_gem_mmap` (a blob-map mmap waiting on a host response that never comes), with
>   sibling threads stuck in `exit_mm → do_exit` — they took fatal signals but **cannot finish dying** (the
>   mmap-stuck thread pins the mm) → **unkillable corpses**. `ps`/`systemd-journal` then go D in
>   `__access_remote_vm` reading those corpses' `/proc/*/cmdline` — the **D-contagion** that drove round-4's
>   3-min pileup (so that pileup was REAL, not just the `vulkaninfo`/poking confound). Load climbed to 42+.
> - **Guest journal:** the original compositor (pid 1282) died **SIGABRT + core dump at ≈25-30 s post-resume**
>   (timing consistent with the venus ring watchdog, `vn_common.c:281-289`); gdm respawn-loops from there.
> - **Host is the clincher:** all 4 vCPUs `resumed from snapshot at pc=`, `KEY_WAKEUP` injected, **zero**
>   post-restore GPU traffic (only the boot-time `virgl_flags = 0x35b`). MMIO ISR-read histogram: net 26 617 /
>   blk 12 125 / i2c 1 480 / vsock 1 379 (all re-negotiated + interrupt-alive on resume) — **virtio-gpu slot
>   `a008`: 0 interrupts, ever.** The device is dead. (Two log caveats recorded by Fable: the MMIO *write* arm
>   has no `debug!` so kicks are invisible — `vstate.rs:483-488`; and round-4's `0xa00d060` 1 Hz reads were
>   **vsock** ISR polls, not GPU — a red herring.)
> - **Root cause (confirmed):** the restored guest believes the GPU is `DRIVER_OK` with live queues (M9.2
>   quiesce exception, `lib.rs:423-424`), but the fresh worker's device was **never activated** — no
>   `DRIVER_OK` write ever arrives on restore — so commands queue into a ring nothing drains → vq fills →
>   `queue_ctrl_sgs`/`vram_mmap` waiters park in D → compositor aborts → gdm respawn-loop → each respawn wedges
>   unkillably at its first blob map → fbcon grabs the CRTC and its flush parks too (modeset lock hostage) →
>   D-contagion → load 42+. Round-4's clean t≈12 s was simply **before the ≈25-30 s cascade onset**; both
>   rounds are consistent.
> - **Two earlier recommendations now REFUTED by this data:** (1) "crash→respawn converges" is FALSE without
>   the transport fix — respawn doesn't just fail, it **accumulates unkillable D-state corpses and
>   self-poisons the whole guest** (the floor without a fix is *worse* than a black screen). (2) The agent
>   unbind/rebind fallback is **probably unworkable** — driver removal can't complete while unkillable
>   `vram_mmap` waiters hold GEM/device refs and the modeset lock is hostage. Demote it from "fallback" to
>   "likely dead against this failure mode."
> - **Stray finding:** phase-2 `--console` capture came back **empty (0 bytes)** on the restore path — would
>   have been the stall-PC oracle; worth a look someday.
>
> ### ✅ v1 BUILD CONFIRMED (round 5): virtio-gpu TRANSPORT-STATE restore — snapshot v4 spec
> With a live transport, the recovery chain becomes benign: the parked flush completes (error responses for
> vanished resources still consume descriptors and **signal their fences**), the corpses' mmaps complete and
> their `exit_mm` unwedges, the compositor's abort still fires once, and the **next** gdm respawn does a clean
> venus init against a responsive device + empty-but-functional rutabaga → working greeter (~10-30 s, session
> lost). Snapshot must carry (**VERSION 3→4**), for each device with `device_status != 0` at capture (today:
> exactly virtio-gpu):
> 1. **MMIO transport register file** — `device_status`, negotiated feature pages, per-queue `QueueNum`,
>    `ready`, desc/avail/used ring GPAs, `interrupt_status`. (The rings are guest RAM — already snapshotted.)
> 2. **Device-side queue progress** — the `Queue` next-avail/next-used counters, so the device resumes
>    consuming exactly where the restored rings expect.
> 3. **Restore-side ACTIVATION** — run the device activation (worker thread + queue-event wiring) that a live
>    boot triggers on the `DRIVER_OK` write, before the vCPUs release (`builder.rs:1234-1243`, beside GIC/GPIO).
> 4. **Snapshot-side quiesce refinement** — drain the GPU control queue/fences before capture (the planned
>    "drain fences") so the captured queue state is empty/consistent (vq must not be mid-command).
> 5. **RED-test keystone** — confirm each command path against a missing resource/context returns an error
>    response **and signals its fence** (not a dropped descriptor); the `ErrInvalidResourceId` paths in
>    `devices/src/virtio/gpu/virtio_gpu.rs` look right but the fence-on-error semantics need a RED test — the
>    entire recovery rests on error responses still completing.
> **Gate:** re-run this exact spike on the transport-restore build; decisive oracles = abort fires once
> (journal), **no** D-state `vram_mmap` corpses accumulate, gdm respawn reaches a rendered greeter (eyeball /
> window capture), load stays sane.
>
> ### FLOOR SPIKE — round 1 result (2026-07-18, `spikes/m9-freeze-trigger/m93-floor-windowed.sh`) [⚠️ CONTAMINATED — see correction above]
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

> **DECISION RECORD (2026-07-18) — transport-state restore attempted, then removed; the wedge is venus
> host-state loss.** An earlier round-5 diagnosis blamed the post-restore venus wedge on a *dead virtio-gpu
> transport* (the fresh worker built the GPU in INIT) and a host-side **snapshot-carried transport-state
> restore** was built (snapshot v4). RED-first validation overturned that premise: with the restore-side
> replay disabled the guest *self-revived* the GPU (fresh worker's Status register went DRIVER_OK on its
> own), and an R4 re-run of the windowed-venus timeseries **still wedged with the replay ON**. Cause, cited
> against the guest kernel (`7.1.2-limina16k`): `virtio_mmio_restore → virtio_device_restore` runs for every
> device on s2idle thaw; the GPU has no driver PM ops so it's the only device left at status `0xf` (the only
> one *captured*), but the bus still resets + re-negotiates it on thaw → the fresh worker activates from the
> guest's own writes. So the transport comes back regardless; the real wedge is **lost host-side venus
> context/resource state** (fresh virglrenderer has no vkr contexts / venus ring blob / VRAM-hostmem blob
> mappings → stale-resource submissions never signal their fence → gnome-shell D-hangs in
> `virtio_gpu_vram_mmap`). That is the venus retain-and-replay above — this doc's §4b, unchanged.
>
> **What landed instead (libkrun patches 0069/0070):** snapshot v4 format + capture of each DRIVER_OK
> device's transport (diagnostics + the substrate for a feature-drift fail-closed guard) + restore-side
> layout validation. The restore-side **negotiation replay** and the save-side **queue drain** were both
> removed: replay is redundant (guest self-revives), and the drain's `avail==used` oracle is wrong for
> RX-style queues (they sit at avail>used at idle) while its fence-consistency purpose is mooted by the
> thaw-reset. The negotiation-replay code stays reachable in git history (libkrun branch, pre-`ec09351`) if
> guest-uncooperative snapshots are ever needed.
>
> **Commitment:** the **s2idle bracket is the sole production snapshot path.** The raw SIGUSR1 path cannot be
> a product path without the removed replay (no thaw ⇒ nothing revives the transports for a guest that never
> knew it was snapshotted); it remains the **L1 test vehicle** for vCPU/GIC/RAM mechanics only.
>
> **Follow-up (worker-quiesce, hardening):** the drain accidentally provided one real thing — no device
> worker writes guest RAM during `dump_ram` (torn dump; loudest as net RX from gvproxy). Replace it with
> "stop the writers, not the rings": park the separate-thread writers (GPU renderer / blk) around the dump.
> Narrow on the production s2idle path (the guest froze net/blk to INIT; only the GPU worker is live).

### M9.4 — Full-snapshot feature + suspend/resume UX
Named snapshots (save / restore / **clone** / roll back / delete); VMGenID reseed on clone; one-click
Suspend; capability probe; ~~stock wall-clock one-shot step on resume~~ **VERIFIED ALREADY CLOSED
2026-07-20, no new code needed**: libkrun 0088 (PL031 anchored to host `CLOCK_REALTIME`; the restore's
fresh worker rebuilds it at current host time) flows through EDK2's `rtc-efi` runtime service — the
guest's hctosys device — and the stock kernel's own s2idle-thaw sleeptime injection does the step.
Measured on an F44 stock guest (own 6.19.10 kernel via EFI, chronyd stopped, no limina components):
guest−host delta **+0.058 s** after a 122 s suspend gap, same as the boot baseline (a broken path
would read −122 s). Guarded permanently in `managed_vm_suspends_and_resumes` (15 s deliberate gap +
±5 s wallclock assertion). Note the *other* stock clock gap — host sleeps while the guest keeps
running — remains enhanced-tier-only (agent TimeSync; see [[limina-guest-clock]]): nothing re-reads
the RTC on a guest that never suspended; **lz4 + sparse RAM writes** to make the "second or two" headline hold for a large guest;
docs. **Point-in-time disk capture** — a live snapshot-then-keep-running diverges the disk from the frozen
RAM/device state, so restoring later corrupts the fs; take an **APFS `clonefile()`** of each data disk at
the pause point (cheap, CoW) and bind it into the snapshot manifest (§8 already scopes disk-*set* identity;
this adds disk-*contents* identity). Until built, gate live snapshots to a clean pause.

**Snapshot write speed — SHIPPED 2026-07-20 (libkrun 0081, v6 format).** Baseline on the P2 stack:
8.8 GB in ~54 s (~165 MB/s) — v5 `save_snapshot` was fully serial (whole-RAM `Vec` copy at ~2×
guest RAM transient, single-threaded CRC32 over the whole file, serial write; ballooned pages
copied too). v6 keeps the head encoding under its own CRC and rebuilds the RAM section as 4 MiB
chunked frames: all-zero chunks become data-less holes, the rest lz4 blocks (raw if lz4 would
grow them), each with a per-frame CRC; a worker pool streams frames straight out of guest memory
(no whole-RAM copy either direction), and restore decompresses frames into guest memory in
parallel (holes zeroed — the fresh worker's boot payload/FDT must be overwritten). Measured,
8 GiB fresh-boot guest: **save 6.6 s / 465 MiB file** (1677 zero + 367 lz4 + 5 raw frames),
**restore RAM apply 2.3 s**; restored guest verified alive. A lived-in guest will land between
this and the old numbers (fewer holes, same parallel lz4 floor). Balloon note: a *blocking*
pre-suspend balloon expansion stays rejected — inflation needs live guest cooperation before
s2idle entry (user-visible latency; page-cache reclaim can take tens of seconds under writeback),
and zero-page holes already capture reclaimed pages at zero delay. A bounded **opportunistic
inflate** (low target, ~2–3 s deadline, proceed regardless) remains a possible later refinement,
enhanced tier first (FRQ free-page reporting makes it cheap).

**Suspend/resume speed vs Parallels — CLOSED 2026-07-20 (libkrun 0085 + a build-profile trap).**
Dogfood said "much slower than Parallels"; forensics on a lived-in 8 GiB guest (seated Firefox
session, 2.5 GiB snapshot, ~840 MB GPU section) found two compounding causes, neither in the v6
format itself:

1. **lz4_flex default features select the SAFE codec** — the checked iterator implementation.
   The save spent ~80% of its time there (`sample` on the worker), the serial GPU-section
   compress alone running ~100 MB/s. Fix = `default-features = false` (0085); frames stay
   CRC-verified at apply, so torn/corrupt data is still caught.
2. **`build-app.sh` defaulted to DEBUG**, and the 2026-07-20 dogfood deploy was a bare
   invocation — debug assertions (ub-checks hot in the sample) stacked on the safe codec.
   The default is now **release**; `cargo xtask app/bundle` always passed the profile
   explicitly and was never affected.

Measured on the same lived-in guest, felt end-to-end (trigger → torn down / start → first
presented frame, via the new `first frame presented` window log):

| build | suspend felt | resume felt | read / apply |
|---|---|---|---|
| debug + safe lz4 (deployed state) | 27.2 s | 14.0 s | 6.4 / 5.7 s |
| debug + fast lz4 | 16.2 s | 10.4 s | 4.8 / 3.9 s |
| **release + fast lz4 (shipped)** | **3.4 s** | **3.7 s** | 1.0 / 1.4 s |

Save breakdown at the shipped config: guest s2idle quiesce ~1 s + GPU fence-drain/capture 0.2 s
+ streamed write 2.0 s. Remaining known headroom (not currently worth the churn): the GPU
section still compresses serially in `encode_head` (~0.5 s), the restore still `fs::read`s the
whole file before applying (mmap would overlap IO with decompress), and F_NOCACHE would stop a
multi-GB apply from flushing the host page cache (the suspected cause of the dogfood-mac-wide UI
stutter during dogfood-guest's resume — see the center-hang exoneration note in
[[limina-m9-suspend-resume]]).

**Second-generation resume crash — ROOT-CAUSED + FIXED same day (libkrun 0086).** First resume
of a suspended session was green; suspending the *resumed* session and resuming again crashed
2/2 — cascading vkr replay `entry failed (stale reference?)` + res-import failures
(`fd_type=-999`), ending in the KK assert `kk_descriptor_set.c:74 (sampled_gpu_resource_id)` →
SIGABRT. Mechanism: **vkr_seq fence epoch mixing.** The rutabaga journal adopted at gen-1
restore (`restore_entries`) keeps its old-epoch CreateBlob fences (~1.5M), while the fresh venus
context's wire journal re-records the replayed commands from seq 1 (`vkr_journal_create` →
`seq_next = 1`). Generation 2's rutabaga/wire merge is then cross-epoch garbage: the first old
fence drains the *entire* new wire journal, so every `VkImportMemoryResourceInfoMESA` import
replays before its exporter blob exists — the -999 cascade; the dropped binds leave an image
view with no memory and a replayed descriptor write trips KK's assert. Fix in the replay driver
(the only place that knows the epoch mapping): right after feeding a CreateBlob's fence, rewrite
`entry.vkr_seq` to the new journal's watermark (`journal_vkr_seq`) — the adopted journal is
single-epoch and generation N+1 is correct by induction. Verified on the 2/2 lived-in repro:
**three** suspend/resume generations green (same boot_id, Firefox alive, 3.7 s felt resume
each). `venus_session_preserved` grew a generation-2 leg as a survival guard — note it does NOT
reproduce the pre-fix crash (the L2 seated session's imports never reference forward blobs); the
lived-in repro script is the sharp instrument. Auto-resume kept the pre-fix failure safe
(consumed snapshot → cold boot; disk unharmed) — only the session was lost.

**Suspend/resume UX — SHIPPED 2026-07-20** (commits bff4cc3, 206d43f, 0dedba4, f881807): the four
bullets below are implemented and eyeball-verified. Deltas from the sketches: the splash rides a
close INTERCEPT (`windowShouldClose` returns NO; the window stays up dimmed with a CA-drawn
spinner + "Suspending…" for the save, then closes itself — user-preferred over hide-and-reopen),
and the overlay is pure Core Animation because the scanout view is layer-hosting (AppKit controls
never composite in it). Suspend persistence now also covers the WINDOWED path (the session monitor
persists `[suspended]`; window-state savers merge via `state::set_window` instead of clobbering),
and the snapshot is SINGLE-USE (renamed `.consumed` at restore-consume; the double-restore
disk-brick class is closed). The VM menu ships without **Restart** — that needs an agent-side
Reboot verb (proto addition; batch with the next guest-tools delivery). Remaining polish: bigger
arc/caption (user request), and the named-snapshot manager / clone / VMGenID half of M9.4 below.
A sibling feature was designed 2026-07-20 (not built): **host sleep → in-place guest s2idle** with a
session-preserving thaw for stock guests (defer-and-classify the GPU session reset) —
`docs/design/host-sleep-s2idle.md`.

**Play-button park/resume — SHIPPED 2026-08-06 (task #18, commit 8c00768).** A menu/CLI
suspend no longer exits the process: the window *parks* — the final frame stays presented
(the IOSurface outlives the dead worker) under the dim scrim with a centered play glyph, and
the title gains "— Suspended". A click anywhere in the content (or the VM menu's Suspend
item, which `validateMenuItem:` retitles to **Resume** while parked) respawns the worker in
the same NSWindow via the reboot-relaunch machinery; `take_pending_resume` makes the spawn a
one-shot restore by construction. Close-to-suspend still closes — parking is only for
suspends where the user kept the window (`should_park_on_suspend`, pure + unit-tested). The
session's monitor thread parks in `recv()` on a resume channel; the window's `PARK_STATE`
(Live/Parked/Resuming) gates the exit path, the overlay machine, the input monitor (parked:
left-click = play, everything else passes to AppKit), the capture tap (full pass-through —
a parked tap swallowing Cmd-W was caught live), and the menu verbs (Shut Down/Force Stop go
dead while parked; they return during Resuming as the hung-resume escape hatch). The parked
quit path deliberately skips the shutdown ladder *and* the process-group kill: a
long-parked worker's pid may have been recycled.

**Auto-resume, one-shot by construction — SHIPPED 2026-07-20 (dogfood incident fix).** The first
dogfood deploy destroyed a guest's btrfs ("parent transid verify failed" → emergency mode): an
in-guest **reboot of a restored session** relaunched the worker with the original argv — including
`--restore` — re-applying the stale pre-resume RAM over the advanced disk. Fix is structural, not
an argv patch: `limina` no longer has a `--restore` flag at all. The armed `--snapshot-file` path
IS the resume-pending record — `supervisor::take_pending_resume` runs inside `spawn_worker` at
EVERY spawn (first boot and reboot relaunch, headless and windowed): snapshot present ⇒ consume it
(rename `.consumed`, clear the `[suspended]` record — now pure UI status) and pass the worker its
internal `--restore` for that one spawn; absent ⇒ cold boot. Both harm directions are closed by
construction: a pending suspend can't be skipped (detection is not optional) and a restore can't
be forged or repeated (nothing to point a flag at; the file leaves its canonical name at consume).
Guards: L2 `managed_vm_suspends_and_resumes` grew a reboot-after-restore leg (in-guest `reboot`
must yield a NEW boot_id + writable fs; RED reproduced the disk-destroyer in 30s), plus
`supervisor::tests` unit tests for consume/one-shot/stale-record reconcile. The harness's
`restore_from` now just arms `--snapshot-file` at an existing snapshot.

**Suspend/resume UX (filed 2026-07-20, from dogfood feedback).**
- **Last-scanout splash:** at suspend, save the final presented frame (the present path already
  holds the IOSurface; the window-capture oracle proves the grab) into the VM bundle next to the
  snapshot; at restore, the window shows it immediately (letterboxed by the existing fit-rect)
  until the first real post-restore present — no more long blank window.
- **Dim/blur + progress animation:** AppKit overlay in the window process (blur the held frame,
  progress indicator), driven by lifecycle events the supervisor already observes (bracket fired /
  snapshot written / restore started / first present).
- **Window-close behavior:** `vm.toml` setting `on_window_close = suspend | shutdown | ask`,
  **default suspend** (rides on this milestone's persisted `Suspended` status + auto-`--restore`).
- **VM window menu:** every VM verb in the menu bar + Dock menu — Suspend, Shut Down (power
  button), Force Stop, Restart, Settings, Show in Finder, Copy SSH command.

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
