# 11 — Audio, x86 Emulation, and Misc Devices

Scope: the remaining Parallels-parity devices that are not covered by the GPU/display/input/net/balloon docs.
This covers **audio** (virtio-snd via the generic vhost-user path, and how to bridge to macOS CoreAudio — the deepest section because a desktop VM needs sound), **x86_64 emulation** in the arm64 guest (Rosetta vs. qemu-user/box64), **virtiofs** host-folder sharing (`krun_add_virtiofs2/3`, DAX/shm window), and the small devices: **RNG**, **RTC**, and **virtio-console multiport** for the guest agent. For each: what exists in our local libkrun/deps today, the options, and a recommendation. All libkrun/dep claims are cited `path:line` against the locally cloned trees.

> Verification caveat: during this pass the transcript harness intermittently elided `.rs` file/bash outputs, so a few internal line numbers below are cited at function/region granularity. Public C header lines, `Cargo.toml` features, and the `#[cfg(...)]` gates quoted from `builder.rs` were verified exactly. Items marked **(verify)** should be re-checked by reading the cited `.rs` directly.
>
> **MAJOR CORRECTION (verified):** the entire vhost-user device path in libkrun is gated `#[cfg(all(feature = "vhost-user", target_os = "linux"))]` — see `src/vmm/src/builder.rs:976` (attach loop), `:991` (the `not(...)` arm), `:1378-1381` and `:1635-1638` (file-backed memory only on Linux), `:2454-2455` (`attach_vhost_user_device`), and the imports at `:54-55,:98`. **vhost-user does not work on macOS in this tree.** This is consistent with the implementation needing `memfd_create` (Linux-only) for file-backed guest memory so an external backend can mmap it. This invalidates the "zero libkrun patching for audio on macOS" assumption — see the rewritten §4. The header pre-defining `KRUN_VHOST_USER_DEVICE_SND` etc. is just the public C surface; the macOS backend wiring is absent.

---

## 1. What exists today

### 1.1 Audio (virtio-snd)

**There is no native virtio-snd device implemented in libkrun.** The device tree under `src/devices/src/virtio/` contains `balloon, block, console, fs, gpu, input, net, rng, vsock, vhost_user` — there is **no `snd/` directory**. The only sound path is the **generic vhost-user device**.

Key facts:

| Fact | Evidence |
|------|----------|
| `snd` Cargo feature exists in libkrun but is an **empty stub** | `src/libkrun/Cargo.toml:18` (`snd = ["vmm/snd"]`) and `src/vmm/Cargo.toml:22` (`snd = []`) — no code references the feature anywhere in `src/`. |
| `KRUN_VIRTIO_DEVICE_SND 25` and `KRUN_VHOST_USER_DEVICE_SND 25` are defined | `include/libkrun.h:747`, `include/libkrun.h:772` |
| Sound queue layout documented: 4 queues — control(0), event(1), TX/playback(2), RX/capture(3) | `include/libkrun.h:776-779` (`KRUN_VHOST_USER_SND_NUM_QUEUES 4`, `KRUN_VHOST_USER_SND_QUEUE_SIZES {64,64,64,64}`) |
| Generic vhost-user device API | `krun_add_vhost_user_device(ctx, device_type, socket_path, name, num_queues, queue_sizes_ptr)` — `include/libkrun.h:824`, impl `src/libkrun/src/lib.rs:1101` |
| The generic device is purely a **vhost-user proxy**: it forwards virtqueues to an external backend over a Unix socket; it does not implement virtio-snd semantics itself | `src/devices/src/virtio/vhost_user/mod.rs:20-70` (`VhostUserDeviceConfig` + `GenericDevice`), `create_generic_device` connects a `UnixStream` and builds a `VhostUserHandle` (`mod.rs:30-52`) |
| The generic vhost-user device is wired in the builder **Linux-only** | `src/vmm/src/builder.rs:976-990` (attach loop, `#[cfg(all(feature="vhost-user", target_os="linux"))]`), `:991` (macOS/`not(...)` arm), `attach_vhost_user_device` at `:2454-2455`. File-backed guest/kernel memory for the backend to mmap is likewise Linux-only (`:1378-1381`, `:1635-1638`). |

