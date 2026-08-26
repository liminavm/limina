# 10 — Guest agent & vsock control plane

Scope: design `limina-agent` (a small daemon shipped inside the Fedora Linux guest) and its
host-side counterpart, communicating over virtio-vsock provided by libkrun. This document is an
exhaustive reference for the relevant libkrun vsock/IPC facilities, enumerates the agent's
responsibilities (clipboard, dynamic display resize/DPI, dynamic-memory pressure + balloon hints,
time sync, heartbeat/guest-info, graceful shutdown, command channel, feature negotiation),
proposes a versioned wire protocol, covers guest-side install/auto-start, and recommends concrete
integration points (including likely libkrun patches).

All `libkrun.h:NN` and `*.rs:NN` citations below are verified against the local checkout at
`~/Projects/limina/third_party/libkrun`. A few internal claims are still flagged
**[VERIFY]** where I inferred behavior from partial reads.

---

## 1. What exists today

### 1.1 libkrun vsock / IPC / shutdown C API (VERIFIED)

| Function | Signature | Cite |
| --- | --- | --- |
| `krun_add_vsock_port` | `int32_t krun_add_vsock_port(uint32_t ctx_id, uint32_t port, const char *c_filepath)` — map a guest vsock `port` to a host **AF_UNIX** path; "a vsock port that the guest will connect to for IPC." | `libkrun.h:986` |
| `krun_add_vsock_port2` | `int32_t krun_add_vsock_port2(uint32_t ctx_id, uint32_t port, const char *c_filepath, bool listen)` — same, plus `listen` = "true if guest expects connections to be initiated from host side." | `libkrun.h:1000` |
| `krun_add_vsock` | `int32_t krun_add_vsock(uint32_t ctx_id, uint32_t tsi_features)` — add an explicit vsock device with a **TSI feature bitmask** (`KRUN_TSI_HIJACK_INET`/`UNIX`; **0 = no TSI hijacking**). Requires `krun_disable_implicit_vsock()` first. "Currently only one vsock device is supported." | `libkrun.h:1006–1023` |
| `krun_disable_implicit_vsock` | `int32_t krun_disable_implicit_vsock(uint32_t ctx_id)` — "disables that behavior entirely - no vsock device will be created." | `libkrun.h:1266–1277` |
| `krun_get_shutdown_eventfd` | `int32_t krun_get_shutdown_eventfd(uint32_t ctx_id)` — "Returns the eventfd file descriptor to **signal the guest to shut down orderly**. This must be called before starting the microVM." Host→guest direction (host writes to request shutdown). | `libkrun.h:1025–1035` |

Verified clarifications:

- **`krun_add_vsock`'s second arg is a TSI feature bitmask, not a CID.** Confirmed by the Rust
  `TsiFlags` bitflags (`HIJACK_INET = 1<<0`, `HIJACK_UNIX = 1<<1`) in
  `src/devices/src/virtio/vsock/mod.rs:33–43`. TSI is "enabled" iff the bitmask is non-empty
  (`mod.rs:46–49`). Plain vsock = `krun_disable_implicit_vsock()` then `krun_add_vsock(ctx, 0)`.
- **TSI C flags** `KRUN_TSI_HIJACK_INET (1<<0)`, `KRUN_TSI_HIJACK_UNIX (1<<1)` — `libkrun.h:360–362`.
- **`krun_add_vsock_port2`'s `listen`**: `listen=false` (or `krun_add_vsock_port`) = **guest
  connects out** to the host UDS (host listens). `listen=true` = host initiates. This maps
  directly to the muxer's per-port `(PathBuf, bool)` entry — see §1.3.
- **`krun_get_shutdown_eventfd` is host→guest orderly-shutdown** (`libkrun.h:1025`). NOTE the
  header comment says "with `krun_start_event`" but the actual start fn is `krun_start_enter`
  (`libkrun.h:1449`); the comment appears stale. **[VERIFY in lib.rs whether a `krun_start_event`
  alias exists.]**
