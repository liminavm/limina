# 01 — libkrun Internals & Full C API

Scope: exhaustive reference for libkrun as consumed by **limina** (a Rust macOS app, Apple Silicon host, Linux guest, willing to patch libkrun/deps). Covers the full `krun_*` C API, the display/input vtable ABIs, build feature flags + library flavors, the boot/init model, `krun_start_enter` exit/threading semantics, the balloon device, and the 2.0 `disable_implicit_*` direction. Every API claim is cited to verified source line numbers in the local checkout (repo version **1.18.0**, `src/libkrun/Cargo.toml:3`); the brew-installed dylib is **1.17.4** so a few APIs noted below are newer than what's on disk in Homebrew.

Verification status: the C headers, `src/libkrun/src/lib.rs` (all 3088 lines), the `Makefile`, `src/libkrun/Cargo.toml`, the balloon device, the macOS vstate exit path, and `nm`/`otool` on the brew dylib were all read directly. krunkit's Rust source could not be opened this session (tool output truncated); those points are labeled `[krunkit unread]`.

---

## 1. What exists today

### 1.1 API shape

A **builder over an opaque `uint32_t ctx_id`** — no handle struct is exposed. `krun_create_ctx()` returns a ctx id; setters/adders mutate a global `CTX_MAP: HashMap<u32, ContextConfig>` (`lib.rs:450`); `krun_start_enter(ctx_id)` removes the entry (`lib.rs:2860`) and runs the VM. Context ids come from a monotonic `AtomicI32` and the namespace is **not** recycled — exhausting it `panic!`s (`lib.rs:551-554`); libkrun is explicitly "not intended to be used as a daemon for managing VMs" (`lib.rs:552`). All functions return `int32_t`: **0 = success (`KRUN_SUCCESS`, `lib.rs:69`), negative = `-errno`**; a few return a positive id/fd.

**Function count:** the brew 1.17.4 dylib exports **61** `krun_*` symbols (`nm -gU`); the 1.18.0 source has **79** `#[no_mangle]` items but many are cfg-gated stub/real pairs, so the distinct C entry points number ~70. The "78" in the brief counts the display/input vtable callback typedefs too. Note the dylib exports **`krun_set_snd_device`** (a sound API present in 1.17.4 but **absent from the current repo header and lib.rs** — it was apparently removed/renamed toward the vhost-user sound path; do not rely on it).

#### Lifecycle & logging
| Function | header / lib.rs | Notes |
|---|---|---|
| `krun_set_log_level(level)` | `libkrun.h:28` / `lib.rs:465` | Global env_logger init. |
| `krun_init_log(target_fd, level, style, options)` | `libkrun.h:66` / `lib.rs:493` | Preferred. `target_fd=-1`→stderr; 1→stdout, 2→stderr, other fd→pipe (`lib.rs:494-501`). |
| `krun_create_ctx()` | `libkrun.h:74` / `lib.rs:535` | On macOS-aarch64 it eagerly creates the **shutdown eventfd** (`lib.rs:536-540`) and loads `KrunfwBindings` (`lib.rs:544`). |
| `krun_free_ctx(ctx)` | `libkrun.h:85` / `lib.rs:561` | |
| `krun_start_enter(ctx)` | `libkrun.h:1449` / `lib.rs:2839` | See §3.5 — runs forever, process dies via `libc::exit()`. |

#### vCPU / memory
| Function | header / lib.rs | Notes |
|---|---|---|
| `krun_set_vm_config(ctx, num_vcpus, ram_mib)` | `libkrun.h:98` / `lib.rs:569` | **Static RAM in MiB** → `VmConfig{vcpu_count, mem_size_mib, ht_enabled:false, cpu_template:None}` (`lib.rs:578-583`). No min/max range. Dynamic memory is NOT a config call (see §4). |
| `krun_get_max_vcpus()` | `libkrun.h:1142` / `lib.rs:2027` | macOS: `hv_vm_get_max_vcpu_count` (`lib.rs:2030-2032`). |
| `krun_set_nested_virt(ctx,bool)` / `krun_check_nested_virt()` | `:1097/:1107` / `lib.rs:1951/1964` | macOS path calls `hvf::check_nested_virt` (`lib.rs:1966`). |
| `krun_split_irqchip(ctx,bool)` | `:1154` / `lib.rs:2056` | **x86_64 only** — returns `-EINVAL` on aarch64 (`lib.rs:2057`). Irrelevant for us. |
| `krun_has_feature(feature)` | `:1134` / `lib.rs:2004` | Pure compile-time `cfg!(feature=…)` probe (`lib.rs:2005-2016`); unknown id → `-EINVAL`. |