So, on the libkrun side, the **public C API** for audio is "stand up a vhost-user-snd backend process and call `krun_add_vhost_user_device(..., KRUN_VHOST_USER_DEVICE_SND, "/tmp/snd.sock", "vhost-snd", 4, KRUN_VHOST_USER_SND_QUEUE_SIZES)`" — but on **macOS that call has no backing implementation** (everything behind it is `target_os = "linux"`). The guest *would* see a standard virtio-snd (VIRTIO_ID_SOUND = 25) device, but only on a Linux host today.

**Two problems on macOS:** (1) libkrun's vhost-user plumbing is Linux-only and would need to be ported (it relies on `memfd_create`-backed memory; macOS would need a different shared-memory scheme so the backend can map guest RAM), and (2) even on Linux, the reference backend has no macOS sink. The reference backend is rust-vmm `vhost-device-sound` (crate `vhost-device-sound`, formerly `vhost-device-snd`). Its backends are **`null`, `pipewire`, `alsa`, and `gstreamer`** selected via `--backend`, socket via `--socket` (`vhost-device-sound --socket /tmp/snd.sock --backend null`). **There is no CoreAudio/macOS backend** — all are Linux audio systems. QEMU's vhost-user sound likewise has no CoreAudio backend.

Consequence: getting host audio on macOS is **not** a "just add a backend" task. Options: (a) implement a **native in-VMM virtio-snd device** in libkrun that talks to CoreAudio directly (fills the empty `snd` feature, no vhost-user/memfd dependency — cleanest on macOS); (b) port libkrun's vhost-user path to macOS **and** write a CoreAudio backend for `vhost-device-sound` (two large patches); (c) keep audio inside the guest and stream it out over vsock. See §4.

### 1.2 x86_64 emulation (Rosetta / qemu-user / box64)

| Fact | Evidence |
|------|----------|
| libkrun has a `rosetta` Cargo feature but it is an **empty stub** in this tree (`rosetta = []`, no body) and there is **no `krun_*rosetta*` C API** and no `rosetta`/`Rosetta` string anywhere in `src/` | `src/libkrun/Cargo.toml:23` (`rosetta = []`); `grep -i rosetta include/libkrun.h` → no match (exit 1); `grep -ri rosetta src/` → no match |
| Upstream container stacks (krunkit / podman machine) implement Rosetta **above** libkrun, not inside it | krunkit exposes `--krun-rosetta` (mounts Rosetta into the guest); see References |

**CRITICAL CONSTRAINT (verified via upstream):** Apple's Rosetta-for-Linux is **only available through `Virtualization.framework`** (`VZLinuxRosettaDirectoryShare`). libkrun on macOS uses **Hypervisor.framework** (HVF) directly, **not** Virtualization.framework, so it **cannot obtain the Rosetta runtime** the way vfkit/Virtualization.framework-based VMMs (UTM, Podman's applehv, Parallels) do. The Rosetta `rosetta` virtiofs share and the special guest-memory access Rosetta needs are provided by Virtualization.framework, not by HVF. The podman maintainers' analysis (Discussion #28297) concludes Rosetta cannot be supported on libkrun for both this technical reason and licensing (DMCA/EULA/trademark) reasons. **Therefore, for limina (libkrun + HVF), Rosetta is effectively off the table** unless we also stand up a Virtualization.framework-based helper, which defeats the point. This is a major divergence from how krunkit/podman "do Rosetta" (they use applehv = Virtualization.framework, not raw libkrun-HVF). Treat earlier "Rosetta via virtiofs" framing as the *Virtualization.framework* model, not available to us.

