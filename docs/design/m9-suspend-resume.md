# M9 — Suspend & resume (hibernate) — design proposal

> **STATUS: PROPOSAL — not yet started.** This folds six suspend/resume research reports into one
> decision-ready plan. Every non-obvious claim carries a `path:line` citation into
> `third_party/libkrun/` or `crates/`, or a URL; re-verify before building (point-in-time anchors).
> **VERIFIED** = read in source this round; **ASSUMED** = inferred, needs a spike (M9.0).

**Goal in one line:** the user closes the lid on their Linux desktop; limina puts it away (no host
RAM held, the worker process gone); hours or days later they reopen it and the desktop is exactly
where they left it — same apps, same windows, correct clock, working accelerated GPU.

---

## 1. Goal & motivation

The user-visible promise is **Parallels "Suspend"**: a menu item / lid-close that freezes the running
guest to disk in a second or two, **tears the worker process down** (reclaiming all host RAM, the
GPU/Metal/IOSurface graph, gvproxy), and a later "Resume" that brings the *same* desktop back — open
apps, scrolled positions, mounted shares — with a correct wall clock and a working accelerated display.

Why it matters for limina specifically:

- It is a headline Parallels feature we currently lack. `docs/research/GAPS-and-verification.md` lists
  "VM lifecycle: snapshot / save-restore / suspend-resume / pause" as an *uncovered* gap and defers the
  feasibility spike.
- The hard constraint is the **two-tier guarantee** (CLAUDE.md): a stock Fedora guest on
  upstream-shaped libkrun must still get *something*, and our enhanced kernel/agent must only *improve*
  it, never gate it.
- We are unusually well positioned because **"reboot" is already implemented as "kill the worker, keep
  the supervisor + host resources, relaunch a fresh worker"** (`crates/limina/src/supervisor.rs`:
  `WORKER_EXIT_REBOOT = 125`, the `should_relaunch` policy, and `run`'s relaunch loop). Suspend/resume
  is the *same shape* with one delta: the relaunched worker restores state (or the guest restores
  itself) instead of cold-booting.

The single dominant technical fact that shapes everything below: **accelerated-GPU host state is the
canonical blocker.** It cannot be serialized, and every VMM in the field works around it the same way —
by making the *guest* release the GPU rather than trying to snapshot it.

---

## 2. Two candidate approaches

### A. VMM-level snapshot (Firecracker / crosVM / Parallels-style)