`KRUN_FEATURE_*` ints (`lib.rs:1993-2001`, matching `libkrun.h:1110-1118`): NET=0, BLK=1, GPU=2, INPUT=4, TEE=6, AMD_SEV=7, INTEL_TDX=8, AWS_NITRO=9, VIRGL_RESOURCE_MAP2=10 (3 & 5 unassigned).

#### Boot: kernel / firmware / exec / root
| Function | header / lib.rs | Notes |
|---|---|---|
| `krun_set_firmware(ctx, path)` | `:924` / `lib.rs:2266` | Sets `FirmwareConfig{path}` (EFI flavor). `-EOPNOTSUPP`-style gated out under `tee`. |
| `krun_set_kernel(ctx, path, format, initramfs, cmdline)` | `:945` / `lib.rs:2180` | aarch64: format 0=Raw, 1=Elf, 2=PeGz, 3=ImageBz2, 4=ImageGz, 5=ImageZstd (`lib.rs:2200-2206`). On **x86_64 raw**, the kernel is `mmap`'d and treated as a bundled kernel at guest 0x8000_0000 (`lib.rs:2122-2166,2199`). |
| `krun_set_kernel_console(ctx, id)` | `:1289` / `lib.rs:2821` | Sets `console=`. |
| `krun_set_root(ctx, path)` | `:119` / `lib.rs:600` | virtiofs root, tag `/dev/root`, default **512 MiB DAX window** (`1<<29`, `lib.rs:616`), injects `init.krun` unless disabled (`lib.rs:620-622`). |
| `krun_set_workdir` / `krun_set_exec` / `krun_set_env` | `:890/:908/:963` / `lib.rs:1326/1360/1418` | Container-style run. exec/env/args are packed into the kernel cmdline as `KRUN_INIT=…`, `KRUN_WORKDIR=…` etc (`lib.rs:206-208,2905-2916`). |
| `krun_set_rlimits` | `:865` / `lib.rs:1292` | `KRUN_RLIMITS=` cmdline. |
| `krun_setuid` / `krun_setgid` | `:1067/:1083` / `lib.rs:2330/2343` | Applied with real `libc::setuid/setgid` **inside `krun_start_enter`** just before VMM build (`lib.rs:2990-3002`). |

#### Disks (feature `blk`)
| Function | header / lib.rs | Notes |
|---|---|---|
| `krun_add_disk(ctx, id, path, ro)` | `:174` / `lib.rs:728` | RAW only; macOS sync defaults to **Relaxed**, Linux **Full** (`lib.rs:754-757`). |
| `krun_add_disk2(…, format, ro)` | `:226` / `lib.rs:770` | RAW/QCOW2/VMDK via `ImageType::try_from` (`lib.rs:787`). |
| `krun_add_disk3(…, direct_io, sync_mode)` | `:278` / `lib.rs:818` | Full control. `SyncMode::try_from` (`lib.rs:842`). |
| `krun_set_root_disk_remount(ctx, device, fstype, options)` | `:1423` / `lib.rs:2358` | **Boots a real block root.** Requires ≥1 block device already added (`lib.rs:2411-2414`); builds a NullFs `/dev/root` serving only `init.krun` + mountpoints `dev/proc/sys/newroot` (`lib.rs:2419-2445`), then init pivots to the block device. `fstype="auto"`→None (`lib.rs:2375`). Sets `KRUN_BLOCK_ROOT_*` cmdline (`lib.rs:225-231`). **Primary candidate for booting the Fedora raw.** |
| `krun_set_root_disk` / `krun_set_data_disk` | `:135/:151` / `lib.rs:870/902` | **Deprecated** (block_id "root"/"data"). |