- **No `KRUN_FEATURE_VSOCK`** constant exists (`libkrun.h:1109–1118`); vsock is implicit-by-default
  and not behind a build feature, so our dependency on it is safe.

### 1.2 Verified IPC-port routing in the device/muxer

The named IPC ports created by `krun_add_vsock_port` are carried all the way through as a map and
are **fully independent of TSI**:

- The device stores `unix_ipc_port_map: Option<HashMap<u32, (PathBuf, bool)>>` — port → (host UDS
  path, listen flag) — alongside `tsi_flags: TsiFlags`
  (`device.rs:38–39`, `device.rs:46–47`). These are separate fields; IPC ports work whether or not
  TSI hijacking is on.
- The muxer holds the same `unix_ipc_port_map` (`muxer.rs:109`, set in `new()` at `muxer.rs:130`)
  and on `activate()` passes it to the worker `MuxerThread::new(..., self.unix_ipc_port_map
  .clone().unwrap_or_default())` (`muxer.rs:154–164`), which sets up the per-port host endpoints.
  TSI's own time-sync thread is also spawned in the same `activate()` only on macOS
  (`muxer.rs:145–150`). This confirms **Option B (control port + TSI networking) is supported with
  no patch** — IPC ports and TSI are wired independently.
- Host endpoint impl is `unix.rs`: `UnixProxy` (guest-initiated, `connect()`s the host UDS,
  `unix.rs:323–359`) and `UnixAcceptorProxy` (host-initiated, `bind`+`listen` on the UDS,
  `unix.rs:622–646`). The `listen` flag selects between them.
- Flow control is standard virtio-vsock credit accounting (`buf_alloc`/`fwd_cnt`,
  `OP_CREDIT_UPDATE`/`OP_CREDIT_REQUEST`) handled in `unix.rs` (`sendmsg` credit logic
  `unix.rs:421–436`; `recv_to_pkt` honors peer credit `unix.rs:206–245`). Per-connection TX buffer
  is `CONN_TX_BUF_SIZE = 8 MiB` (`mod.rs:78`); max packet buf 64 KiB (`mod.rs:71`,
  `MAX_PKT_BUF_SIZE`). Treat the stream like TCP.

Constants worth knowing (all `mod.rs`): host CID `VSOCK_HOST_CID = 2` (`mod.rs:149`); device ID 19
(`mod.rs:113`); 3 virtqueues RX/TX/Event (`mod.rs:64–68`); ops
`REQUEST/RESPONSE/RST/SHUTDOWN/RW/CREDIT_UPDATE/CREDIT_REQUEST` (`mod.rs:119–131`); shutdown flags
`SHUTDOWN_RCV/SEND` (`mod.rs:137–139`); device advertises
`VIRTIO_F_VERSION_1 | VIRTIO_VSOCK_F_DGRAM` (`device.rs:63`).

### 1.3 Built-in time sync over vsock (macOS only) — RELEVANT, likely makes our TIME_SET redundant

`src/devices/src/virtio/vsock/timesync.rs` is compiled **only on macOS** (`mod.rs:17–18`,
`#[cfg(target_os = "macos")]`). A `TimesyncThread` periodically pushes host realtime to the guest
**over a dedicated DGRAM vsock port 123** (`timesync.rs:14`, `TSYNC_PORT: u32 = 123`):

- It sends an `OP_RW` DGRAM packet src/dst port 123, `src_cid=2`→guest, with a packed time via
  `pkt.write_time_sync(time)` (`timesync.rs:38–58`).
- Cadence: every 60 s (`UPDATE_INTERVAL`, `timesync.rs:12`) **or** when it detects a long sleep
  (woke up ≥3× the 2 s expected nap — i.e. host suspend/resume) (`timesync.rs:13, 64–75`).

So **libkrun already covers periodic + post-suspend host→guest time sync** on macOS. The guest
side must consume port-123 DGRAM packets and apply the time — **[VERIFY]** whether libkrun's own
guest init does this, or whether `limina-agent` must implement the port-123 listener. If the latter,
our agent reads port 123 instead of inventing a `TIME_SET` message.