**Mechanism.** Pause all vCPUs, quiesce the virtio worker threads, read out vCPU + device-model state,
dump guest RAM to a file, kill the worker. Restore = relaunch the worker with `--restore <file>`,
reload RAM + device state + vCPU registers, resume the vCPUs. This is exactly Firecracker's
`Pause → CreateSnapshot → (kill) → LoadSnapshot → Resume` lifecycle
([snapshot-support.md](https://github.com/firecracker-microvm/firecracker/blob/main/docs/snapshotting/snapshot-support.md))
and crosVM's "freeze vCPUs, then devices, then serialize"
([crosvm snapshotting](https://crosvm.dev/book/architecture/snapshotting.html)).

**What it captures.** Guest RAM, vCPU registers, and per-device VMM-resident state (virtqueue indices,
feature bits, config space). With a two-file split (small vmstate + big sparse mem file) and lazy
`MAP_PRIVATE` COW restore, resume is near-instant regardless of RAM size.

**What it does NOT capture** — and this is fatal as a *pure* approach on our stack:

- **Host GPU/venus/KK/Metal/IOSurface state is non-serializable.** rutabaga snapshots **2D only** —
  `RutabagaComponent::snapshot` returns `Unsupported` unless there are zero contexts
  (`third_party/libkrun/src/rutabaga_gfx/src/rutabaga_core.rs`). QEMU installs a **migration blocker**
  the moment virgl/blob is live ("virgl is not yet migratable"; blob VMs NULL-crash in
  `virtio_gpu_save`). Android Cuttlefish (production crosVM) supports snapshot **only with the software
  renderer**.
- **In-flight venus fences** parked on host GPU completions: a naive dump captures descriptors waiting
  on a Metal command buffer that no longer exists after restore → the guest **hangs on a flush
  forever** (`crates/limina-vmm/src/krun/mod.rs`).
- Live host-resource bindings (gvproxy unixgram fd, virtiofs/block fds, vsock connections) —
  reconnectable with graceful degradation.

**Pros.** Transparent to the guest → the **only** approach that needs zero guest cooperation, which is
exactly the stock-tier baseline. Owns the vCPU, so it can set `CNTVOFF_EL2` via
`hv_vcpu_set_vtimer_offset` to keep `CLOCK_MONOTONIC` continuous on restore — the one place the VMM
layer earns its keep.

**Cons.** HVF gives **no dirty-page log** → stop-the-world full dump only, no live/iterative pre-copy
(`docs/research/02-macos-hvf.md`). Large net-new libkrun surface: there is **no real pause path**
(`VcpuEvent::Pause`/`Resume` exist but are inert; `resume_vcpus` is a no-op), **no bulk register
get/set** (accessors private, SIMD entirely unbound at the libkrun layer), and the in-kernel **GIC
state APIs are unused**. And it does **not** solve the GPU on its own — to be correct it must ask the
guest to release the GPU first, at which point it has stopped being a *pure* VMM snapshot.

### B. Guest-side hibernation (Linux S4 suspend-to-disk; S3 considered and rejected)

**Mechanism.** The supervisor sends a `Hibernate` command over the existing vsock control plane;
`limina-agent` runs `systemctl hibernate`. The guest kernel's swsusp snapshots its own RAM to its
**own swap on the virtio-blk disk**, the drivers run their `.freeze` callbacks (GPU released by the
DRM driver), and the guest finishes with PSCI `SYSTEM_OFF`. Resume = the supervisor cold-boots a fresh
worker with `resume=<guest swap>` on the cmdline; the guest kernel's early resume path restores the
image and every driver re-handshakes via `virtio_device_restore()`.

**What it captures.** Everything, because it's the guest doing it — the host writes **nothing extra**
beyond a tiny "hibernated" flag so the supervisor knows the next launch is a resume.

**Pros.** Cleanest on the two hard subsystems:

- **GPU:** the host renderer is **never serialized** — there is none to serialize. On resume a
  **brand-new worker process cold-boots venus exactly as on every normal boot** (a well-exercised
  path), and the guest virtio-gpu DRM driver re-establishes the GPU *exactly as on a real-hardware
  power cycle* via its hibernate/resume PM callbacks. **No host process needs to survive** — and that
  is the point: the resume state lives entirely in the guest's own swap on the persistent virtio-blk
  disk image, so hibernate survives the **entire app (supervisor + worker) being quit, and even a host
  reboot or power loss** (the defining property of suspend-*to-disk*). The out-of-tree drm/virtio
  freeze/restore series (Dongwon Kim, v5 2025-10) re-submits every tracked `virtio_gpu_object` on
  resume for *exactly* our "the VMM process was terminated and a fresh one started" scenario
  ([dri-devel v5](https://www.mail-archive.com/dri-devel@lists.freedesktop.org/msg567003.html)).
  (Note: this is **not** the libkrun-0022 / `venus_reset` property — that's an *in-process* renderer
  singleton surviving the EFI→kernel device reset *within one worker's lifetime*, which is irrelevant
  here because hibernate kills the whole process tree. What matters is the ordinary cold-boot venus
  init, which already works on every boot.)
- **Network:** ideal — link bounce, NM re-DHCP against gvproxy's static `.2` lease; the gvproxy recycle
  is the existing reboot path.
- Reuses the **single-shot worker + relaunch** substrate verbatim; no host snapshot file, no GPU
  serialization, no new register/GIC plumbing.

**Cons.** Requires guest cooperation → **not a stock-tier floor**. Stock Fedora aarch64 has
`CONFIG_HIBERNATION=y` compiled in but **does not hibernate out of the box**: it ships **zram swap
only** (volatile, unusable for swsusp), no `resume=`, and lockdown-under-Secure-Boot can block it
([Fedora Magazine](https://fedoramagazine.org/update-on-hibernation-in-fedora-workstation/)). The
resume kernel must **match the hibernated kernel's page size / VA bits** — a 16 KiB enhanced kernel and
a 4 KiB stock kernel cannot resume each other's images ([LWN 675989](https://lwn.net/Articles/675989/)).
S4 writes multi-GB RAM to guest swap → slower than a host RAM dump. virtio guest hibernation has
historically been weak — **but we own the guest kernel**, so the enhanced tier can carry the
freeze/thaw patch.

**On S3 (suspend-to-RAM).** Wrong tool for this goal and not possible today: libkrun returns
`NOT_SUPPORTED` for PSCI `SYSTEM_SUSPEND` (`third_party/libkrun/src/hvf/src/lib.rs`), and S3 **keeps the
worker and all host RAM alive** blocked on a wakeup IRQ — contradicting "free the machine." It's a
future *fast-pause* feature, not hibernate.

---

## 3. The hard parts, and how each approach handles them

| Hard part | VMM-snapshot (A) | Guest-side S4 (B) |
|---|---|---|
| **Accelerated GPU host state** (the canonical blocker) | **Cannot serialize.** Must force the guest to release the GPU first → degenerates into a hybrid. | **Neutralized by construction** — host renderer never serialized; a fresh worker cold-boots venus (the every-boot path) and the guest DRM driver re-inits via its resume PM callbacks. No surviving host process required. |
| **In-flight venus fences** | Parked descriptors deadlock on restore unless drained to fence-quiescent first. | Drained by the driver's own `.freeze` callback for free. |
| **virtio device state** | Serializable POD (`Queue`/`MmioTransport`/features/config) but all net-new code — none serialized today. | Sidestepped — devices re-enumerate; guest re-negotiates features via `virtio_device_restore()`. |
| **In-kernel GIC state** | Blob API exists (`hv_gic_state_get_data`/`set_state`) but **unused & unproven on HVF** — restore ordering uncharted (ASSUMED). | N/A — GIC re-inits on cold boot. |
| **`CLOCK_MONOTONIC` jump** | **Best-equipped:** owns the vCPU, sets `CNTVOFF_EL2` via `hv_vcpu_set_vtimer_offset` to stay continuous, dodging Firecracker's "resumed on another day" jump. Must make the policy choice explicitly. | Kernel saves/restores its own timekeeping across S4. |
| **`CLOCK_REALTIME` (wall clock)** | **No `/dev/rtc` on macOS** → must push from host. libkrun's port-123 timesync *already fires on long-sleep detection* (`timesync.rs`, the "long nap" path) but **no stock guest consumer exists** — the enhanced agent must consume it. | Same dependency — guest can't re-read an RTC, so even S4 needs our host wall-time push. |
| **Networking / gvproxy** | Restores TCP control blocks intact → half-open sockets until timeout; gvproxy relaunched (already supported). | Clean: link bounce, NM re-DHCP, apps see honest disconnects. In-flight TCP loss is **acceptable** either way (lid-close semantics). |
| **16 KiB-host / 4 KiB-guest pages** | RAM dump is host-page agnostic (just `hv_vm_map`'d host memory); restore re-maps the same IPAs. **Skip the 8 GiB GPU SHM window** and optionally balloon-inflated pages. | Resume kernel must **match the page size it hibernated on** — fine, since we cold-boot the *same* enhanced kernel. |
| **Uniqueness / entropy (CSPRNG clone hazard)** | Real for Firecracker's 1000-clones model; **largely evaporates** for limina's one-user-one-VM suspend-resume-once case. Ship a VMGenID-style reseed notification cheaply for the rare "duplicate VM" feature; Linux ≥5.18 reseeds its kernel PRNG. | Same hazard only if a hibernation image is *cloned*; not for normal resume. Agent reseed on resume is the enhanced nicety. |

**The decisive convergence:** two independent production VMMs (QEMU blocks, Cuttlefish software-only)
plus all three of our GPU-focused research angles reach the same verdict — **do not serialize the GPU;
make the guest release it and re-init on resume.** The re-init it relies on is just the ordinary
cold-boot venus init that already runs on **every** boot — no special "renderer survives" property is
needed (and none could help, since hibernate kills the whole process tree). That cold-boot path being
proven on every launch is what makes the guest-side approach low-risk on the GPU front.

---

## 4. Recommendation

**Adopt a HYBRID, structured as two tiers of the same lifecycle: guest-side S4 as the enhanced-tier
path, and a guest-assisted supervisor RAM/vCPU snapshot as the stock floor.**

1. **Enhanced tier → guest-side S4 (primary).** The agent drives `systemctl hibernate`; the guest
   writes its own image to a pre-provisioned swap, releases the GPU through its DRM driver, and powers
   off. Resume cold-boots the same 16 KiB enhanced kernel with `resume=`. This is the cleanest design
   on the two walls (GPU + fences) and reuses the reboot=relaunch spine almost verbatim.

2. **Stock tier → guest-assisted-quiesce + VMM RAM/vCPU snapshot (the floor that must always work).**
   Because a stock guest can't be *relied on* to hibernate (no swap, no `resume=`), the stock path is a
   VMM-level snapshot — but **never a pure one**. The supervisor first asks the guest to quiesce the
   GPU to a fence-clean state (best-effort: agent if present; otherwise a DPMS-off / device-suspend
   nudge, accepting a dropped GPU context on the truly bare stock guest), *then* pauses vCPUs and dumps
   RAM + vCPU + virtio device-model state, **skipping the 8 GiB GPU SHM window**. On restore it sets
   `CNTVOFF_EL2` to keep monotonic time continuous (the one thing only the VMM layer can do), relaunches
   the worker, reloads state, and lets the guest re-init the GPU.

**Why this split and not "just one":**

- A **pure VMM snapshot is impossible** on this stack (GPU + parked fences), and the moment it becomes
  correct it's already a hybrid. So we don't pretend otherwise.
- A **pure guest-side S4 violates the two-tier floor** — it makes hibernate an entry fee (swap +
  `resume=` + matching kernel) that a stock guest hasn't paid. The guarantee forbids that.
- The hybrid honors **"mechanism in libkrun, policy in limina"**: libkrun gains pause + register/GIC
  save-restore + `CNTVOFF` control + a `--restore` boot mode (mechanism); limina decides *which* path a
  given guest takes from additively-detected capabilities (policy).
- It maximizes **reuse**: both tiers ride the existing relaunch loop, gvproxy recycle, control-plane
  reconnection, and port-123 timesync.

**Honest caveat — where the research diverged.** One research angle argued guest-side S4 is *the*
mechanism and the stock tier should get only a documented-manual degraded path (don't build a VMM
snapshot at all). The others argued the two-tier guarantee needs a *transparent* VMM snapshot for
stock. We side with the latter **for the floor** because "stock gets a manual recipe only" is a weak
floor — but we adopt S4 wholesale for the enhanced tier because it's strictly cleaner where we control
the kernel. **The risk:** the VMM-snapshot floor is the larger build (pause + registers + GIC + device
serialization). **The primary scoping decision for the user:** if the M9.0 spike shows the stock VMM
snapshot is too costly for v1, ship **enhanced-tier S4 first** and let the stock tier get a coarse
"suspend = clean shutdown with best-effort session save" until the snapshot floor lands.

---

## 5. Two-tier mapping

**Stock guest (unmodified Fedora, upstream-shaped libkrun) — must never break, gets the floor:**

- **Suspend/resume via VMM-level snapshot** (transparent, no guest cooperation). RAM + vCPU + virtio
  device state to a file; worker killed; restore relaunches and resumes.
- **GPU on resume: a context reset / display blip.** The bare stock guest may lose GPU contexts
  (degraded but acceptable per the two-tier guarantee); apps re-initialize. Where stock Mesa handles
  `VK_ERROR_DEVICE_LOST` / DRM reset gracefully, it recovers; where it doesn't, the user sees a redraw.
- **Monotonic clock stays continuous** (host sets `CNTVOFF_EL2` — pure host-side, works on stock).
- **Wall clock** corrected by NTP slew after resume (chrony may be slow to / refuse a huge step — the
  honest degraded story) since there's no agent to consume port-123.
- **Never broken:** if quiesce/snapshot can't proceed, suspend falls back to a clean power-off — the
  guest still boots next time.

**Enhanced tier (our 16 KiB kernel + limina-agent + our libkrun) — unlocks the real thing:**

- **Guest-side S4** driven by the agent: clean device + GPU teardown via the DRM `.freeze` path, image
  in guest swap, no host snapshot file.
- **Pre-provisioned swap ≥ max RAM + baked `resume=`/`resume_offset=` + dracut `resume` module** in the
  enhanced image; **PM kernel configs** added to `scripts/build-test-kernel.sh`'s fragment (today it
  has *none* — verified): `CONFIG_PM=y`, `CONFIG_PM_SLEEP=y`, `CONFIG_HIBERNATION=y`,
  `CONFIG_HIBERNATION_SNAPSHOT_DEV=y`, a compressor.
- **Carry the drm/virtio freeze/restore patch** in `patches/linux/` until upstream, so venus/virtio-gpu
  objects survive the worker recycle.
- **Clean wall-clock on resume:** the agent consumes libkrun's already-firing port-123 long-sleep
  timesync and `clock_settime`s `CLOCK_REALTIME`.
- **Userspace reseed + VMGenID-style notification** (the part the kernel PRNG reseed can't reach) for
  the rare duplicate-VM case.

**Additive, not a tier switch (CLAUDE.md doctrine):** detect *each* prerequisite independently — "swap
configured?" AND "agent present?" AND "freeze/thaw kernel?" — and only offer one-click S4 when all
hold; otherwise fall back per-feature to the VMM-snapshot floor. The enhanced pieces are delivered
*into* a basic guest by the normal RPM/agent path; none are a precondition to boot.

---

## 6. Reuse of existing machinery

The spine already exists — this is the strongest reason the milestone is tractable:

- **Reboot = relaunch-the-worker.** `SYSTEM_RESET` → `WORKER_EXIT_REBOOT = 125` → supervisor relaunches
  a fresh worker while it, gvproxy, and the control plane survive (`crates/limina/src/supervisor.rs`;
  `crates/limina/src/main.rs` relaunch loop). Suspend adds a **third worker exit disposition**
  ("suspended", e.g. code 126) alongside power-off (0) / reboot (125).
- **Windowed fd-swap into a live `WorkerConn`** (`crates/limina/src/main.rs`, the
  `spawn_windowed_worker` → `WorkerConn` swap) already re-points the supervisor's display/input/surface
  plumbing at a brand-new worker **without tearing down the NSWindow** — the exact precedent for
  restoring plumbing across a resume.
- **Surface-port receiver outlives workers** by design (`crates/limina/src/window.rs`); on resume the
  new worker republishes the IOSurface over the existing port, same as runtime display-resize already
  exercises.
- **Control plane is already reconnect-tolerant.** The accept loop runs for the supervisor's life;
  agents re-`HELLO` on reboot; clipboard initial-offer replay handles a late joiner. Resume needs
  **essentially no new control-plane work** — a worker gap already reads as a reconnect.
- **gvproxy recycle** at the same socket path is the existing reboot path (`crates/limina/src/gateway.rs`);
  pin `--ssh-port` for a stable inbound forward across resume.
- **Port-123 timesync already fires on long-sleep** (`third_party/libkrun/src/devices/src/virtio/vsock/timesync.rs`,
  `TSYNC_PORT = 123`, sends immediately when it detects a nap far longer than the update interval — the
  host-suspend signature). We get resume wall-clock correction nearly free *once a guest consumer
  exists*.
- **`limina-proto` extension:** add `Hibernate`/`Suspend` request + ACK and a `TimeSet` message; the
  `Message` enum today covers Hello/Welcome/Heartbeat/MemPressure/Shutdown/ShutdownAck/Clip*
  (`crates/limina-proto/src/lib.rs`, `enum Message`). **M6 lesson:** a new variant breaks both guest
  binaries' matches — update them together.

---

## 7. Milestone breakdown — M9

> Slots **after M8** (audio/x86/polish): hibernate is the remaining headline lifecycle gap. Each step
> is bisectable: edit → `scripts/apply-libkrun-patches.sh` → build → codesign → `scripts/test-boot.sh`.
> RED-first: each "Done test" starts as a failing `crates/limina-test` test driving the shipped binaries.

### M9.0 — Founding spikes (gate the whole milestone)

**Goal:** prove the three gating unknowns before building (see §8). **No production code.**
**Spike #1 is DONE (2026-06-28, `spikes/s4-hibernate/RESULTS.md`)** — and it changed M9.1 below:
guest-side S4 in libkrun is **blocked by two libkrun HVF gaps**, so M9.1 is no longer "no core libkrun
patches." The guest-side setup (swap, `resume=`, image discovery) is correctly wired and the guest
reaches `hibernation_snapshot`, but: (a) **PSCI `CPU_OFF`/`AFFINITY_INFO` are unimplemented** →
`disable_nonboot_cpus()` aborts multi-vCPU hibernate; (b) an **unhandled EL1 debug sysreg (`OSDLR_EL1`,
…) on the CPU-suspend path** halts the VM even single-vCPU. Both are ours to patch. (Spikes #2/#3 still
pending.)

### M9.1 — Enhanced-tier guest-side S4 (the clean path first)

**Goal:** an enhanced guest hibernates on agent command and resumes to the same desktop.
**Key tasks (dependency order — libkrun HVF gaps FIRST, per spike #1):**
1. **libkrun: implement PSCI CPU hotplug-offline** — `CPU_OFF` (0x84000002) + `AFFINITY_INFO`
   (0x84000004 / 0xc4000004) so the kernel can offline non-boot vCPUs for the snapshot. (RED test: a
   `--cpus 2+` guest currently aborts hibernate at `disable_nonboot_cpus()`.)
2. **libkrun: handle the EL1 debug/suspend sysreg set** the CPU-suspend path touches — `OSDLR_EL1` (the
   one that crashed the spike), and almost certainly `OSLAR_EL1` / `MDSCR_EL1` / the `DBGB*`/`DBGW*`
   breakpoint regs — read+write, modeled or safely stubbed, instead of "stop the VM."
3. Image build: pre-provision swap ≥ RAM (file with a known `resume_offset`; **SELinux-label it
   `swapfile_t`** — a raw btrfs subvol is `unlabeled_t` and logind is denied), add
   `resume=`/`resume_offset=` + dracut `resume` module (`scripts/build-image.sh`, `docs/images.md`).
4. Kernel: add the PM/HIBERNATION configs to the 16k kernel build (`scripts/build-kernel-rpm.sh` /
   `build-test-kernel.sh` fragment — none today).
5. Carry `patches/linux/00NN-drm-virtio-freeze-restore.patch` (Dongwon Kim v5) for GPU object re-submit.
6. `limina-proto`: `Hibernate` request + ACK (update both guest binaries' matches).
7. Agent: handle `Hibernate` → `systemctl hibernate`; ACK "entering hibernate" before `SYSTEM_OFF`.
8. Supervisor: a `hibernate-pending` flag set on ACK so it disambiguates *hibernate-off* from *ordinary
   power-off* (both exit 0 — the disambiguation must come from control-plane state). Record
   `state = hibernated`; reap the worker; persist the tiny flag.
9. Resume action: deliberately invoke the relaunch path with `resume=` wired — **not** the automatic
   reboot trigger (resume is a user action).
10. Agent: consume the port-123 long-nap sync → `clock_settime(CLOCK_REALTIME)` on resume.

**libkrun patches (REQUIRED, was "none" — corrected by spike #1):** PSCI `CPU_OFF`/`AFFINITY_INFO`
(task 1); the EL1 debug-sysreg handlers (task 2); and likely **virtio-mmio freeze/restore hardening**
(the spike saw `update virtio queue in invalid state 0x8f` + guest WARNs at `virtio_config.h:276` and a
virtio-net that didn't recover across in-place thaw). Optional: a **distinct exit code for "powered off
after hibernation"** if the agent-ACK disambiguation proves fragile (mechanism in libkrun, policy in
limina).
**Done test (RED→GREEN):** an enhanced guest, given `Hibernate` over vsock, writes its image and exits
0; a subsequent worker launch with `resume=` brings the guest back with a marker file/process from
before still present, venus re-enumerates, and the clock is correct within seconds. Pixel-verify the
desktop via `iosdump.swift` (human oracle for the window per the `limina-window-control` note).
**Risks/spike first:** does the 16 KiB enhanced kernel reliably resume across a libkrun cold-boot in
the deterministic memory layout? Does venus come back usable with vs. without the drm/virtio patch?
(M9.0.)

### M9.2 — Stock-tier VMM-snapshot floor, part 1: pause + RAM + vCPU

**Goal:** pause a running guest, dump RAM + vCPU registers to a file, kill the worker, relaunch with
`--restore`, resume — for a **no-GPU / software-2D** stock guest first (isolate the GPU variable).
**Key tasks:**
1. libkrun: real HVF **pause/quiesce** — kick all vCPUs via `hv_vcpus_exit` (exists), park at a barrier,
   wire the dead `VcpuEvent::Pause`/`Resume` into the run loop (`resume_vcpus` is a no-op today); drain
   virtio worker threads so virtqueue indices are consistent.
2. libkrun: `HvfVcpu::save_state`/`restore_state` enumerating X0–X30, PC, CPSR/PSTATE, SP_EL0/EL1,
   SPSR_EL1, ELR_EL1, the EL1 sysreg set, Q0–Q31 + FPCR/FPSR, pending interrupt, vtimer mask, **and the
   vtimer offset**.
3. libkrun/limina: memory dump iterating `guest_memory` regions writing host bytes (RAM is VMM-owned
   `hv_vm_map`'d host memory — no HVF call needed to read), **skip the 8 GiB GPU SHM window**; restore
   re-mmaps + `hv_vm_map`s the same IPAs. Two-file split (small vmstate + sparse mem file) + CRC the
   vmstate. Lazy `MAP_PRIVATE` COW restore if cheap.
4. Worker: a third exit disposition "suspended" (code 126) + a `--restore <file>` boot mode that reloads
   instead of cold-booting.
5. Restore sets `CNTVOFF_EL2` via `hv_vcpu_set_vtimer_offset` to keep `CLOCK_MONOTONIC` continuous;
   **make the Firecracker-style policy choice explicit** (continuous vs. advanced monotonic — default
   continuous for a desktop).

**libkrun patches:** pause/quiesce; `save_state`/`restore_state`; memory dump/reload + GPU-window skip;
`CNTVOFF` set on restore; `--restore` boot mode.
**Done test (RED→GREEN):** a software-2D stock guest with a counter running in a tight loop is
suspended; the worker exits 126 and a snapshot file appears; relaunch `--restore` resumes and the
counter continues from (not before) its pre-suspend value, monotonic time hasn't leapt, and the desktop
redraws.
**Risks/spike first:** HVF full vCPU round-trip (do all EL1 sysregs accept `set_sys_reg` post-run?);
re-mmap + `hv_vm_map` at original IPAs behaves like first boot (ASSUMED). (M9.0.)

### M9.3 — Stock floor, part 2: virtio device state + GIC + GPU quiesce

**Goal:** extend the snapshot to a full virtio device set and a GPU-enabled stock guest via
guest-assisted quiesce.
**Key tasks:**
1. libkrun: a **versioned device-state schema** (CBOR like crosVM, magic + semver header, fail-closed on
   version mismatch — adopt Firecracker's "no cross-version migration" pragmatism; QEMU-subsection-style
   optionality because the two tiers have *different device sets present*). Serialize each device's
   `Queue`/`MmioTransport`/features/config.
2. libkrun: in-kernel **GIC snapshot** via `hv_gic_state_create`/`get_size`/`get_data` + `hv_gic_set_state`
   (unused today) — **highest-risk item, spike-gated**. Fallback: a userspace `GicV3` path (plain
   fields, trivially serializable) if the in-kernel blob won't round-trip.
3. Guest-assisted GPU quiesce: supervisor asks the guest (agent, or a coarse DPMS-off / device-suspend
   nudge for bare stock) to release the GPU and drain to **fence-quiescent** before the dump; invalidate
   the in-flight scanout on restore; let the guest produce a fresh first frame.
4. Reopen host resources on restore: block/fs fds re-seek/reopen; net unixgram to a fresh gvproxy
   (recycle); vsock re-handshake (control plane already does this).

**libkrun patches:** device (de)serialization; GIC state; GPU-quiesce trigger.
**Done test:** a GPU-enabled stock guest is suspended and resumed; virtio-block survives, the desktop
re-inits the GPU and redraws, no parked-fence hang, gvproxy recycled, agent (if present) re-HELLOs.
**Risks/spike first:** `hv_gic_set_state` cleanly restores into a fresh GIC (ASSUMED).

### M9.4 — UX, polish, two-tier glue

**Goal:** one-click Suspend in the window; capability detection chooses S4 (enhanced) vs. snapshot
(stock); resume restores the NSWindow without a flash.
**Key tasks:** menu/keybinding; additive capability probe (swap? agent? freeze/thaw kernel?);
VMGenID-style reseed notification (cheap, for duplicate-VM); stock wall-clock NTP-step backstop; docs.
**Done test:** human-verified suspend→resume on both a stock and an enhanced image; clock correct after
a real multi-hour suspend; window survives.

### Summary — net-new vs. libkrun

| Step | Net-new limina | libkrun patches |
|---|---|---|
| M9.1 S4 enhanced | image swap+`resume=`+SELinux-label, proto `Hibernate`, agent hibernate + time-consumer, supervisor hibernate-state + resume trigger | **PSCI `CPU_OFF`/`AFFINITY_INFO`; EL1 debug-sysreg handlers (OSDLR_EL1…); virtio-mmio freeze/restore hardening** (spike #1); carry `patches/linux` drm/virtio freeze/restore; optional distinct hibernate exit code |
| M9.2 snapshot RAM+vCPU | snapshot file format/CRC, `--restore` wiring, monotonic policy | pause/quiesce; `save_state`/`restore_state`; mem dump + GPU-window skip; `CNTVOFF` set; `--restore` mode |
| M9.3 devices+GIC+GPU | versioned device schema, GPU-quiesce orchestration, host-resource reopen | device (de)serialize; GIC state blob; GPU-quiesce trigger |
| M9.4 UX | menu, capability probe, VMGenID, NTP backstop | (none) |

---

## 8. Spikes to de-risk first (M9.0 — cheapest, do before building)

1. ✅ **DONE (2026-06-28) — `spikes/s4-hibernate/RESULTS.md`.** "Does stock arm64 Fedora S4-hibernate
   inside libkrun?" → the guest-side path is correctly wired (swap, `resume=`, image discovery) and the
   guest reaches `hibernation_snapshot`, but hibernation is **blocked by two libkrun HVF gaps**: PSCI
   `CPU_OFF`/`AFFINITY_INFO` unimplemented (aborts multi-vCPU at `disable_nonboot_cpus()`) and an
   unhandled EL1 debug sysreg `OSDLR_EL1` on the CPU-suspend path (halts the VM single-vCPU). Plus a
   SELinux swap-labeling gotcha and rough in-place virtio freeze/restore (network didn't recover). The
   **resume handshake is therefore not yet provable** — gated behind those libkrun fixes (folded into
   M9.1 tasks 1–2). Vehicle (stock-kernel-first, to isolate env from our-kernel-configs): a clone of
   `Fedora-Workstation-43.accessible.raw` booted headless via `limina --net`; `spikes/s4-hibernate/`
   holds the repro + evidence.

2. **Can HVF round-trip full vCPU state?** A bare-metal arm64 vehicle (reuse `spikes/hvf-trap-probe`,
   the `limina-hvf-graceful` note): run a few instructions, `get` all GP/SIMD/EL1-sysreg/PSTATE/
   vtimer-offset, `set` them back into a fresh vCPU, continue, and confirm identical results — and that
   **no EL1 sysreg rejects `set_sys_reg` post-run** with `HV_BAD_ARGUMENT` (the chief ASSUMED). Same
   vehicle: confirm `hv_gic_state_get_data` → `hv_gic_set_state` round-trips into a freshly created
   controller. **Gate for M9.2/M9.3.**

3. **Does the GUEST cleanly re-init venus when resuming from its own hibernate image, against a
   freshly-booted worker?** This is the real open question — *not* "does the host renderer survive"
   (it doesn't, and needn't: the worker is dead and a new one cold-boots venus fresh, which already
   works on every boot). The spike: drive a seated venus desktop, `systemctl hibernate` the enhanced
   guest so the whole worker process exits, then cold-boot a fresh worker on the same disk and
   pixel-verify the desktop comes back via `iosdump.swift`, with **no parked-fence hang** and the
   guest's virtio-gpu driver re-attached. Test with vs. without the drm/virtio freeze/restore patch to
   size whether we *must* carry it. **Gate for M9.1 GPU correctness and M9.3 quiesce.**

---

## 9. Open questions & risks

**Verified (read in source / cited this round):**
- HVF exposes the full register/SIMD/sysreg/GIC/vtimer get-set surface; libkrun leaves SIMD +
  GIC-state unbound and keeps register access private; no pause/snapshot exists.
- Guest RAM is VMM-owned anonymous `hv_vm_map`'d host memory; the GPU SHM window is 8 GiB and
  dynamically re-pointed at renderer memory → must be skipped.
- rutabaga snapshots 2D only; QEMU blocks virgl/blob migration; Cuttlefish snapshot is
  software-renderer-only.
- HVF has no dirty-page log → stop-the-world only (`docs/research/02-macos-hvf.md`).
- `CONFIG_HIBERNATION=y` in Fedora aarch64 but no usable swap/`resume=` out of the box; arm64 hibernate
  needs matching kernel page size.
- libkrun returns `NOT_SUPPORTED` for PSCI `SYSTEM_SUSPEND` → S3 impossible today.
- Reboot=relaunch, windowed fd-swap, surface-port persistence, gvproxy recycle, reconnect-tolerant
  control plane, and the long-sleep-aware port-123 timesync all already exist.
- No `/dev/rtc` on macOS → wall clock must be host-pushed.

**Assumed / needs a spike (do not build on without M9.0):**
- `hv_gic_set_state` cleanly restores into a fresh GIC; all EL1 sysregs accept `set_sys_reg` post-run;
  re-mmap + `hv_vm_map` at original IPAs behaves like first boot; exact `set_vtimer_offset` resume
  semantics.
- The drm/virtio freeze/restore series is **not confirmed merged** (v5, Oct 2025) — likely carried as an
  out-of-tree guest patch.
- Stock guest's resume clock: chrony may refuse a large step after a multi-day suspend → the stock
  wall-clock story may be worse than "slews quickly."
- Whether the **stock VMM-snapshot floor is worth its build cost for v1**, or whether to ship
  enhanced-S4 first and give stock a coarse "clean shutdown + best-effort session save" interim (the
  primary scoping decision flagged in §4).

**Where the research diverged (surfaced, not papered over):**
- **Mechanism for the floor.** One angle: guest-side S4 is the mechanism; stock gets a *documented
  manual* path; don't build a VMM snapshot. The others: the two-tier guarantee needs a *transparent*
  VMM snapshot for stock. We chose the hybrid (S4 enhanced + VMM-snapshot floor) but acknowledge the
  snapshot floor is the heavier lift and may be deferred.
- **GPU in a VMM snapshot.** All GPU-touching angles agree it can't be serialized; they differ only on
  framing ("restrict to 2D / no-GPU or guest-assisted" vs. "any correct VMM snapshot is already a
  hybrid"). Resolved the same way: guest-assisted quiesce, never serialize.
- **Uniqueness/entropy.** One angle treats it as central (Firecracker's threat model); others note it
  "largely evaporates" for one-user-one-VM resume. We ship VMGenID cheaply but do not let it drive the
  design.

---

## Primary file touch-points for the implementer

- `crates/limina/src/supervisor.rs` — third exit disposition ("suspended") alongside `WORKER_EXIT_REBOOT`.
- `crates/limina/src/main.rs` — windowed fd-swap / resume relaunch (`spawn_windowed_worker`, `WorkerConn`).
- `crates/limina-vmm/src/krun/mod.rs` — boot / `build_microvm`, where snapshot save/restore hooks in.
- `crates/limina-proto/src/lib.rs` — new `Hibernate`/`TimeSet` messages (update **both** guest binaries).
- `scripts/build-test-kernel.sh` — the PM/HIBERNATION config fragment (none today).
- `third_party/libkrun/src/hvf/src/lib.rs` + `bindings.rs` — pause, save/restore, GIC, `CNTVOFF`.
- `third_party/libkrun/src/devices/src/virtio/vsock/timesync.rs` — the port-123 consumer in the agent.

*This proposal was synthesized from a six-agent research pass (libkrun/HVF snapshot feasibility,
Firecracker, crosVM/QEMU device state, guest-side arm64 hibernation, the limina state inventory, and
GPU/time/network reconnection) on 2026-06-28.*