#### virtio-fs (feature-implicit; gated out only under tee/aws-nitro)
| Function | header / lib.rs | Notes |
|---|---|---|
| `krun_add_virtiofs/2/3` | `:313/:330/:349` / `lib.rs:636/647/659` | 1 & 2 delegate to 3 (`lib.rs:641,653`). NULL path → NullFs virtual-only (`lib.rs:676`). |
| `krun_fs_add_overlay_file(ctx, fs_tag, path, data, len, mode, one_shot)` | `:1236` / `lib.rs:2523` | Host-memory-backed virtual file; data **borrowed `'static`, not copied** (`lib.rs:2547-2553`). Supports nested paths via `resolve_overlay_path` (`lib.rs:2471`). |
| `krun_fs_add_overlay_dir(ctx, fs_tag, path, mode)` | `:1262` / `lib.rs:2570` | Virtual RO dir / mountpoint. |
| `krun_get_default_init(&data,&len)` | `:1207` / `lib.rs:2606` | Pointer to embedded `init_blob::INIT_BINARY` (`lib.rs:93,2613`). |
| `krun_disable_implicit_init(ctx)` | `:1187` / `lib.rs:2457` | Sets `disable_implicit_init`; **must precede `krun_set_root`** (verified by unit test `lib.rs:3069-3086`). |
| `krun_set_mapped_volumes` | `:300` / `lib.rs:718` | **Hard `-EINVAL` stub** — removed. |

#### Networking (feature `net`)
| Function | header / lib.rs | Notes |
|---|---|---|
| `krun_add_net_unixstream(ctx, path, fd, mac, features, flags)` | `:414` / `lib.rs:985` | passt / socket_vmnet. path xor fd (`lib.rs:1002-1007`). |
| `krun_add_net_unixgram(…)` | `:458` / `lib.rs:1044` | gvproxy / vmnet-helper. `NET_FLAG_VFKIT` → `UnixgramPath(path, send_vfkit_magic)` (`lib.rs:1080-1087`). **Best fit for limina NAT.** |
| `krun_add_net_tap(…)` | `:489` / `lib.rs:1105` | **Linux only** — macOS build is a `-EINVAL` stub (`lib.rs:1157-1165`). |
| `krun_set_net_mac` / `krun_set_passt_fd` / `krun_set_gvproxy_path` | `:544/:512/:532` / `lib.rs:1220/1170/1192` | Last two **deprecated** legacy single-NIC path (`lib.rs:1179,1207`). |
| `krun_set_port_map(ctx, map)` | `:571` / `lib.rs:1238` | `"host:guest"`; **requires vsock enabled** → `-ENODEV` otherwise (`lib.rs:1277`); rejects dup host/guest ports (`lib.rs:1262-1269`). |

NIC ids are assigned `eth0, eth1, …` in add order (`lib.rs:2110`). **TSI** is the default backend when no NIC is added (see §3.4). Net feature/flag bit constants live at `lib.rs:936-980`.

#### vsock / shutdown
| Function | header / lib.rs | Notes |
|---|---|---|
| `krun_add_vsock(ctx, tsi_features)` | `:1023` / `lib.rs:2645` | Requires implicit vsock disabled first or `-EEXIST` (`lib.rs:2659`). **macOS: `HIJACK_UNIX` unsupported → `-EINVAL`** (`lib.rs:2651-2653`). guest_cid hard-coded **3** (`lib.rs:2943,2969`). |
| `krun_add_vsock_port/2(ctx, port, path, listen)` | `:986/:1000` / `lib.rs:1467/1477` | host UNIX-socket IPC; `listen=true` requires the socket NOT pre-exist (`lib.rs:1493-1498`). |
| `krun_disable_implicit_vsock(ctx)` | `:1277` / `lib.rs:2632` | Sets `VsockConfig::Disabled`. |
| `krun_get_shutdown_eventfd(ctx)` | `:1035` / `lib.rs:1910` | macOS returns the **write** end of the eventfd (`lib.rs:1916`). Host writes it to request orderly shutdown. **Essential for limina.** |