For reference, the Virtualization.framework model (what we *cannot* directly use): host shares the Apple Rosetta runtime via a virtiofs mount tagged `rosetta`; the guest registers the `rosetta` translator binary with `binfmt_misc` (`:rosetta:M::...x86_64...:/path/to/rosetta:OCF`) so x86_64 ELFs transparently invoke Rosetta in-guest; needs `softwareupdate --install-rosetta` on the host and `CONFIG_BINFMT_MISC=y` in the guest.

The alternatives that live entirely in the guest and need **no** host/Apple support — **these are the realistic path for limina**:
- **FEX-Emu** — high-performance x86→arm64 JIT; Linux-guest-native, no host hooks. Notably, **upstream krun/libkrun already auto-configures `binfmt_misc` inside the microVM for FEX when FEX-Emu is detected** (per upstream docs — see References; not found wired in *this* libkrun src tree, so treat as a krun-launcher/guest-image behavior, **verify**). Best general-purpose option for us since it is fully guest-side.
- **box64/box86** — user-space x86→arm64 translator; wired via `binfmt_misc`. Good compatibility, packageable in the guest image.
- **qemu-user-static** (`qemu-x86_64`) — full software emulation via `binfmt_misc`; slowest, but most compatible and trivially available in Fedora (`qemu-user-static-x86`). Good default fallback.

Guest kernel already supports this: `CONFIG_BINFMT_MISC=y` is set in the libkrunfw kernel configs (`third_party/libkrunfw/config-libkrunfw-sev_x86_64:854`, `...-tdx_x86_64:887`; the aarch64 guest config should be confirmed the same way — **verify** `config-libkrunfw_aarch64`).

### 1.3 virtiofs host-folder sharing

This is the workhorse for seamless file sharing **and** the transport for Rosetta. Three C entry points:

| API | Signature | Evidence |
|-----|-----------|----------|
| `krun_add_virtiofs` | `(ctx_id, c_tag, c_path)` | `include/libkrun.h:313-315` |
| `krun_add_virtiofs2` | `(ctx_id, c_tag, c_path, shm_size)` — adds the **DAX/shm window size** | `include/libkrun.h:317-320` |
| `krun_add_virtiofs3` | `(ctx_id, c_tag, c_path, shm_size, flags)` — adds a `flags` word | `include/libkrun.h:322-326` |
| Root-fs tag constant | `KRUN_FS_ROOT_TAG "/dev/root"` | `include/libkrun.h:102` |
| Overlay APIs (inject host files/dirs into a virtiofs tree) | `krun_add_virtiofs_overlay_file` / `..._overlay_dir` referenced in header | `include/libkrun.h:1210-1252` |

Implementation: full FUSE passthrough server in `src/devices/src/virtio/fs/` — `device.rs`, `server.rs`, `worker.rs`, `fuse.rs`, `filesystem.rs`, with platform passthrough backends `fs/macos/passthrough.rs` and `fs/linux/passthrough.rs`, plus virtual-overlay support (`augment_fs.rs`, `virtual_entry.rs`, `read_only.rs`, `null_fs.rs`). The DAX/shm window is real: `fs/device.rs` carries a `shm_region: Option<VirtioShmRegion>` with `set_shm_region()`/`shm_region()` (`fs/device.rs:49,:85,:101-102,:197,:220-221`), and the device advertises it to the guest as a virtio shared-memory region (the `shm_size` from `krun_add_virtiofs2/3` sizes it). This is the macOS-capable file-sharing path (unlike vhost-user, the fs device is **not** Linux-gated). **(verify)** the exact `FUSE_SETUPMAPPING`/SHMCAP request handling and window alignment in `fs/macos/passthrough.rs`.

Guest side: virtiofs needs `CONFIG_VIRTIO_FS=y` and (for DAX) `CONFIG_FUSE_DAX=y`; mount with `mount -t virtiofs <tag> <mnt>` (add `-o dax` when a shm window is configured).

### 1.4 RNG