### 1.4 The reference example (CORRECTED)

- `tests/guest-agent/src/main.rs` is **NOT a vsock agent** — it is a generic in-guest test runner
  that dispatches to `TestCase::in_guest()` by name (`main.rs:24–30`). Do not model the agent on
  it.
- The **real vsock reference** is `tests/test_cases/src/test_vsock_guest_connect.rs` (VERIFIED):
  - Port constant `VSOCK_PORT = 1234` (`:32`).
  - **Host:** `UnixListener::bind(sock_path)`, spawn an `accept()` server, then
    `krun_add_vsock_port(ctx, 1234, sock_path)` (`:64–74`). Host listens; no `listen` flag → guest
    connects out.
  - **Guest:** `socket(AF_VSOCK, SOCK_STREAM)`, `connect(VsockAddr::new(VMADDR_CID_HOST, 1234))`,
    then read/write a plain byte stream (`:95–110`). `VMADDR_CID_HOST` = 2.
  - This is exactly the limina control-plane shape: guest `connect(2, LIMINA_CTRL_PORT)` ↔ host
    `accept()` on the UDS from `krun_add_vsock_port`.

### 1.5 Other relevant verified API surface

- **virtiofs overlay (agent delivery).** Inject host-memory-backed virtual files/dirs into a
  virtiofs device with no host file:
  `krun_fs_add_overlay_file(ctx, fs_tag, path, data, data_len, mode, one_shot)` (`libkrun.h:1236`;
  "data pointer is NOT copied — caller must keep the memory valid for the full VM lifetime"),
  `krun_fs_add_overlay_dir` (`libkrun.h:1262`). `KRUN_FS_ROOT_TAG="/dev/root"` (`libkrun.h:104`);
  independent virtiofs via `krun_add_virtiofs/2/3` (`libkrun.h:313/330/349`). libkrun injects its
  own `/init.krun` this way (`krun_disable_implicit_init`/`krun_get_default_init`,
  `libkrun.h:1187/1207`), so the mechanism is first-class.
- **virtio-console multiport** (alt transport): `krun_add_virtio_console_multiport`
  (`libkrun.h:1361`) + `krun_add_console_port_inout(ctx, console_id, name, input_fd, output_fd)`
  (`libkrun.h:1400`) → bidirectional `/dev/vportNpM` byte channel.
- **Display knobs (host-set, pre-start config setters):** `krun_add_display(ctx,w,h)`
  (`libkrun.h:629`), `krun_display_set_edid` (`648`), `_dpi` (`663`), `_physical_size` (`679`),
  `_refresh_rate` (`693`), `krun_set_display_backend` (`706`). Whether callable post-`krun_start_enter`
  for live resize is **[VERIFY, prototype]**.
- **Input:** `krun_add_input_device` (`722`), `krun_add_input_device_fd` (`736`).
- **Build-feature probe:** `krun_has_feature` (`1134`); `KRUN_FEATURE_GPU 2`, `KRUN_FEATURE_INPUT 4`
  (`1112–1113`).
- **No balloon C API in the header.** `libkrun.h` exposes display/input/gpu/console/vsock/fs but
  **no `krun_*balloon*` symbol**. Dynamic memory has **no host control knob today** → a libkrun
  patch is required to set a balloon target / read inflation. (A balloon device dir exists under
  `src/devices/src/virtio/balloon` per the listing but is not wired to the public API.) **[VERIFY
  by grepping lib.rs for `balloon`.]**

### 1.6 macOS host facilities

- **Clipboard:** `NSPasteboard` (AppKit). No change notifications — poll `changeCount`. UTI-typed
  (`public.utf8-plain-text`, `public.png`, `public.file-url`).