#### GPU / display (feature `gpu`, which also pulls in `krun_display`; `Cargo.toml:17`)
| Function | header / lib.rs | Notes |
|---|---|---|
| `krun_set_gpu_options(ctx, virgl_flags)` | `:595` / `lib.rs:1517` | virgl flag bits at `libkrun.h:574-584` (VENUS=1<<6, NO_VIRGL=1<<7, USE_EXTERNAL_BLOB=1<<5, …). |
| `krun_set_gpu_options2(ctx, virgl_flags, shm_size)` | `:609` / `lib.rs:1531` | + host SHM "vRAM" window. |
| `krun_add_display(ctx, w, h)` | `:629` / `lib.rs:1680` | Returns display index; cap `MAX_DISPLAYS`=16 → `-ENOMEM` (`lib.rs:1684`). |
| `krun_display_set_edid / _dpi / _physical_size / _refresh_rate` | `:648/:663/:679/:693` / `lib.rs:1736/1804/1772/1704` | The non-EDID setters only apply to a `DisplayInfoEdid::Generated` EDID, else `-EALREADY` (`lib.rs:1714-1716,1782-1784`). |
| `krun_set_display_backend(ctx, vtable, size)` | `:706` / `lib.rs:1563` | Copies the `DisplayBackend` struct via `read_unaligned`, calls `.verify()`, stores in `vmr.display_backend` (`lib.rs:1568-1584`). **Without `gpu` feature: `-ENOTSUP` stub (`lib.rs:1551`).** |

#### Input (feature `input`, pulls in `krun_input`; `Cargo.toml:18`)
| Function | header / lib.rs | Notes |
|---|---|---|
| `krun_add_input_device(ctx, cfg, cfg_size, events, ev_size)` | `:722` / `lib.rs:1638` | Validates sizes + `.verify()`, pushes `(config, events)` into `vmr.input_backends` (`lib.rs:1649-1666`). **No `gpu` needed** — input is independent of display. Without `input` feature → `-ENOTSUP` (`lib.rs:1595`). |
| `krun_add_input_device_fd(ctx, fd)` | `:736` / `lib.rs:1608` | **Linux host only** (`PassthroughInputBackend` reading `/dev/input/*` ioctls, `lib.rs:1609`); useless on macOS. |

#### Console / serial
`krun_set_console_output` (`lib.rs:1929`), `krun_disable_implicit_console` (`lib.rs:2619`), `krun_add_virtio_console_default` (`lib.rs:2672`), `krun_add_serial_console_default` (`lib.rs:2800`), `krun_add_virtio_console_multiport`→console_id (`lib.rs:2700`), `krun_add_console_port_tty` (requires a real TTY → `-ENOTTY` else, `lib.rs:2737`) (`lib.rs:2718`), `krun_add_console_port_inout` (`lib.rs:2762`). A generic `inout` port (`/dev/vportNpM`) is the clean **guest-agent channel** for limina.

#### vhost-user (feature `vhost-user`) & SMBIOS
`krun_add_vhost_user_device(ctx, device_type, socket, name, num_queues, queue_sizes)` (`lib.rs:1826`; `-ENOTSUP` stub without the feature, `lib.rs:1897`). Device type ints = virtio IDs (`libkrun.h:742-748`). `krun_set_smbios_oem_strings` (`lib.rs:2072`). TEE: `krun_set_tee_config_file` (`lib.rs:1448`, tee-only).

### 1.2 Display backend ABI (`libkrun_display.h`, symlink → `src/display/libkrun_display.h`)

Software-framebuffer vtable. `struct krun_display_backend { uint64 features; void* create_userdata; create; union vtable }` (`:177-182`). `basic_framebuffer` vtable (`:165-171`): `destroy`(opt), `disable_scanout`, `configure_scanout(scanout_id, display_w, display_h, w, h, format)`, `alloc_frame(scanout_id, &buffer, &size)`→`frame_id`, `present_frame(scanout_id, frame_id, *damage_rect)`. Feature `KRUN_DISPLAY_FEATURE_BASIC_FRAMEBUFFER=1`. Pixel formats mirror virtio-gpu (`:18-33`). **Threading contract (`:148-163`): created on one thread, all methods called from that same thread; must not block long; caller MUST zero-init the struct.** limina's backend uploads the buffer to a Metal texture and presents in its `NSWindow`. This is the 2D scanout/blit path; 3D (virgl/Venus) renders GPU-side but still presents through the same scanout.

### 1.3 Input backend ABI (`libkrun_input.h`)

Two objects registered together. **Config** `struct krun_input_config` (`:150-155`) vtable (`:137-145`): `query_device_name/serial_name/device_ids/event_capabilities/abs_info/properties`. **Event provider** `struct krun_input_event_provider` (`:160-165`) vtable (`:84-88`): `get_ready_efd`→fd readable when events pending (required), `next_event(&out)`→1/0/neg non-blocking (required). `struct krun_input_event{u16 type; u16 code; u32 value}` (`:25-29`) is **binary-compatible with Linux/virtio input events**. limina translates `NSEvent`/`CGEvent`→linux `EV_*` codes in the event provider — this is where **Cmd↔Option swap, keymap remap, mouse capture, and macOS combo capture** all live. (Header redefines `krun_input_create_fn` at `:43` and `:122` — harmless.)