| Fact | Evidence |
|------|----------|
| **Native** virtio-rng device exists in tree | `src/devices/src/virtio/rng/{device.rs,event_handler.rs,mod.rs}` |
| Also available as a generic vhost-user device | `KRUN_VHOST_USER_DEVICE_RNG 4` (`include/libkrun.h:771`), `KRUN_VHOST_USER_RNG_NUM_QUEUES 1`, queue size `{256}` (`include/libkrun.h:760-761`) |
| Public device-type id | `KRUN_VIRTIO_DEVICE_RNG 4` (`include/libkrun.h:743`) |

The native device is the simplest; it feeds the guest entropy pool from the host. Guest needs `CONFIG_HW_RANDOM_VIRTIO=y`.

### 1.5 RTC

| Fact | Evidence |
|------|----------|
| `KRUN_VIRTIO_DEVICE_RTC 17` defined | `include/libkrun.h:744` |
| RTC via vhost-user: 2 queues — requestq(0), alarmq(1), sizes `{1024,1024}` | `include/libkrun.h:765-768` |
| **No native `rtc/` device directory** in `src/devices/src/virtio/` | directory listing (only the 10 dirs listed in §1.1) |

So RTC, like snd, is **vhost-user-only** today — and since vhost-user itself is Linux-host-only in this tree (§1.1 correction), **there is no RTC device path on macOS at all** right now. The libkrunfw guest configs set `CONFIG_RTC_LIB=y`/`CONFIG_RTC_MC146818_LIB=y` but `# CONFIG_RTC_CLASS is not set` (`config-libkrunfw-sev_x86_64:1608-1610`), i.e. no userspace `/dev/rtc`. For a desktop VM, accurate wall-clock + suspend/resume correction matters; virtio-rtc (the new spec with alarms) would be the clean answer but needs a backend libkrun doesn't have on macOS. For our milestone, time should come from the guest's own NTP plus a one-shot sync on resume (driven by the limina agent).

### 1.6 virtio-console multiport (agent transport)

| Fact | Evidence |
|------|----------|
| Console device type id `KRUN_VIRTIO_DEVICE_CONSOLE 3` | `include/libkrun.h:742` |
| Multiport console with 4 queues for control/data | `include/libkrun.h:751-755` |
| Multiport API: `krun_add_console_port_tty`, `krun_add_console_port_inout` | `include/libkrun.h:1372`, `:1380` |
| Implicit console + explicit ports, naming `hvc0/hvc1...`, `ttyS0...` | `include/libkrun.h:1292-1340` |
| `krun_disable_implicit_console`, `krun_set_console_output`, `krun_set_kernel_console` | `include/libkrun.h:1175`, `:1051`, `:1289` |
| Full multiport implementation incl. a `vsock_port` bridge | `src/devices/src/virtio/console/{device.rs,port.rs,console_control.rs,vsock_port.rs,process_backend.rs}` |

For the limina guest agent (clipboard, dynamic-DPI, layout hints, balloon hints) the natural transport is **virtio-vsock** (separate device, see networking doc) or a dedicated **virtio-console port**. The console multiport with named ports is a clean, ordered, low-overhead channel that doesn't consume a network port number.

---

## 2. How it works end to end

### 2.1 Audio (vhost-user-snd path)

```
Guest userspace (PipeWire/PulseAudio/ALSA)
   │  ALSA virtio-snd PCM
   ▼
Guest kernel virtio-snd driver (snd-virtio, VIRTIO_ID_SOUND=25)
   │  4 virtqueues: ctrl(0) event(1) tx(2) rx(3); PCM frames in tx/rx descriptors
   ▼  (MMIO doorbell)
libkrun GenericDevice (vhost_user/mod.rs)  ── forwards via vhost-user protocol ──┐
   │  shares guest memory FDs + kicks/calls eventfds                              │
   ▼                                                                             ▼
vhost-user UNIX socket  ───────────────────────────────►  vhost-device-sound (host process)
                                                              │ reads PCM from guest mem (mmaped)
                                                              ▼ backend = null | pipewire | alsa
                                                          [MISSING ON macOS: CoreAudio backend]
```

