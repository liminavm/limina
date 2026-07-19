# Spike F — is suspend-to-idle reachable + wakeable inside libkrun?

**Gate before M9.2** (see `docs/design/m9-freeze-trigger.md` §5). Two coupled unknowns:
(1) can a libkrun guest enter `freeze` (s2idle) and be woken, without tripping an HVF gap the way
S4 did; (2) does the virtio-gpu resubmit hook the sleep callbacks. This spike attacks (1) — the
wakeup-source half — because that is what blocks everything else.

Vehicle: `spikes/m9-freeze-trigger/f44-s2idle.raw` = CoW clone of
`Fedora-Workstation-44.accessible.raw` (stock kernel `6.19.10-300.fc44`, has `CONFIG_SUSPEND` +
`rtc-pl031`). Booted headless EFI + `--net`, probed read-only over SSH. Date: 2026-07-18.

## Findings

### ✅ s2idle IS available in the guest
- `/sys/power/state` → `freeze mem disk`; `/sys/power/mem_sleep` → `[s2idle]`. So the `freeze`
  state exists and `mem` maps to s2idle. (Entry/exit not yet exercised — blocked on a wakeup, below.)

### ✅ libkrun's PL031 had no alarm — FIXED (libkrun 0054, this session)
- Firecracker's PL031 stored the Match Register but never fired, and on macOS `register_mmio_rtc`
  dropped the interrupt eventfd + ignored the `IrqChip`. So an armed alarm never delivered → no
  `rtcwake` wakeup. Fixed: a timer thread + event-manager `Subscriber` that raises the SPI on match;
  `register_mmio_rtc` now wires intc + irq line + subscriber; macOS RTC FDT node is edge-triggered
  (the in-kernel GIC's `set_irq` only asserts a one-shot pulse). Unit-tested (`test_rtc_alarm_fires`),
  boot-neutral.

### ❌ …but the PL031 alarm is UNREACHABLE on the EFI boot path — the real blocker
On the EFI/GRUB boot path (how **all** real images boot, stock and enhanced):
- The guest's **rtc0 is `rtc-efi`** (EFI runtime-services RTC), which has **no `wakealarm`**:
  `rtcwake -m freeze -s 5` → `rtcwake: set rtc wake alarm failed: Invalid argument`, so it bails
  *before* freezing (no hang, but no suspend either).
- The **PL031 never binds**: its DT node `rtc@a002000` is present but **`status = "disabled"`**, so
  `of_platform` skips it (no amba device, no `rtc1`, no IRQ in `/proc/interrupts`). The UART
  (`a001000`) and GPIO (`a003000`) primecell siblings bind fine.
- The PL031 MMIO itself is healthy (read directly via `/dev/mem`: PID `31 10 14 00`, CID
  `0d f0 05 b1` = the `0xB105F00D` primecell signature, valid `RTCDR`). So the device is there and
  correct — it's just **disabled in the DT that reaches the OS**.
- libkrun's `create_rtc_node` emits **no** `status` property (→ "okay" on the direct-kernel `--kernel`
  path), so the `disabled` is applied on the **EFI path by the EDK2/krun-efi firmware** (it shadows
  the DT RTC with its own EFI runtime RTC, an ArmVirtPkg-style pattern). We own that firmware
  (`scripts/build-krun-efi.sh`).

### ✅ FIX + VALIDATION — krun-efi patched, s2idle round-trip wakes (2026-07-18)
The blocker was purely the firmware. **Root cause: `ArmVirtPkg/Library/ArmVirtPL031FdtClientLib`'s
constructor explicitly sets the pl031 DT node `status="disabled"`** ("UEFI takes ownership of the RTC
hardware... disable it in the device tree to prevent the OS from attaching its device driver as
well"). **Fix (`scripts/build-krun-efi.sh` patch step 1c): flip that `"disabled"` → `"okay"`** so the
guest's `rtc-pl031` binds (UEFI keeps using the same PL031 for GetTime — concurrent reads don't
conflict). Rebuilt `KRUN_EFI.gop.fd`; re-probed a fresh clone:

- `rtc1 -> rtc-pl031 a002000.rtc; wakealarm: YES`; `/proc/device-tree/rtc@a002000/status` = `okay`;
  `a002000.rtc` is now an amba device; `/proc/interrupts` shows `rtc-pl031` on GIC SPI 33 (Edge).
- **`rtcwake -m freeze -s 10 -d rtc1` round-trip:**
  - **ENTRY works** — guest freezes, virtio drivers quiesce (`update virtio queue in invalid state
    0x8f`), NO S4-style PSCI/OSDLR crash.
  - **WAKE works** — the libkrun 0054 alarm arms with correct timing (`match=…273 now=…262 -> 11.1s`),
    fires on schedule 11 s later (`fire_alarm imsc=1 intc=true irq_line=Some(33)`), and the **vCPU
    resumes from s2idle**: proven by the guest immediately executing the `RTCIMSC=0` MMIO store (only a
    running vCPU can), i.e. its rtc IRQ handler ran.
  - ✅ **Full resume/thaw now works** (libkrun 0055/0056/0057 — see below). The `0x8f` on the initial
    attempt was the reset-less-device bug (the design's M9.2 virtio freeze/thaw item), NOT a wake
    failure; fixing `reset()` on net/balloon/vsock gives a clean, repeatable SSH+net round-trip.

## Conclusion / where this leaves M9

**Spike F wakeup-half: ANSWERED — s2idle is reachable in libkrun and the PL031 alarm wakes it.** The
mechanism is now complete on the real (EFI) boot path: libkrun 0054 (RTC alarm) + the krun-efi patch
(PL031 enabled) → `rtcwake -m freeze` enters s2idle and the vCPU wakes on schedule.

### ✅ Virtio freeze/thaw hardening — DONE (2026-07-18, libkrun 0055/0056/0057)
On resume the guest re-inits each virtio device, starting with a reset (status write 0). The MMIO
transport calls `VirtioDevice::reset()`; the trait default returns `false`, which marks the device
FAILED (`device_status 0x8f`) and drops all queue re-init → the device never comes back. Three of our
devices lacked a `reset()` override:
- **virtio-net** (libkrun 0055): now implements `reset()` — stops the worker via a new stop-eventfd,
  goes Inactive, returns true — **and preserves the gateway connection** across the reset (the backend,
  the gvproxy unixgram socket, is opened once and handed back by the worker's `JoinHandle` on stop,
  then reused on re-activate). Reconnecting instead dropped gvproxy (it exits when its vfkit peer
  disconnects).
- **balloon + vsock** (libkrun 0056): implement `reset()` → drop to Inactive, return true. Unlike
  net/block they have no dedicated worker; they run under the shared EventManager with queue eventfds
  that stay registered across a transport reset (the transport reuses `queue_evts`), so re-activate
  just works. Also named the device in the two mmio diagnostics (the `invalid state 0x…` warning and
  the `does not support reset` path).
- 0057 adds gated `debug!` traces (per-device status-transition ladder + net reset/activate) that made
  the resume path observable.

**VALIDATED — full, repeatable s2idle suspend/resume round-trip (F44 stock, EFI + `--net`):**
`rtcwake -m freeze -s N -d rtc1` now suspends and cleanly resumes. Guest `dmesg`:
`PM: suspend entry (s2idle)` → `PM: resume devices took 0.005 seconds` → `PM: suspend exit`. The worker
log shows **every** device walk the full re-init ladder on resume
(`0x0 -> 0x1 -> 0x3 -> 0xb -> 0xf`, i.e. INIT→…→DRIVER_OK) — net, vsock, balloon, block, console, rng,
snd, i2c. Post-resume the guest is fully alive: **SSH round-trips, `eth0` is UP, outbound `curl`
returns 200.** Verified across **two consecutive freeze/wake cycles** on the same guest (dmesg shows
2× suspend entry / 2× suspend exit), both recovering SSH + net.

> ⚠️ Observation discipline note: the resume is **delayed** — the wake fires ~N s after freeze-entry and
> the guest completes `dpm_resume` a moment later. Checking the worker log too soon shows only the
> freeze-entry `0xf -> 0x0` reset ladder and *looks* like "the guest never resumes / net is dead."
> It does resume; wait past the armed wake before judging.

### ⚠️ CORRECTION (2026-07-18) — the "DONE" above was validated through a RACE that masked a guest crash
The clean round-trips above were **real but not reliable**: whether they worked depended on a race, and
the same recipe soon failed *consistently* (guest wedged, never recovers). An adversarial review + a
follow-up investigation root-caused it, and the earlier "invariance is a smell" discipline applied
squarely — the failing/​passing didn't correlate with any host-side change:

- **Two more host-side bugs the review found (fixed, libkrun 0058):** (1) `Vsock::reset` made the device
  re-activatable, but `muxer.activate` unconditionally spawned a fresh timesync/muxer/reaper trio with
  **no teardown of the old ones** → every suspend cycle leaked 3 threads that kept writing used-ring
  entries into the **freed/reallocated** guest RX ring (memory corruption). Fixed: stop the muxer
  threads on reset. (2) balloon/vsock/snd/i2c didn't drain their eventfd in the not-activated branch →
  a level-triggered spurious event (e.g. the host balloon policy kicking `target_evt` while suspended)
  **busy-spun the worker at 100 % CPU** for the whole suspended interval. Fixed: drain-before-warn.
  (+ net stop-eventfd drain, balloon reset-state clear, transport feature-clear.)
- **The actual wedge = an upstream `virtio_balloon` kernel bug, triggered by OUR `F_REPORTING`
  feature.** `virtballoon_freeze()` frees the balloon virtqueues (`remove_common`) **without** stopping
  the free-page-reporting worker, which runs on the non-freezable system `events` workqueue (unlike
  `virtballoon_remove`, which `page_reporting_unregister`s first). ~1.5 s into s2idle the worker
  use-after-frees the dead reporting vq → oops (`page_reporting_process → virtballoon_free_page_report
  → virtqueue_add_inbuf`, identical PC across runs, recovered from a **RAM-dump of a wedged guest**);
  with default `console_suspend=Y` the mid-suspend oops then deadlocks `dpm_resume` and the guest parks
  forever. Decisive control: `modprobe -r virtio_balloon` → clean repeated round-trips. limina hit it
  because M6 dynamic-memory advertised `VIRTIO_BALLOON_F_REPORTING`; upstream QEMU users rarely pair it
  with s2idle. (The "earlier successes were real" — the race is whether page-reporting work lands in the
  freeze window; a settled GNOME session makes the worker hot → consistent failure.)
- **Fixes:** (a) host-side — **mask `VIRTIO_BALLOON_F_REPORTING` by default** (libkrun 0059 +
  `VmResources.balloon_free_page_reporting`, worker `--balloon-free-page-reporting`, default off); stock
  keeps coarser inflate-time `MADV_FREE_REUSABLE` reclaim and s2idles safely (two-tier: degraded but
  working). (b) real fix — **kernel patch `patches/linux/0005`** (`virtballoon_freeze` unregisters
  page-reporting, `virtballoon_restore` re-registers) so the enhanced tier can re-enable FRQ reclaim and
  still suspend; needs a kernel rebuild + enhanced-image respin to deploy, then flip the flag on for
  enhanced VMs. Submit upstream.

**RE-VALIDATED with the mask (2026-07-18):** `page_reporting` absent in the guest; **5/5 consecutive
`rtcwake -m freeze` cycles clean** on stock F44 — every cycle recovers SSH + outbound `curl` 200, and
the worker **thread count stays flat at 11** (was leaking +3/cycle before 0058). This is the reliable
result; the un-caveated "DONE" above stands only with `F_REPORTING` masked (default) or the kernel patch
deployed.

**Enhanced tier also s2idles (verified 2026-07-18).** An earlier note here claimed the 16k enhanced
kernel "has no PM configs" — that was WRONG (it conflated the `scripts/build-test-kernel.sh`
`--kernel`-injection *test* kernel with the shipped enhanced tier). The enhanced kernel is built by
`scripts/provision/f44/build-kernel-rpm.sh` **from the guest's real Fedora config** + only a 16k
delta, so it inherits Fedora's full PM stack. Grepped the shipped `/boot/config-7.1.2-limina16k`:
`CONFIG_SUSPEND=y`, `CONFIG_PM_SLEEP=y`, `CONFIG_HIBERNATION=y`, `CONFIG_RTC_DRV_PL031=y`,
`CONFIG_ARM64_16K_PAGES=y`; `/sys/power/state`=`freeze mem disk`, `mem_sleep`=`[s2idle]`. And the
**enhanced 16k kernel does the same clean `rtcwake -m freeze` round-trip** as stock F44 (WOKE, eth0
UP, outbound 200, `PM: suspend entry (s2idle)` → `resume devices took 0.019s` → `suspend exit`). So
there is **no enhanced-tier kernel blocker** for s2idle.

Spike F **part 2** (does the virtio-gpu resubmit hook the sleep callbacks) still needs the
Dongwon-Kim series we don't carry — that's the remaining GPU-state question, independent of the
kernel PM config.

## Repro
```
cp -c Fedora-Workstation-44.accessible.raw spikes/m9-freeze-trigger/f44-s2idle.raw
target/debug/limina --firmware target/krun-efi/KRUN_EFI.gop.fd \
    --disk spikes/m9-freeze-trigger/f44-s2idle.raw --cpus 2 --ram-mib 4096 --net
ssh -p <PORT> claude@127.0.0.1   # PORT from the "SSH forward ready" log line
  cat /sys/power/state /sys/power/mem_sleep
  cat /sys/class/rtc/rtc0/name              # rtc-efi (no wakealarm)
  cat /proc/device-tree/rtc@a002000/status  # disabled
  sudo rtcwake -m freeze -s 5 -v            # "set rtc wake alarm failed: Invalid argument"
```

---

## ✅ B3 — stock, NO-agent suspend trigger via a GPIO KEY_SLEEP button (2026-07-18, PASS; libkrun 0060)

The freeze trigger the M9 host-side snapshot needs (`docs/design/m9-freeze-trigger.md`) assumed the
*enhanced* tier (agent-coordinated). B3 proves the **stock floor** can suspend cooperatively with **no
limina-agent and no custom kernel** — a host-driven GPIO button is enough.

**Mechanism (libkrun 0060):** libkrun's PL061 GPIO already carries a poweroff/restart `gpio-keys`
button on line 3. Added a **second `gpio-keys` button on line 4 emitting `KEY_SLEEP` (142)**, driven by
a new *suspend eventfd* that mirrors the existing shutdown eventfd. On the worker, a `SIGUSR2` handler
pulses that eventfd (`crates/limina-vmm/src/suspend.rs`); the GPIO subscriber raises the line; the
guest's `gpio-keys` driver emits `KEY_SLEEP`; **stock `systemd-logind` maps `KEY_SLEEP` →
`HandleSuspendKey` (default: suspend)** → `systemctl suspend` → `/sys/power/state=mem` → s2idle. No
agent in the loop.

**Result (stock `Fedora-Workstation-44.accessible.raw`, EFI + `--net`, headless SSH):**
```
[press SIGUSR2 -> worker]  # == GPIO KEY_SLEEP button
[    7.121120] PM: suspend entry (s2idle)     # logind suspended the guest, no agent
   ... SSH unreachable ~27s (guest frozen) ...
[   38.004689] PM: resume devices took 0.012 seconds   # woke via the pre-armed PL031 alarm
[   38.020356] PM: suspend exit
[SSH recovers]                                 # virtio thaw hardening (0058) intact
```
Verified premises (via `b3-diag.sh`): the guest `gpio-keys` input dev advertises **both** keys
(`KEY=1000000 0 0 0 4000 0 0` → bit 142 KEY_SLEEP + bit 408 KEY_RESTART), and udev tags it
`power-switch` so logind watches it (`CURRENT_TAGS=:power-switch:`).

**Wake is separate & already solved:** the button only *enters* s2idle (logind arms no wake). Here the
test pre-armed `rtcwake -m no -s 30 -d rtc1` (arms the PL031 alarm without suspending) so the guest
self-wakes; in production the host injects the wake. PL031 alarm + EFI `status=okay` fix are Spike F.

**Bug found & fixed mid-spike (macOS EventFd read/write-fd trap):** the first run did NOT suspend — the
worker's gpio debug log never logged "Generate a suspend key press event". Root cause: on macOS
`EventFd` is a *pipe*; `as_raw_fd()` is the **read** end (which the GPIO subscriber epolls), so the
`SIGUSR2` handler must write the **write** end via `get_write_fd()` — same trap `snapshot.rs` already
documents. `suspend.rs` fixed to publish `get_write_fd()`. (Note: `shutdown.rs` still stores the read
fd — the same latent bug — which is one reason the stock GPIO *poweroff* button has been unreliable;
left for a follow-up.)

**Scope note:** `SIGUSR2` is the spike/mechanism seam; M9.2 wires the supervisor to pulse the suspend
button around the host-side snapshot (suspend bracket). The button device always exists (build_microvm
synthesizes the eventfd if the caller passes None), so it's harmless on the C-API/stock path.

### B3 repro
```
bash spikes/m9-freeze-trigger/b3-suspend-button.sh          # PASS verdict
bash spikes/m9-freeze-trigger/b3-diag.sh                    # premise diagnostics (input/udev/logind)
```

---

## ✅ B4 — does a >=1h in-place s2idle preserve the guest wall clock? (2026-07-18, PASS: skew 0s)

The 2026-07-17 review predicted a stock guest's `CLOCK_REALTIME` would **freeze during s2idle and drift
by the sleep duration** unless we injected a resume-time clock step. B4 tests it directly: stock F44
clone, headless, NTP disabled, `rtcwake -m freeze -s 3720 -d rtc1` (~62 min in-place s2idle), compare
host vs guest epoch before/after.

**Result:** clock **PRESERVED** — `post_wake_skew=0s` (host_delta=3728s, guest_delta=3727s; the 1s is
the initial −1s baseline). Guest dmesg: `PM: suspend entry (s2idle)` at +8.4s → `PM: resume devices
took 0.022s` at +3729s → `suspend exit`. `timedatectl` after wake == host wall time.

**Why (and the M9.2 consequence):** the guest runs its own resume path, and `timekeeping_resume()`
re-reads the PL031 RTC on s2idle exit — so `CLOCK_REALTIME` self-corrects with **no bespoke port-123
consumer and no explicit clock step**. This validates the freeze-trigger design's "the clock rides the
bracket for free" assumption (`docs/design/m9-freeze-trigger.md` §3) and **falsifies the review's drift
prediction** for the enter/exit mechanics.

**Scope caveat (the teardown case is still open):** B4 is *in-place* s2idle — the worker stays alive,
so HVF's CNTVCT keeps advancing through the freeze. The real M9 suspend **tears the worker down** and
restores into a **fresh** worker after an arbitrary gap (possibly a host reboot). There, correctness
reduces to a single question already scoped elsewhere: **does the fresh worker's PL031 report the
current wall time?** If yes, the same `timekeeping_resume()` re-read self-corrects the teardown gap for
free; if the PL031 is `Instant`-anchored (see [[limina-guest-clock]]), that's the scoped PL031
wallclock fix, not new M9 work. B4 proves the guest *mechanism* is correct; the host RTC source is the
only remaining variable.

### B4 repro
```
bash spikes/m9-freeze-trigger/b4-clock.sh   # (job-tmp copy; ~75 min: 62 min freeze + wake + compare)
```

## ⭐ M9.3 rounds 5–9 — ROOT CAUSE: dead GPU queues after the no-PM-ops bus-fallback resume (2026-07-19)

The five instrumentation probes (libkrun 0071 `gpu/trace.rs` + virglrenderer 0033 vkr dump; vehicle
`m93-floor-windowed.sh`) were built to count the presumed stale-ctx storm hitting the fresh renderer.
They found the opposite, and the chain ended at a **register-level root cause one layer below venus**:

- **Round 6 (counted):** post-restore the guest sends the GPU device **nothing** — 902 ticks of
  `submits=+0 unknown_ctx=+0 fences_req=+0 outstanding=0`. No stale-ctx storm; pure silence.
- **Round 7 (guest stacks):** gnome-shell AND a fresh `vulkaninfo` canary D-state in
  `virtio_gpu_vram_mmap` (uninterruptible; guest `timeout` can't kill it). `PM: resume devices took
  0.003s` — dpm_resume completed; **virtio_blk logged its re-probe, the GPU logged nothing**.
  OS otherwise healthy (same boot_id, vsock RSTs); degrades progressively (~30 min → new SSH hangs).
- **Round 8 (MMIOTRACE, the direct observation):** on s2idle thaw every device re-programs its queue
  geometry (input 42 writes, balloon/snd 28, vsock 21, net 14, blk/i2c/rng 7) **except the gpu:
  `Status 0x0 → 0x1 → 0x3 → features → 0xb → 0xf` and ZERO queue writes.** DRIVER_OK driven onto a
  device whose queue registers were never programmed → dead control queue → every GPU command waits
  forever → `vram_mmap` D-hang → CRTC never re-lights → D-contagion (mmap_lock → logind/sshd).
- **Kernel citation (v7.1.2 sources, fetched):** `virtgpu_drv.c` has **no PM ops** (no freeze/restore,
  even at 7.1.2); `virtio.c virtio_device_restore_priv()` **always resets the device**, re-negotiates
  features, calls `drv->restore` *if present*, else just `virtio_device_ready()` — queues never
  re-programmed. So **upstream virtio-gpu s2idle is architecturally broken** against any
  spec-faithful device (reset must clear queue state). The snapshot angle was incidental — an
  in-place guest s2idle should wedge the GPU identically.
- **Retro-explanations:** the removed transport-restore replay was *necessary but not sufficient* —
  and also *mis-timed*: the guest's thaw reset wipes replay-time registers (the old
  R4-with-replay-ON "CRTC enable=1 then D-hang" was requested software state, not command flow).
  R2's fenced-dd GREEN was blind to dead queues (`fb_deferred_io_fsync` waits for the flush *work*,
  not device consumption; one small write can't fill the vring), so both R2 arms were false-GREEN
  on queue liveness — GPU_STATUS=0xf proved *status* re-negotiation only.

**FIX (round 9): device-side sticky queue re-arm** (mechanism in libkrun `mmio.rs`, in the spirit of
the existing QueueNum=0 leniency): track `queues_programmed` per negotiation cycle; if a driver
reaches DRIVER_OK having programmed **no** queue and a previous activation's register file exists
(`activated_queue_regs` — survives reset), re-arm the queues from it, ring cursors from restored
RAM's `used.idx` (completed stays completed; the backlog re-processes). The restore path seeds the
stash from the snapshot's captured transport state (`validate_transport_states`). Covers both the
snapshot restore AND in-place s2idle; inert on normal boots (any queue write sets the flag) and for
every PM-ops driver. The upstreamable deep fix — guest kernel virtio-gpu freeze/restore PM ops — is
an enhanced-tier follow-up that makes the leniency a no-op.

**Instrumentation gotchas (recorded):** a bare `RUST_LOG` directive list silences all other targets
(GPUTRACE = `krun_devices`, vkr dump = `krun_rutabaga_gfx` at info) — use
`warn,limina_vmm=info,krun_vmm=info,krun_rutabaga_gfx=info`. Host-side timebox every guest probe
(`ssht`); a D-state guest process shrugs off guest-side `timeout`.

**Retain-and-replay bill of materials (probe 4, healthy seated GNOME):** 1 vkr context
("gnome-shell") = 3 rings, 1 sync_queue, 18 resources, **1137 objects**: dset=770 buffer=93 image=62
image_view=43 memory=29 cmd_buf=26 pipeline=19 cmd_pool=17 shader=16 dpool=14 pipe_layout=9
semaphore=9 dset_layout=8 pipe_cache=8 sampler=7 buffer_view=2 (queue/fence/device/instance/
phys_dev ×1). Idle desktop ≈ GPU-silent; bursts ~11 submits / 5 fences; req==ret, outstanding=0.

## ⚠️ M9.3 round 10 (eyeball) — 0072 VALIDATED, but "seamless" was overclaimed: the SESSION RESTARTS (2026-07-19)

Second consecutive clean restore (windowed, human-verified): SSH back ~12 s, same boot_id, CRTC
live, zero GPUTRACE error counts at steady state, and the user eyeballed the kept window: **live
and responsive, no black screen, no garbled frames**. The transport fix (libkrun 0072 sticky queue
re-arm) is fully validated — the virtio-gpu wedge and its D-contagion are gone.

**But round 9's "gnome-shell re-creates its venus context / session preserved" was WRONG.** The
window came back on **GDM**, not the user session: ~17 s after restore the pre-suspend gnome-shell
**SIGABRTs** (core dumped, pid 1270), every Wayland client dies ("Lost connection to Wayland
compositor" — the injected gnome-calculator continuity marker died with it), and GNOME starts a
fresh session. What looked like recovery was a **restart**.

**Abort origin (root-caused from the core, both ends corroborated):** guest gdb was not installed;
`eu-stack` saw only the signal-handler re-raise, but `dnf install gdb` + `gdb -batch bt` through
the signal frame gave the full chain:

```
abort()
vn_relax                      ← mesa venus ring-wait loop hit its dead-ring abort threshold (~17 s)
vn_ring_submit_locked
vn_ring_submit_command
vn_image_init → vn_image_create → vn_CreateImage        (zink resource_object_create)
st_TexSubImage ← cogl glyph-cache upload ← clutter_text (the clock repainting text!)
```

The host restores with an **empty GPU world** — worker log at the abort moment:
`rutabaga state: 0 contexts, 0 resources`, `vkr state: renderer not initialized`; then the dying
shell's teardown bounces off it (3× `CtxDestroy → ErrRutabaga(InvalidContextId)`,
`unknown_ctx=+3 unknown_res=+93` — the stale-id burst), and the *replacement* session builds a
fresh world (`submits=+154, fences 67/67 req==ret`). So: the guest's mesa still believes in its
pre-suspend venus context and rings; the host no longer services them; the first real submission
after thaw (a glyph texture for the clock) spins in `vn_relax` until mesa's dead-ring abort.

**Where this leaves seamless (the actual M9.3 goal):** the transport layer is done; the remaining
gap is **host-side venus state loss across restore**. The options, in the order worth trying:
1. **Retain-and-replay** (the plan of record): recreate the vkr context/rings/objects at restore
   from the bill of materials above — the guest then never notices.
2. A guest-side reset/notify path (e.g. virtio-gpu PM ops + mesa VK_ERROR_DEVICE_LOST plumbing) —
   upstream-friendly but turns every venus app into a device-lost survivor; compositors mostly
   are not.

Forensics recipe that worked (keep): `coredumpctl dump PID -o core` + scp out; `eu-stack --core`
for thread census (no gdb needed); `gdb -batch -ex "bt 40"` unwinds THROUGH the signal frame where
eu-stack stops; correlate guest-local time with the worker log's UTC (+3 h from -03).