### 1.4 Flavors & build (verified: `Cargo.toml`, `Makefile`)

Cargo features (`src/libkrun/Cargo.toml:11-21`): `net, blk, gpu, input, vhost-user, virgl_resource_map2, tee, amd-sev (=blk+tee+…), tdx (=blk+tee+…), aws-nitro`. `gpu = ["vmm/gpu","devices/gpu","krun_display"]` (`:17`); `input = ["krun_input","vmm/input","devices/input"]` (`:18`). HVF dep only on macOS (`:39-40`); KVM deps only on Linux (`:42-47`). Library is both `cdylib` and `lib` (`:50`).

**Flavors** are `Makefile` variables, not distinct crates: `SEV=1`→`-sev`, `TDX=1`→`-tdx`, `AWS_NITRO=1`→`-awsnitro` (`Makefile:25-67`); the **EFI flavor is just `--features efi`-style** — note: there is **no `efi` Cargo feature**; firmware support is in the non-tee default build (`krun_set_firmware` is gated only by `not(tee)`, `lib.rs:2262`). "libkrun-efi" in upstream packaging = a build that bundles an EFI firmware via libkrunfw-efi; on macOS, `krun_set_firmware(path)` just points at a firmware file. Feature flags are passed à la `make GPU=1 INPUT=1 NET=1 BLK=1 VHOST_USER=1` (`Makefile:49-63`).

**macOS→Linux init cross-compile (verified `Makefile:105-127`):** on Darwin, if `SYSROOT_LINUX` unset, the Makefile **downloads a Debian sysroot** (`libc6, libc6-dev, libgcc-12-dev, linux-libc-dev`, `Makefile:166`) from deb.debian.org (`Makefile:172-191`) and cross-compiles `init/init.c` with Apple `clang -target aarch64-linux-gnu -fuse-ld=lld --sysroot … -B/-L gcc-lib-dir` (`Makefile:116-119`). Requires brew `lld`. The init is then embedded as `init_blob::INIT_BINARY` (`lib.rs:93`). (A FreeBSD sysroot path exists too, `Makefile:133-163`, irrelevant.)

**Brew dylib feature evidence (verified):** `otool -L /opt/homebrew/lib/libkrun.1.17.4.dylib` links **Hypervisor.framework, libepoxy.0, libvirglrenderer.1, libiconv, libSystem** → **HVF + gpu/virgl are compiled in**. `nm -gU` confirms `krun_add_display, krun_set_display_backend, krun_display_set_*, krun_add_input_device(_fd), krun_set_gpu_options/2, krun_add_net_*, krun_add_disk/2/3, krun_set_root_disk_remount, krun_get_shutdown_eventfd, krun_set_snd_device` are all exported — so brew 1.17.4 has **gpu + input + net + blk** at minimum. (No `krun_add_vhost_user_device`, `krun_fs_add_overlay_*`, `krun_disable_implicit_init`, `krun_add_console_port_*` in the 1.17.4 export list — those are **1.18 additions**, so building our own libkrun is advisable.)

### 1.5 libkrunfw & KrunfwBindings (verified `lib.rs:109-143,2285-2327`)

`KRUNFW_NAME` on macOS = `"libkrunfw.5.dylib"` (`lib.rs:84`), `dlopen`'d lazily (`lib.rs:109-110`). `KrunfwBindings` binds `krunfw_get_kernel(*guest_addr,*entry_addr,*size)->host_addr` (`lib.rs:113-116,131`) (+ `get_initrd`/`get_qboot` under tee). `load_krunfw_payload` reads those into a `KernelBundle` (`lib.rs:2285-2305`). If **no** external_kernel/kernel_bundle/firmware is set, `krun_start_enter` falls back to the bundled libkrunfw kernel (`lib.rs:2865-2878`). Whether that bundled kernel has the configs a desktop distro needs (virtio-gpu DRM, virtio-input, balloon, fbcon) is **unverified — must audit the libkrunfw kernel `.config`**.

### 1.6 macOS HVF & shutdown (verified `src/hvf/src/lib.rs`, `src/vmm/src/macos/vstate.rs`)