Control flow: the guest's virtio-snd driver issues control messages (PCM info, set-params, prepare, start, stop) on the control queue; PCM data flows as descriptor chains on tx (playback) and rx (capture). libkrun's `GenericDevice` does **not** parse any of this — it negotiates vhost-user features (`src/devices/src/virtio/vhost_user/vu_common_ctrl.rs`, `VhostUserProtocolFeatures`), passes the guest memory table and per-queue kick/call eventfds to the backend, and the backend does all virtio-snd parsing and talks to the host audio API. The "macOS bridge" therefore lives **entirely in the backend process** — libkrun needs no audio code if we use vhost-user.

For a **native** libkrun virtio-snd device (the empty `snd` feature), the parsing would move into libkrun and the device thread would call CoreAudio (`AudioQueue`/`AudioUnit`/Core Audio HAL) directly from the VMM process.

### 2.2 Rosetta / x86 emulation

```
Host: softwareupdate --install-rosetta  →  /Library/Apple/.../OOPJitService + runtime
   │  bind/expose runtime dir
   ▼ krun_add_virtiofs(ctx, "rosetta", "<host rosetta runtime path>")
Guest: mount -t virtiofs rosetta /run/rosetta
   │  oneshot writes /proc/sys/fs/binfmt_misc/register:
   │    :rosetta:M::\x7fELF...x86_64...:/run/rosetta/rosetta:OCF
   ▼
exec ./some_x86_64_binary  → kernel binfmt_misc → /run/rosetta/rosetta translates → runs
```

Rosetta is a user-space translator; it reads/writes the calling process's memory and uses Apple's translation cache. No host VMM involvement after the mount — it's "just files + binfmt." box64/qemu-user/FEX follow the identical `binfmt_misc` pattern but the translator binary ships *in the guest*, so they need no host mount at all.

### 2.3 virtiofs (with DAX)

Guest `mount -t virtiofs <tag> <mnt>` → guest FUSE client issues FUSE ops over the virtio-fs request queue → libkrun `fs/server.rs` worker threads execute them against the host path via `passthrough.rs`. With a `shm_size` DAX window, file reads/writes use `FUSE_SETUPMAPPING` to map host file pages into the guest's DAX window (registered via `MmioShmManager` on macOS), avoiding per-read copies — important for low overhead on large file shares.

---

## 3. Options inventory for limina

### 3.A Audio