- **AF_UNIX** for the host end of `krun_add_vsock_port` — standard BSD sockets, no entitlement.
  (libkrun's host endpoint sets `SO_NOSIGPIPE` and non-blocking on macOS, `unix.rs:72–85, 59–70`.)
- **eventfd shim:** libkrun uses `utils::eventfd::EventFd` (`device.rs:17, 55`); the fd from
  `krun_get_shutdown_eventfd` is waitable host-side. **[VERIFY shim type — kqueue/pipe.]**
- **Time:** `clock_gettime(CLOCK_REALTIME)` for any extra time push.

---

## 2. How it works end to end

### 2.1 Transport bring-up (recommended: guest connects out — matches the verified example)

```
HOST (limina)                              GUEST (Fedora)
-----------                              --------------
UnixListener::bind("/run/limina/<vmid>/ctrl.sock")
krun_add_vsock_port(ctx, LIMINA_CTRL_PORT, that path)   systemd starts limina-agent.service
host accept()s on the UDS  <----  agent: socket(AF_VSOCK, SOCK_STREAM);
                                        connect(cid=VMADDR_CID_HOST(2), LIMINA_CTRL_PORT)
            virtio-vsock OP_REQUEST -> OP_RESPONSE  (muxer UnixProxy connects host UDS)
host <-->  reliable, ordered, flow-controlled byte stream (8 MiB tx buf)  <--> agent
```

libkrun's `UnixProxy` connects the host UDS when the guest opens the port (`unix.rs:323`), bridges
RW packets, and manages credit. limina runs **one multiplexed, framed protocol** over this single
connection (§3). TSI networking can stay on simultaneously (§1.2).

### 2.2 Per-feature flows

- **Clipboard (guest→host):** guest copy → agent watches Wayland (`wl_data_device`) / X selection
  → `CLIPBOARD_OFFER{mimes}` → host caches; on paste host sends `CLIPBOARD_REQUEST{mime}` →
  `CLIPBOARD_DATA`. **host→guest:** host polls `NSPasteboard.changeCount`; on change sends
  `CLIPBOARD_OFFER`; guest serves the selection on request.
- **Display resize/DPI:** host window resize → `DISPLAY_CONFIG{w,h,dpi,refresh,scale}` → agent
  applies via Wayland output mode / `xrandr`. Host *may* also update EDID via `krun_display_set_*`
  iff callable post-start (open question §6).
- **Dynamic memory:** agent reads `/proc/pressure/memory` (PSI) + `/proc/meminfo`, sends
  `MEM_PRESSURE{...}`; host's controller computes a target in `[min,max]`. **No host balloon API
  today** (§1.5) — actuation needs a libkrun patch.
- **Time sync:** **largely already handled by libkrun's macOS `timesync.rs` over DGRAM port 123**
  (§1.3). limina should *consume* port 123 in-guest if libkrun's init doesn't; only add a custom
  message if gaps remain.
- **Heartbeat/guest-info:** `HELLO{ver,caps,os,kernel}` then periodic `HEARTBEAT`; absence = dead.
- **Graceful shutdown:** preferred = host sends `SHUTDOWN{reason}` → agent `systemctl poweroff`.
  Forcing fallback = host writes `krun_get_shutdown_eventfd` (verified host→guest orderly shutdown,
  `libkrun.h:1025`).
- **Command channel (run/login):** `EXEC{argv,env,cwd,uid,tty}` → spawn, stream `EXEC_DATA{fd,data}`,
  return `EXEC_EXIT{code,signal}`. Capability- + policy-gated (§5).

---

## 3. Proposed wire protocol (`limina-agent` v1)

### 3.1 Framing — length-prefixed, fixed 16-byte header, little-endian

```
struct FrameHeader {
    u32 magic;    // 0x474B564D ("LIMINA")
    u8  version;  // protocol major; v1 = 1
    u8  type;     // MessageType
    u16 flags;    // bit0 request, bit1 response, bit2 stream-cont, bit3 error
    u32 channel;  // correlation id (req<->resp) / bulk sub-stream id
    u32 length;   // payload length (cap, e.g. 16 MiB)
}                 // then `length` bytes of CBOR payload
```

Payload encoding: **CBOR** (`ciborium`/`serde`) — compact, evolvable, no codegen in the guest
build. (bincode faster but brittle; protobuf needs codegen; JSON too verbose for clipboard
images.)

### 3.2 Message types (v1)

| type | Name | Dir | Payload |
| --- | --- | --- | --- |
| 0x01 | HELLO | G→H | `{agent_ver, proto_min, proto_max, caps:[str], os, kernel}` |
| 0x02 | WELCOME | H→G | `{host_ver, proto_selected, caps:[str]}` |
| 0x03 | HEARTBEAT | G→H | `{uptime_s, load1, mem_free_kb}` |
| 0x10 | CLIPBOARD_OFFER | both | `{seq, mimes:[str]}` |
| 0x11 | CLIPBOARD_REQUEST | both | `{seq, mime}` |
| 0x12 | CLIPBOARD_DATA | both | `{seq, mime, data}` (chunk via stream-cont) |
| 0x20 | DISPLAY_CONFIG | H→G | `{output, w, h, dpi, refresh_mhz, scale}` |
| 0x21 | DISPLAY_STATE | G→H | `{outputs:[{name,w,h,dpi}]}` |
| 0x30 | MEM_PRESSURE | G→H | `{some10, some60, full10, avail_kb, total_kb, cached_kb}` |
| 0x31 | MEM_HINT | G→H | `{want_reclaim_kb}` |
| 0x32 | MEM_TARGET | H→G | `{target_kb}` (advisory) |
| 0x40 | TIME_SET | H→G | `{realtime_ns}` (only if libkrun port-123 timesync insufficient) |
| 0x50 | SHUTDOWN | H→G | `{reason, timeout_s}` |
| 0x51 | SHUTDOWN_ACK | G→H | `{}` |
| 0x60 | EXEC | H→G | `{id, argv, env, cwd, uid, tty}` |
| 0x61 | EXEC_DATA | both | `{id, fd, data}` |
| 0x62 | EXEC_EXIT | G→H | `{id, code, signal}` |
| 0x7F | ERROR | both | `{code, msg}` (flags.error) |

### 3.3 Negotiation & versioning

`HELLO`/`WELCOME` first. Each side advertises `proto_min..proto_max` + a `caps` set
(`"clipboard.image"`, `"exec"`, `"mem.psi"`, `"display.wayland"`). The cap intersection governs
valid types; unknown `type` → `ERROR{UNSUPPORTED}` (or dropped for fire-and-forget), never fatal.
Major bump = framing change (reject); minor additions are cap-gated.

---

## 4. Options inventory for limina

### A — Do nothing / reuse upstream example
The upstream `tests/guest-agent` is a test runner, not an agent (§1.4); `test_vsock_guest_connect`
is a 2-message ping/pong demo. Useful only as a transport smoke test. No clipboard/display/mem/
exec, no versioning.

### B — Single multiplexed control connection over one `krun_add_vsock_port` (RECOMMENDED)
Pros: one host UDS + one guest connection; framed protocol carries every feature; one
flow-controlled 8 MiB stream; **verified to coexist with TSI** (`unix_ipc_port_map` is independent
of `tsi_flags`, §1.2); matches the verified example shape (§1.4). Cons: head-of-line blocking if a
big clipboard image starves heartbeats — mitigate with chunking + a 2nd bulk port.

### C — One vsock port per feature
Pros: isolation, no HOL blocking. Cons: many `krun_add_vsock_port` calls, more host sockets/
threads, harder atomic negotiation. Overkill for v1. (Each port is one `UnixProxy`/`UnixAcceptorProxy`.)

### D — virtio-console multiport instead of vsock
`krun_add_virtio_console_multiport` + `krun_add_console_port_inout` (`libkrun.h:1361/1400`) →
`/dev/vportNpM`. Pros: trivial guest side, no AF_VSOCK kernel dep. Cons: no port namespace / no
multiple independent connections; less idiomatic. Reasonable fallback.

### E — Reuse an existing agent/protocol
- **SPICE vdagent:** clipboard + display-resize solved, but assumes SPICE/`virtio-serial`; libkrun
  has **no SPICE server**; needs a vsock⇄spice shim. Heavyweight.
- **qemu-guest-agent:** JSON-RPC over virtio-serial; wrong transport *for this* — it cannot carry
  clipboard, display or input, so it is no substitute for `limina-agent`. It is, however,
  **already installed** in every Fedora/Ubuntu desktop guest and additive per feature, so limina
  exposes its port and uses it as a stock-tier fallback (the guest clock today; see
  `crates/limina/src/qga/`). Rejected as *the* agent, adopted as an on-ramp — the same shape M12
  took for SPICE.
- Net: all assume virtio-serial and miss our GUI feature set; adapting > purpose-built vsock agent.

### F — Raw vsock: `krun_disable_implicit_vsock` + `krun_add_vsock(ctx, 0)` + own muxer
Pros: full control, no TSI. Cons: removes TSI networking → we own virtio-net + gvproxy/NAT entirely;
only one vsock device allowed (`libkrun.h:1012`). Defer unless networking area picks explicit vsock.

### Sub-options
- Encoding: **CBOR (rec.)** vs bincode vs protobuf vs JSON.
- Delivery: **virtiofs overlay (rec., `krun_fs_add_overlay_file`/`dir`)** vs bake into `.raw` vs
  initramfs/cmdline injection.

---

## 5. Recommendation

1. **Transport — Option B.** One multiplexed control connection over
   `krun_add_vsock_port(ctx, LIMINA_CTRL_PORT, "/run/limina/<vmid>/ctrl.sock")` with the **guest
   connecting out** (default, no `listen`), mirroring the verified `test_vsock_guest_connect` flow.
   Add a **second bulk port** for clipboard-image/file payloads once images land. **Keep TSI on**
   for milestone 1 (verified coexistence; Option F only if networking needs explicit vsock).
2. **Protocol — the v1 framed CBOR protocol in §3** with HELLO/WELCOME cap negotiation.
3. **Delivery — virtiofs overlay** via `krun_fs_add_overlay_file`/`krun_fs_add_overlay_dir`
   (verified): inject the `limina-agent` binary (or launcher) + a `limina-agent.service` unit, keeping
   the user's Fedora `.raw` untouched. **Validate where overlay files mount and what minimal
   in-image hook (baked unit vs systemd generator) auto-starts the agent early.**
4. **Time sync — reuse libkrun's macOS port-123 DGRAM timesync** (§1.3) rather than inventing
   `TIME_SET`; only confirm the guest consumes it.
5. **First-milestone scope (boot the `.raw` + prove the plane):**
   - Bring up the vsock control connection; implement `HELLO`/`WELCOME`/`HEARTBEAT` and
     `SHUTDOWN`/`SHUTDOWN_ACK`.
   - Wire `krun_get_shutdown_eventfd` as the forcing fallback.
   - Defer clipboard / display / mem-PSI / exec behind caps.

### What must be patched / built

- **Build by us:** `limina-agent` (Rust, static glibc/musl), `limina-agent.service`, host control
  client + dispatcher, virtiofs overlay layout + bootstrap unit, optional port-123 timesync
  consumer.
- **No libkrun patch needed for v1 transport** — `krun_add_vsock_port`, IPC-port routing,
  `krun_get_shutdown_eventfd`, overlay APIs, and macOS timesync are all present and verified.
- **libkrun patch REQUIRED for dynamic memory:** no balloon host API (§1.5). Add a host call to set
  a balloon target + read current inflation, and ensure inflation actually returns RAM to macOS
  (madvise on the guest mapping).
- **Possibly patch for live display:** if `krun_display_set_*` cannot be called post-start, add a
  runtime display-reconfig path (else drive resize purely in-guest via the agent).

---

## 6. Open questions / things to prototype

1. **`krun_start_event` vs `krun_start_enter`** — the shutdown-eventfd doc names `krun_start_event`
   (`libkrun.h:1027`) but only `krun_start_enter` exists in the header. Confirm in `lib.rs`.
2. **Guest CID value** — host CID is 2 (`mod.rs:149`); confirm the guest CID assigned by libkrun
   and AF_VSOCK kernel support in libkrunfw (`CONFIG_VSOCKETS`, `CONFIG_VIRTIO_VSOCKETS`).
3. **Who consumes timesync port 123 in the guest?** Does libkrun's guest init already apply it, or
   must `limina-agent` listen on DGRAM port 123 and `clock_settime`? (`timesync.rs` is host-side only.)
4. **Balloon:** add host API (target + current); does inflation `madvise(MADV_FREE/DONTNEED)` the
   guest RAM mapping back to macOS? Behavior under **16 KiB host pages** vs balloon's 4 KiB.
5. **Live display reconfig:** can `krun_display_set_edid/dpi/refresh_rate` run after
   `krun_start_enter` with guest re-probe, or must the agent drive the in-guest mode change?
6. **PSI in the guest kernel** (`CONFIG_PSI=y`, `psi=1` cmdline) for `/proc/pressure/*`.
7. **virtiofs overlay + systemd** — mount point of overlay files and minimal early-start hook.
8. **Reconnect semantics** — `UnixProxy` HANG_UP sends RST and defers removal (`unix.rs:548–562`);
   confirm the guest can reconnect the same port and re-negotiate without VM restart.
9. **eventfd shim type on macOS** for `krun_get_shutdown_eventfd` (kqueue/pipe) — host-side wait.
10. **DGRAM usage:** device advertises `VIRTIO_VSOCK_F_DGRAM` (`device.rs:63`) and timesync uses
    DGRAM; confirm whether our bulk/clipboard channel could use DGRAM or must stay STREAM.

---

## 7. References

Verified locally (path:line):
- `include/libkrun.h`: `krun_add_vsock_port` (986), `krun_add_vsock_port2` (1000), `krun_add_vsock`
  (1023), `krun_disable_implicit_vsock` (1277), `krun_get_shutdown_eventfd` (1035), TSI flags (360),
  overlay (1236/1262), `KRUN_FS_ROOT_TAG` (104), console multiport (1361/1400), display setters
  (629–706), input (722/736), `krun_has_feature` (1134), `krun_start_enter` (1449).
- `src/devices/src/virtio/vsock/mod.rs`: TsiFlags (33–49), constants (64–149).
- `src/devices/src/virtio/vsock/device.rs`: `unix_ipc_port_map`+`tsi_flags` fields (38–47),
  config space CID (49–52), advertised features (63).
- `src/devices/src/virtio/vsock/muxer.rs`: `unix_ipc_port_map` field (109), set in `new()` (130),
  passed to `MuxerThread` on activate (154–164), macOS timesync spawn (145–150), `MuxerRx` ops
  (32–75), `push_packet` (77–97), TSI proxy create gated by `tsi_flags` (282–332).
- `src/devices/src/virtio/vsock/unix.rs`: `UnixProxy::connect` (323), `UnixAcceptorProxy`
  (622–646), credit/recv logic (206–245, 421–436), macOS socket setup (59–85), HANG_UP/RST
  (548–562).
- `src/devices/src/virtio/vsock/timesync.rs`: macOS-only host→guest time push over DGRAM port 123
  (12–88).
- `src/devices/src/virtio/vsock/proxy.rs`: `Proxy` trait + `ProxyStatus` (28–98).
- `tests/test_cases/src/test_vsock_guest_connect.rs`: full guest-connect example (32–110).
- `tests/guest-agent/src/main.rs`: generic in-guest test runner, NOT a vsock agent (24–30).

External:
- virtio spec v1.2 §5.10 (vsock), §5.5 (balloon); `uapi/linux/virtio_vsock.h`.
- Linux PSI — `Documentation/accounting/psi.rst`, `/proc/pressure/*`.
- `NSPasteboard` (AppKit; poll `changeCount`).
- SPICE vdagent / qemu-guest-agent (Option E comparison).