vCPU exits handled in `macos/vstate.rs`: MMIO read/write dispatch to the bus, and **`VcpuExit::Shutdown => VcpuEmulation::Stopped`** (`:407-409`). The vCPU run loop then does `Ok(VcpuEmulation::Stopped) => { self.exit(FC_EXIT_CODE_OK); break; }` (`:474-478`) (emulation errors → `FC_EXIT_CODE_GENERIC_ERROR`, `:480-482`). `fn exit` does NOT itself call `libc::exit` — it sends `VcpuResponse::Exited(exit_code)` on a channel and writes the `exit_evt` eventfd (`:511-519`); the VMM/EventManager observes that and tears the process down. Net effect from the C caller: the process terminates on guest shutdown. Shutdown is produced by the **PSCI** trap handler in HVF: guest `HVC` → `handle_psci_request` (`hvf/src/lib.rs:526`), with `SYSTEM_OFF (0x8400_0008)` and `SYSTEM_RESET (0x8400_0009)` both mapping to `VcpuExit::Shutdown` (`hvf/src/lib.rs:536-540`), and `CPU_ON (0xc400_0003)` for SMP bringup (`:542`). HVF requires the `com.apple.security.hypervisor` entitlement — repo `hvf-entitlements.plist` (verified present); **limina's VMM binary must be code-signed with it.**

---

## 2. (consolidated into §1 / §3)

## 3. How it works end to end

### 3.1 Build sequence
`create_ctx` → `set_vm_config` → choose boot (firmware+disk / external kernel / virtiofs root+exec / disk + `root_disk_remount`) → add devices → `start_enter`. Inside `start_enter` (`lib.rs:2839`): pick kernel source (`:2865-2878`), attach block devices (`:2880-2886`), assemble the kernel cmdline including the packed `KRUN_INIT=/KRUN_WORKDIR=/KRUN_BLOCK_ROOT_*=/KRUN_RLIMITS=/env` and `init=/init.krun` (`:2905-2916`), realize legacy net (`:2922-2936`), realize vsock (incl. **implicit TSI heuristic**, `:2938-2977`), apply gpu flags (`:2979-2984`), drop uid/gid (`:2990-3002`), `vmm::builder::build_microvm` (`:3006`), optionally start a GPU worker thread on macOS when virgl is enabled (`:3019-3022`), then **loop `event_manager.run()` forever** (`:3032-3040`).

### 3.2 Display flow (guest→host)
Guest virtio-gpu driver issues SET_SCANOUT/RESOURCE_FLUSH → libkrun's gpu device → backend `configure_scanout` → `alloc_frame` (CPU buffer) → pixel copy → `present_frame(damage)` (all on the GPU device thread, §1.2). limina blits to Metal. 3D (virgl/Venus→virglrenderer→MoltenVK→Metal) renders GPU-side, presented via the same scanout. A GPU worker thread is spun specifically on macOS+virgl (`lib.rs:3019-3022`).

### 3.3 Input flow (host→guest)
limina captures `NSEvent`→linux `EV_*`, enqueues `krun_input_event`s, signals `get_ready_efd`; libkrun's input device drains via `next_event` onto the virtio-input eventq → guest `/dev/input/eventN`.