| Option | How | Pros | Cons |
|--------|-----|------|------|
| **A0. Do nothing / no audio** | omit snd device | zero work; fine for milestone-1 boot | not Parallels-parity; a desktop VM with no sound is unacceptable long-term |
| **A2. Native libkrun virtio-snd device → CoreAudio (recommended)** | implement a `snd/` device under `src/devices/src/virtio`, fill the empty `snd` feature, call CoreAudio (`AudioUnit`/HAL output+input) from a device thread in-process | works on macOS without vhost-user/memfd (the only model that does not require porting the Linux-gated vhost-user path); one process; lowest latency; no socket/fd juggling; full control of buffering for low overhead | most code (virtio-snd protocol parsing + CoreAudio); we own all maintenance; an audio bug can destabilize the VMM |
| **A1. Port vhost-user to macOS + add CoreAudio backend to `vhost-device-sound`** | make `builder.rs:976/991/1378/1635/2454` work on macOS with a non-memfd shared-mem scheme; run `vhost-device-sound --backend coreaudio --socket ...`; `krun_add_vhost_user_device(SND)` | reuses upstream virtio-snd parsing; process isolation (backend crash can't kill VMM) | **two** large patches incl. a macOS shared-memory rework of guest RAM; upstream has no CoreAudio backend; extra process + fds + a hop of latency; biggest scope |
| **A3. Audio inside guest, streamed out (PipeWire→vsock→host CoreAudio player)** | run PipeWire in guest, ship PCM over vsock to a small limina host player | no virtio-snd and no libkrun patch at all; works on macOS today (vsock is available) | high CPU/latency; clock-sync/xrun hell; reinvents what virtio-snd exists to avoid; bad A/V sync; still need a CoreAudio player on the host |
| **A4. vhost-user-snd `pipewire` backend + PipeWire-on-macOS** | stock backend + PipeWire daemon on macOS | no new backend code if PipeWire/macOS were viable | requires the macOS vhost-user port (A1); PipeWire on macOS is not first-class; fragile |

### 3.B x86_64 emulation

| Option | How | Pros | Cons |
|--------|-----|------|------|
| **B0. None** | — | zero work | no Intel apps; minor for a Linux desktop but a parity gap |
| **B1. Rosetta via virtiofs + guest binfmt_misc** | mount Rosetta runtime, register binfmt in guest | fastest x86→arm64 on Apple silicon; what krunkit/podman-applehv do | **NOT AVAILABLE to limina**: Rosetta-for-Linux requires Virtualization.framework; libkrun uses Hypervisor.framework (Discussion #28297). Plus Apple licensing constraints. Would require a separate VZ-based helper VM. |
| **B2. FEX-Emu in guest (recommended)** | package FEX in guest image, register binfmt_misc | no host/Apple dependency; ships in our image; fast JIT; fully guest-side so HVF/VZ distinction is irrelevant; krun has prior art auto-wiring FEX binfmt | we maintain guest packaging; JIT warmup; slower than Rosetta for some workloads |
| **B2b. box64 in guest** | package box64, binfmt_misc | good compatibility, no Apple dep | similar trade-offs to FEX |
| **B3. qemu-user-static** | `dnf install qemu-user-static-x86` | trivial, most compatible | slowest; fine as a fallback only |

### 3.C virtiofs / file sharing — there is no real alternative

| Option | Pros | Cons |
|--------|------|------|
| **C1. `krun_add_virtiofs3` with a DAX shm window (recommended)** | already implemented + macOS shm plumbing exists; low overhead via DAX; doubles as Rosetta transport; overlay APIs for injecting agent files | DAX details need verification (`fs/macos/mod.rs`) |
| C2. `krun_add_virtiofs` (no DAX) | simplest | per-read copies, higher overhead |
| C3. virtio-blk + a real fs | block-level perf | not "seamless folder sharing"; no live host visibility |

### 3.D RNG / RTC / console

| Device | Option | Note |
|--------|--------|------|
| RNG | **native virtio-rng** (in-tree `rng/`, not Linux-gated) | recommended; trivial; works on macOS; the vhost-user-rng fallback at `builder.rs:983-988` is Linux-only |
| RTC | vhost-user-rtc backend, **or** rely on guest NTP + resume-sync | no in-tree native RTC; defer a real virtio-rtc backend |
| Agent channel | **virtio-vsock** (preferred) or **virtio-console multiport port** | both in-tree; vsock is the cleaner socket API for the agent |

---

## 4. Recommendation

**Audio: pursue A2 (a NATIVE in-VMM virtio-snd device → CoreAudio). The vhost-user route (A1) is the wrong first move on macOS.**
Rationale (corrected from the first draft): libkrun's generic vhost-user path is **Linux-host-only** — `attach_vhost_user_device`, the RNG fallback, and the file-backed (memfd) guest memory a backend needs to mmap are all `#[cfg(all(feature = "vhost-user", target_os = "linux"))]` (`src/vmm/src/builder.rs:976,:991,:1378-1381,:1635-1638,:2454-2455`). The header pre-defining `KRUN_VHOST_USER_DEVICE_SND` (`include/libkrun.h:772-779`) is only the C surface; there is no macOS backing. So "zero libkrun patching via vhost-user" is **false on macOS**. The native device fills the already-reserved (empty) `snd` feature (`src/libkrun/Cargo.toml:18`, `src/vmm/Cargo.toml:22`) and the reserved `KRUN_VIRTIO_DEVICE_SND 25` id, modeled on the in-tree `rng/` and `console/` devices (which *do* work on macOS), with a device thread driving CoreAudio output/input units. This avoids the memfd/shared-memory problem entirely because the device runs inside the VMM with direct guest-memory access.

What must be built/patched for audio:
- **Native virtio-snd device in libkrun** under `src/devices/src/virtio/snd/` (control/event/tx/rx queues per `include/libkrun.h:776-779`), wired into the builder for macOS, with the `snd` feature filled.
- A **CoreAudio sink/source** (output `AudioUnit`/HAL first; input/mic second, incl. macOS TCC mic-permission handling).
- Guest: `CONFIG_VIRTIO_SND=m` (Fedora has it) + PipeWire/PulseAudio already present.
- Fallback if native proves too costly: A1 (port vhost-user to macOS + CoreAudio backend for `vhost-device-sound`) — larger scope, only if we want process isolation.

**x86 emulation: B2 (FEX-Emu in the guest) as primary, B3 (qemu-user-static) as always-available fallback. Rosetta (B1) is NOT available to limina.**
Rosetta-for-Linux is tied to Virtualization.framework; libkrun uses Hypervisor.framework, so we cannot obtain the Rosetta runtime without a separate VZ helper VM (Discussion #28297) — plus licensing constraints. FEX-Emu and box64 run entirely in the guest, so the HVF/VZ distinction is irrelevant; ship FEX in the guest image and register `binfmt_misc` (krun has prior art auto-wiring FEX). Ship qemu-user-static as the compatibility fallback. No libkrun patch is required for x86 emulation; it is all guest-image work.

**virtiofs: C1** — use `krun_add_virtiofs3` with a DAX shm window for the user's shared folders and the Rosetta runtime; use the overlay APIs (`include/libkrun.h:1210-1252`) to inject the guest agent and the binfmt oneshot without modifying the user's disk image.

**RNG: native virtio-rng** (just enable the in-tree device). **RTC: defer** a real backend; sync time via guest NTP and a resume hook. **Agent channel: virtio-vsock** (fall back to a named virtio-console port if vsock is contended).

Net libkrun patching for the recommended path: a **native virtio-snd device + CoreAudio sink** (the main effort) and, later, optionally a native RTC. Everything else (FEX/qemu binfmt, virtiofs shares, RNG) is either guest-image work or already-working in-tree devices. Note this is the reverse of the first-draft conclusion: vhost-user is **not** a shortcut on macOS.

---

## 5. Open questions / things to prototype

1. **Native virtio-snd device spike** — stand up a minimal `snd/` device modeled on `rng/`/`console/`, enumerate a virtio-snd card in the guest, and play a tone through a CoreAudio output `AudioUnit`. Measure round-trip latency vs. Parallels. This is the make-or-break for A2.
2. **CoreAudio buffering/clock model** — confirm we can match the guest virtio-snd period/buffer sizes to a CoreAudio render callback without xruns, and how to expose CoreAudio device changes (default-device switch, sample-rate) to the guest.
3. **Capture (microphone)** — confirm the rx queue path and CoreAudio input unit, including the macOS mic **TCC** permission prompt behavior inside the limina app bundle.
4. **Is porting vhost-user to macOS easier than a native device?** Scope what `builder.rs:1378-1381/1635-1638` (memfd-backed memory) needs on macOS — is there a `MAP_SHARED` file-backed scheme HVF tolerates? If cheap, A1 (with a CoreAudio `vhost-device-sound` backend) regains appeal for process isolation.
5. **virtiofs DAX on Apple-silicon (16 KiB host pages)** — read `fs/macos/passthrough.rs` + `fs/device.rs` (`VirtioShmRegion`) to confirm shm-window alignment and that `FUSE_SETUPMAPPING`/SHMCAP work with our `shm_size`; test `mount -t virtiofs ... -o dax`. Page-size mismatch is the most likely failure.
6. **FEX binfmt auto-wiring** — verify whether the FEX `binfmt_misc` auto-configuration attributed to krun applies to our libkrun-HVF launch path, or whether limina must do it from the guest agent/image. Confirm `CONFIG_BINFMT_MISC=y` in `config-libkrunfw_aarch64`.
7. **Native `snd`/`rtc` feature reality** — confirm `vmm/snd` and the reserved `KRUN_VIRTIO_DEVICE_SND/RTC` ids are truly inert (no half-wired path) so the native-device plan starts from a clean slate.
8. **Agent transport bake-off** — vsock vs. virtio-console multiport for the clipboard/DPI/keymap agent: latency, reconnect behavior, and whether the console `vsock_port` bridge (`console/`) gives us a hybrid. (vsock works on macOS; vhost-user-vsock does not.)

---

## 6. References

Local source (preferred ground truth):
- `third_party/libkrun/include/libkrun.h` — device ids and vhost-user constants: SND `:747,:772,:776-779`; RNG `:743,:760-761,:771`; RTC `:744,:765-768`; CONSOLE `:742,:751-755,:1292-1340,:1372,:1380`; virtiofs `:102,:313-326,:1210-1252`; generic vhost-user `:824`.
- `third_party/libkrun/src/libkrun/Cargo.toml:18` (`snd`), `:23` (`rosetta`) — empty stub features.
- `third_party/libkrun/src/vmm/Cargo.toml:22` (`snd = []`).
- `third_party/libkrun/src/libkrun/src/lib.rs:1101` — `krun_add_vhost_user_device`.
- `third_party/libkrun/src/devices/src/virtio/vhost_user/mod.rs:20-70` — `VhostUserDeviceConfig`, `GenericDevice`, `create_generic_device`.
- `third_party/libkrun/src/devices/src/virtio/vhost_user/vu_common_ctrl.rs` — vhost-user feature negotiation.
- `third_party/libkrun/src/vmm/src/builder.rs` — vhost-user is **Linux-only**: `:54-55,:98` (gated imports), `:976-990` (attach loop), `:991` (`not(...)` arm), `:1378-1381`/`:1635-1638` (memfd file-backed memory for backends), `:2454-2455` (`attach_vhost_user_device`), `:983-988` (Linux-only vhost-user-rng fallback).
- `third_party/libkrun/src/devices/src/virtio/fs/device.rs:49,:85,:101-102,:197,:220-221` — `VirtioShmRegion` DAX/shm window plumbing (macOS-capable).
- `third_party/libkrunfw/config-libkrunfw-sev_x86_64:854,:1608-1610` (`CONFIG_BINFMT_MISC=y`; no `RTC_CLASS`); `...-tdx_x86_64:887`.
- `third_party/libkrun/src/devices/src/virtio/fs/` — virtiofs server (`passthrough.rs`, `server.rs`, `worker.rs`, `fuse.rs`, `macos/mod.rs`).
- `third_party/libkrun/src/devices/src/virtio/rng/`, `.../console/` — native RNG and console multiport.

External:
- rust-vmm `vhost-device-sound` — backends **null/pipewire/alsa/gstreamer**, `--socket`/`--backend`; **no CoreAudio backend**. https://github.com/rust-vmm/vhost-device/blob/main/vhost-device-sound/README.md
- **podman Discussion #28297 — "Why Rosetta 2 Cannot Be Supported on libkrun"** (Hypervisor.framework vs Virtualization.framework + legal). https://github.com/containers/podman/discussions/28297
- Apple — Running Intel Binaries in Linux VMs with Rosetta (`VZLinuxRosettaDirectoryShare`, Virtualization.framework only). https://developer.apple.com/documentation/virtualization/running-intel-binaries-in-linux-vms-with-rosetta
- Podman Desktop — Rosetta on applehv (Virtualization.framework path, not raw libkrun). https://podman-desktop.io/docs/podman/rosetta
- FEX-Emu (https://fex-emu.com), box64 (https://github.com/ptitSeb/box64), qemu-user-static (Fedora `qemu-user-static-x86`).
- krunkit (vfkit-compatible REST front-end). https://github.com/containers/krunkit