### 3.4 Networking
- **TSI (default):** enabled when no NIC and no legacy net cfg (`lib.rs:2954`), `TsiFlags::HIJACK_INET` (`:2962`) — userspace socket impersonation over vsock; lowest overhead but **not a real NIC** (a desktop distro/NetworkManager won't be happy).
- **gvproxy NAT:** `krun_add_net_unixgram(path, -1, mac, feats, NET_FLAG_VFKIT)` → real virtio-net, NAT + `krun_set_port_map`.
- **bridged:** vmnet-helper / socket_vmnet via unixgram/unixstream (Apple `vmnet`, entitlement/root).

### 3.5 `krun_start_enter` exit & threading (VERIFIED)
`krun_start_enter` **does not itself call `exit()`** — it runs `loop { event_manager.run() }` (`lib.rs:3032-3040`) and only *returns* `-EINVAL/-ENOENT` on a **pre-boot** error. The process terminates on guest shutdown: guest PSCI `SYSTEM_OFF/RESET` → `VcpuExit::Shutdown` → `VcpuEmulation::Stopped` (`macos/vstate.rs:407-409`) → run loop `self.exit(FC_EXIT_CODE_OK); break` (`:474-478`); `fn exit` signals the VMM via a `VcpuResponse::Exited` channel + `exit_evt` eventfd (`:511-519`), which drives process teardown. So from the caller's perspective the function "never returns and the whole process dies on guest shutdown." **Consequence for limina: run the VMM in a dedicated child process** (krunkit's model) — the GUI process must not host `krun_start_enter`, or guest shutdown / a stray vCPU exit kills the whole app. GUI↔VMM communicate via a vsock guest-agent channel (or a `console_port_inout`) plus the shutdown eventfd; the GUI also owns the display/input backend callbacks (which run on VMM-side threads, so if backends live in the GUI process they'd need a cross-process transport — simplest is to keep backends in the VMM process and ship frames/events over IPC, or accept the in-process model and patch the `libc::exit` away).

### 3.6 Balloon / dynamic memory (VERIFIED — important)
A virtio-balloon device **is unconditionally attached** in `build_microvm` via `attach_balloon_device` (`builder.rs:973,2399`), created with **`devices::virtio::Balloon::new()` taking no arguments** (`builder.rs:2399`). It advertises 5 queues — inflate/deflate/stats/page-hint/free-page-reporting (`balloon/device.rs:16-24`) — but only enables features `VERSION_1 | STATS_VQ | FREE_PAGE_HINT | REPORTING` (`device.rs:27-30`); the only queue actually serviced is free-page-reporting, which `madvise(MADV_DONTNEED)`s reported guest pages (`device.rs:96-101`). Its config struct has a `num_pages` "pages host wants guest to give up" inflate-target field and an `actual` field (`device.rs:35-38`), **but NO `krun_*` API ever writes `num_pages`** — so today the balloon only does guest-driven free-page reclaim, never host-driven inflate/shrink. **Dynamic memory (min..max with host-driven ballooning) is therefore a libkrun patch**: add a control API that writes `num_pages` (and reads `actual`) plus likely a guest agent reporting `MemAvailable`.

### 3.7 krunkit `[krunkit unread this session]`
krunkit is the headless vfkit-compatible front-end that builds a libkrun ctx (disks, gvproxy net, virtiofs, gpu) and calls `krun_start_enter` in its own process — the closest existing model to limina minus GUI/display/input. Mine it for the REST→`krun_*` device mapping and gvproxy wiring.

---

## 4. Options inventory for limina

### Boot Fedora raw (milestone 1)
| Option | Pros | Cons |
|---|---|---|
| **A. Firmware (EFI) + `krun_add_disk(raw)`** — GRUB boots the distro's own kernel | unmodified distro; distro kernel has full drivers (virtio-gpu DRM/input/balloon) | need an EFI firmware bundled with libkrunfw-efi; brew ships plain `libkrun`+`krunkit`, so likely **build libkrun ourselves**. |
| **B. libkrunfw kernel + `krun_add_disk` + `krun_set_root_disk_remount("/dev/vda1",…)`** (`lib.rs:2358`) | uses the built-in init→pivot machinery; no EFI | libkrunfw's minimal kernel may lack desktop configs (would need kernel-config patch); Fedora rootfs may be btrfs/LVM/multi-partition, breaking a simple `/dev/vdaN` remount. |
| **C. External distro kernel via `krun_set_kernel` + initramfs** | full control | brittle, must extract & track kernel/initramfs per update. |
| **D. krunkit as-is** | zero work | headless; fails GUI/desktop goal. |

**Lean A long-term, B as fallback.**

### Display — software FB backend (Metal blit) now; virgl/Venus 3D later. `examples/gui_vm` (SDL backend) is the throwaway prototype.
### Input — custom config+event-provider backend (NSEvent→EV_*); `_fd` variant is Linux-only.
### Networking — gvproxy NAT first (brew ships gvproxy; `krun_add_net_unixgram`); vmnet bridged later; TSI only for headless.
### Dynamic memory — **must patch libkrun**: add balloon control API + guest agent. Static `krun_set_vm_config` is the only no-patch option (fails the goal).

---

## 5. Recommendation

1. **Process model: dedicated VMM child process** (krunkit-style). Verified rationale: `krun_start_enter` loops forever and the process dies via `libc::exit` from a vCPU thread on guest shutdown (`macos/vstate.rs:408-411`). Keep display/input backends in the VMM process; GUI drives via vsock agent + shutdown eventfd. *Optional later patch:* replace the `libc::exit` calls with an EventManager break + a real `krun_stop`, enabling in-process control + clean teardown.
2. **Build our own libkrun** (`make GPU=1 INPUT=1 NET=1 BLK=1 VHOST_USER=1`) — brew's 1.17.4 lacks 1.18 APIs (overlay files, multiport console ports, vhost-user, disable_implicit_init) and we'll be patching anyway. The macOS Debian-sysroot cross-compile of init is automatic (`Makefile:105-127`) given brew `lld`.
3. **Milestone-1 boot: Option A (firmware+disk)**; fallback B (`root_disk_remount`).
4. **Display:** software-framebuffer `krun_display_backend` (Metal). **Input:** custom backend. **Net:** gvproxy NAT.
5. **Must-patch in libkrun (ranked):** (High) balloon control C API for dynamic memory; (Med) non-`exit()` stop/teardown; (Med, conditional) EFI firmware bundling and/or libkrunfw kernel-config (virtio-gpu DRM, virtio-input, balloon, fbcon) if using B; (Low) zero-copy blob present for 3D perf.
6. **Code-sign** the VMM binary with `com.apple.security.hypervisor` (`hvf-entitlements.plist`); GUI needs Accessibility for global key capture.

---

## 6. Open questions / things to prototype

1. **Boot Fedora 43 raw end-to-end** via Option A; if it fails, try B and check whether the rootfs is a plain partition or btrfs/LVM (affects `root_disk_remount`).
2. **EFI firmware on macOS arm64** — does any brew package ship it, or must we build libkrunfw-efi / source EDK2?
3. **libkrunfw kernel `.config` audit** — virtio-gpu DRM / virtio-input / virtio-balloon / fbcon present? Determines if B works without kernel patching.
4. **Balloon control patch** — read `balloon/device.rs` + `event_handler.rs` fully; design a host-driven inflate/deflate API + guest pressure reporting; prototype min..max.
5. **In-process vs child-process VMM** — measure whether patching out `libc::exit` (replace with EventManager break) is clean enough to host the VMM in-process, or commit to the child-process IPC model.
6. **Display backend latency** — measure `present_frame`+Metal upload; confirm single-thread call contract; damage-tracking strategy.
7. **3D/Venus on MoltenVK** — spike `examples/gui_vm` / a Vulkan guest with `VENUS|NO_VIRGL` on M1 Max.
8. **`krun_set_snd_device`** in the 1.17.4 dylib but absent from 1.18 source — confirm the sound story (vhost-user-snd vs removed builtin).
9. **krunkit device model** — read `third_party/krunkit` REST→`krun_*` mapping + gvproxy/virtiofs wiring (unread this session).
10. **16 KiB host pages** vs 4 KiB guest — confirm libkrun/balloon/virtiofs-DAX handle the mismatch on M1.

---

## 7. References

Verified source (local checkout, repo 1.18.0):
- `src/libkrun/src/lib.rs` — all C exports (line numbers throughout).
- `include/libkrun.h`, `src/display/libkrun_display.h`, `src/input/libkrun_input.h` — ABIs.
- `src/libkrun/Cargo.toml` — features. `Makefile` — flavors + macOS init cross-compile.
- `src/vmm/src/macos/vstate.rs:408-411` (exit), `src/hvf/src/lib.rs:526-542` (PSCI shutdown).
- `src/vmm/src/builder.rs:973,2392-2408` + `src/devices/src/virtio/balloon/{device.rs,event_handler.rs,mod.rs}` (balloon).
- `otool -L` / `nm -gU` of `/opt/homebrew/lib/libkrun.1.17.4.dylib`.
- `hvf-entitlements.plist`, `init/init.c`, `examples/{gui_vm,boot_efi.c,external_kernel.c,consoles.c,chroot_vm.c}`.

To read next: `third_party/krunkit/` (REST/device model), `third_party/libkrunfw` (kernel `.config` + bindings), `src/devices/src/virtio/{gpu,input}`, `src/vmm/src/builder.rs` (full device wiring), `src/vmm/src/macos/vstate.rs` (vCPU loop).

External: libkrun 2.0 / implicit-resource removal — https://github.com/containers/libkrun/issues/634 ; https://github.com/containers/libkrun ; https://github.com/containers/krunkit
