# 05 — Clipboard sharing

Scope: how to share clipboard contents (text, images, files; primary selection) bidirectionally between a Linux guest (Wayland/X11) running under libkrun and the macOS host (`NSPasteboard`). We own the guest image, so the recommended path is a custom `limina-agent` in the guest talking to a host-side `liminad` over **virtio-vsock**, with the host side bridging to `NSPasteboard`. This doc inventories that and the alternatives (spice-vdagent, virtio-console, wl-clipboard shims), covers format negotiation / large transfers / security, and shows how to plumb the channel with `krun_add_vsock_port2`.

---

## 1. What exists today

### 1.1 libkrun host<->guest channels relevant to clipboard

libkrun exposes three byte-stream / packet transports usable to carry a clipboard protocol. Only vsock is a true bidirectional multiplexed channel; the others are one-way or limited.

| Channel | Host C API | Direction | Multiplexed | Notes |
|---|---|---|---|---|
| virtio-vsock (unix-socket bridged) | `krun_add_vsock_port`, `krun_add_vsock_port2` | bidi | yes (per-port) | The right tool. Each port maps to one host `AF_UNIX` socket. |
| virtio-console multiport I/O port | `krun_add_virtio_console_multiport` + `krun_add_console_port_inout` | bidi | yes (per-device, multi-port) | Generic bidirectional stream over fds; viable but less convenient than vsock. |
| virtio-console output (implicit) | `krun_set_console_output(ctx_id, filepath)` | guest→host only | no | Logs guest console to a file. Not a clipboard transport. |
| virtio-net | `krun_set_net_*` | bidi | n/a | Could carry TCP, but heavyweight + couples to networking. Rejected. |

`krun_add_vsock_port` signatures (header `include/libkrun.h`). **Note: there is no `krun_add_vsock_port3` and no `flags` parameter in this libkrun (~v1.18); only the two below exist.**

```c
// include/libkrun.h:986
int32_t krun_add_vsock_port(uint32_t ctx_id, uint32_t port, const char *c_filepath);
// include/libkrun.h:1000  — adds `listen`
int32_t krun_add_vsock_port2(uint32_t ctx_id, uint32_t port, const char *c_filepath, bool listen);
```

`krun_set_console_output` is at `include/libkrun.h:1051`. The bidirectional generic console port API is `krun_add_virtio_console_multiport` (`:1361`) + `krun_add_console_port_inout(ctx, console_id, name, input_fd, output_fd)` (`:1400`).

Rust implementation (`src/libkrun/src/lib.rs`):

- The context struct stores the port map: `unix_ipc_port_map: Option<HashMap<u32, (PathBuf, bool)>>` — `lib.rs:178` (the `bool` is `listen`). There is **no** separate flags map.
- `krun_add_vsock_port` (`lib.rs:1467`) → delegates to `krun_add_vsock_port2(..., listen=false)` (`lib.rs:1472`).
- `krun_add_vsock_port2` (`lib.rs:1477`) → calls `cfg.add_vsock_port(port, filepath, listen)` (`lib.rs:1507`), which inserts `(PathBuf, listen)` into `unix_ipc_port_map` (`add_vsock_port` at `lib.rs:329`).
- At `krun_start_enter`, the map is copied into `VsockDeviceConfig.unix_ipc_port_map` (`lib.rs:2945`/`:2971`) and handed to the `VsockMuxer` (`muxer.rs:109`, `:117`).

**`listen` semantics** (header comment at `include/libkrun.h:998`: "true if guest expects connections to be initiated from host side"). Concretely, confirmed by the muxer connect path (`muxer.rs:549-553`, which sends RST if you try to connect to a `listen=true` port):

- `listen = false` (default): the **host** owns the `AF_UNIX` socket at `filepath`; the **guest connects out** to that vsock port (guest = client). The guest agent does `connect(AF_VSOCK, cid=2 (host), port)` and the muxer bridges to the host unix socket via `UnixProxy::new` (`muxer.rs:563`). This is the classic libkrun pattern (e.g. the bundled `tests/guest-agent`).
- `listen = true`: the **guest** side listens on the vsock port; the host connects in via the unix socket. Used when the host initiates.

For a clipboard we want **both directions to be event-driven**, so we will use one persistent connection (either direction works; see Recommendation) and run a full-duplex protocol over it. A single connected vsock stream is bidirectional, so one port suffices for both host→guest and guest→host clipboard updates.

### 1.2 vsock device internals (libkrun)

The vsock device lives under `src/devices/src/virtio/vsock/` (files: `device.rs`, `muxer.rs` (713 lines), `muxer_thread.rs`, `muxer_rxq.rs`, `unix.rs`, `proxy.rs`, `reaper.rs`, `packet.rs`, `tsi_stream.rs`, `tsi_dgram.rs`, `timesync.rs`, `event_handler.rs`, `mod.rs`):

- `muxer.rs` — `VsockMuxer` (struct at `:99`) maps vsock ports to host `AF_UNIX` proxies. On a guest connect to a `unix_ipc_port_map` entry it creates a `UnixProxy` (`muxer.rs:563`). It also handles TSI TCP/UDP proxy creation (`process_proxy_create` `:261`) and DGRAM listen requests (`process_listen_request` `:424`).
- `unix.rs` — `UnixProxy`, the `AF_UNIX` proxy bridging a vsock stream to a host unix socket.
- `proxy.rs` — the `Proxy` trait + `ProxyUpdate`/`ProxyRemoval`.
- `muxer_thread.rs` / `muxer_rxq.rs` — the dedicated muxer thread and RX queue; spawned in `VsockMuxer::activate` (`muxer.rs:135`). A reaper thread cleans up closed proxies (`muxer.rs:168`).
- `packet.rs` — vsock packet layout. Standard virtio-vsock credit-based flow control applies (the receiver advertises buffer space; sender blocks rather than overruns), which makes large transfers safe.

Host CID is the standard `VMADDR_CID_HOST = 2`; the guest CID is assigned by libkrun (the muxer holds `cid: u64`, `muxer.rs:100`).

Implication: vsock already gives us a reliable, flow-controlled, ordered byte stream per connection. We do **not** need to invent framing reliability; we only need an application-level message protocol on top.

### 1.3 What libkrun does NOT have

- **No spice/vdagent code** in libkrun: a tree-wide search for `spice`/`vdagent` under `src/` returned nothing. libkrun has no SPICE server, no virtio-serial `com.redhat.spice.0` port wiring, and no clipboard concept. (`grep -rli 'vdagent\|spice' src` → empty.)
- **There IS a general bidirectional virtio-console multiport device** (corrected from an earlier assumption): `krun_add_virtio_console_multiport` (`:1361`) creates a device; `krun_add_console_port_inout(ctx, id, name, input_fd, output_fd)` (`:1400`) adds a generic, non-TTY bidirectional port that appears in the guest as `/dev/vportNpM`. This is close to what spice-vdagent expects (a named virtio-serial port), so Option C / a virtio-serial-style transport is **not** as impossible as first thought. However it is fd-based (host owns two pipe fds) and single-connection per port, with no built-in muxing/reconnect — vsock is still the better fit. The implicit `krun_set_console_output` (`:1051`) remains output-to-file only.
- **No built-in clipboard agent** in `libkrunfw` guest kernel/initramfs beyond what we add.

### 1.4 macOS host clipboard facilities

- `NSPasteboard` (AppKit) — the system clipboard. `[NSPasteboard generalPasteboard]`. Reads/writes typed data via Uniform Type Identifiers (UTType).
  - Common UTIs: `public.utf8-plain-text` (text), `public.png` / `public.tiff` (images), `public.file-url` (file references), `public.rtf`.
  - Change detection: `NSPasteboard.changeCount` — increments on every write by any app. There is **no push notification** for clipboard changes on macOS; you must **poll** `changeCount` (typical 100–250 ms timer) to detect host-side copies. This is the same approach Parallels/VMware/UTM use.
  - Writing: `declareTypes:owner:` + `setData:forType:`, or `writeObjects:` with `NSPasteboardItem`s. Lazy/promised data is possible (`pasteboard:provideDataForType:`), useful for deferring large/image payloads until paste.
- File transfer: macOS file URLs are host-filesystem paths and meaningless inside the guest. "Copy a file" requires actually transferring bytes (or exposing via virtio-fs and rewriting the URL). Treat files as a later milestone.
- Clipboard must be touched on the **AppKit main thread** in practice; we will marshal pasteboard access onto a dedicated host thread / run loop.

### 1.5 Linux guest clipboard facilities

- **Wayland** (Fedora 43 default = GNOME on Wayland): no global clipboard API for arbitrary clients. The compositor owns selections. Options to read/write the Wayland clipboard from an agent:
  - `wlr-data-control-unstable-v1` / `ext-data-control-v1` protocols — let a privileged client get/set the clipboard and **primary selection** without being focused. `wl-clipboard` (`wl-copy`/`wl-paste`) uses these. **GNOME/Mutter historically did NOT implement `wlr-data-control`**; this is the single biggest guest-side risk. Verify on Fedora 43 / Mutter version. If absent, we need either the newer `ext-data-control-v1` (if Mutter supports it) or a GNOME Shell extension / portal, or we fall back to XWayland.
  - `org.freedesktop.portal.Clipboard` exists only inside the RemoteDesktop session context — not a general clipboard API.
- **X11 / XWayland**: standard `XFIXES` selection-owner-change notifications + `XConvertSelection`. `xclip`/`xsel`/`wl-clipboard`'s X backend work here. Reliable but only covers X11 apps (under GNOME Wayland, XWayland clipboard is bridged to the Wayland clipboard by Mutter, so an XFIXES-based agent often suffices in practice — verify).
- **Primary selection** (middle-click paste) is a separate selection; macOS has no equivalent, so primary↔NSPasteboard is a design choice (most VM tools ignore primary or map it only guest-internally).

---

## 2. How it works end to end

Target architecture (recommended option A). Components:

```
 macOS host                                   Linux guest
 ┌─────────────────────────────┐             ┌──────────────────────────────┐
 │ liminad (our VMM process)     │             │ limina-agent (our daemon)      │
 │  ├ NSPasteboard poller      │             │  ├ Wayland data-control or   │
 │  │  (changeCount timer)     │             │  │  XFIXES selection watcher │
 │  ├ clipboard bridge thread  │  vsock      │  ├ clipboard bridge          │
 │  │  ⇅ AF_UNIX socket  ◄─────┼── port N ──►│  │  ⇅ AF_VSOCK connect cid 2│
 │  └ libkrun (muxer→UnixProxy)│             │  └ protocol codec            │
 └─────────────────────────────┘             └──────────────────────────────┘
```

### 2.1 Setup / plumbing

1. Host (liminad) picks a vsock port, say `1025`, and a host unix-socket path, e.g. `$XDG_RUNTIME_DIR/limina/clip.sock`.
2. Before `krun_start_enter`, liminad calls `krun_add_vsock_port2(ctx, 1025, "/.../clip.sock", false)`.
   - `listen=false` ⇒ host owns the unix socket; guest connects out to vsock port 1025 on CID 2 (host).
   - libkrun stores `(path, false)` in `unix_ipc_port_map[1025]` (`add_vsock_port`, `lib.rs:329`; map field `lib.rs:178`).
3. At VM start, libkrun's vsock muxer registers a `UnixProxy` for port 1025 bound to that path (`muxer.rs:563`).
4. In the guest, `limina-agent` opens `socket(AF_VSOCK)` and `connect()` to `(cid=VMADDR_CID_HOST=2, port=1025)`. The muxer accepts and bridges this vsock stream to the host unix socket: `liminad` `accept()`s the unix connection. Now we have one full-duplex byte pipe.
5. Both ends run the limina clipboard protocol (sec 2.4) over that pipe. Reconnect with backoff if the agent restarts.

### 2.2 Host copy → guest paste (data flow)

1. Host poller fires; `NSPasteboard.generalPasteboard.changeCount` changed.
2. liminad reads **available type list** (UTIs) from the pasteboard. It does **not** eagerly fetch large payloads.
3. liminad sends an `OFFER` message to the guest listing normalized formats (e.g. `text/plain;charset=utf-8`, `image/png`, `text/uri-list`) and a monotonically increasing serial.
4. guest agent receives `OFFER`, takes ownership of the Wayland/X11 selection advertising those MIME types (lazy — it does not yet have the bytes).
5. When a guest app pastes, the compositor asks the agent for `image/png` (etc.). The agent sends a `REQUEST{serial, mime}` to the host.
6. liminad fetches that UTI's data from `NSPasteboard` (now, lazily) and streams it back as `DATA{serial, mime, chunks...}` (sec 2.3 chunking). Agent writes the bytes to the compositor's fd.

### 2.3 Guest copy → host paste

Mirror image: agent watches selection-owner changes (data-control `selection`/`primary_selection` events, or XFIXES `SelectionNotify`). On change it sends `OFFER` with the guest's MIME types. liminad registers itself as `NSPasteboard` owner declaring the mapped UTIs (lazy via promised data). When a host app pastes, AppKit calls our `provideDataForType:`, which sends `REQUEST` to the guest and blocks (with timeout) until `DATA` arrives, then returns it.

### 2.4 Protocol sketch (length-prefixed binary over the vsock stream)

```
Frame:  u32 len (LE, of payload) | u8 type | payload[len-1]

Types:
  HELLO    { u16 version, u32 features }          // both directions, handshake
  OFFER    { u64 serial, u8 selection, repeated { str mime } }
           // selection: 0=clipboard, 1=primary
  REQUEST  { u64 serial, u8 selection, str mime }
  DATA_HDR { u64 serial, str mime, u64 total_len }
  DATA     { u64 serial, u32 chunk_len, bytes }   // 0-len chunk = EOF
  DATA_ERR { u64 serial, u16 code }               // owner vanished / too big / cancelled
  CLEAR    { u8 selection }                        // selection emptied
  PING / PONG
```

- **Serial** invalidates stale transfers: if the source clipboard changes mid-transfer, the new `OFFER` bumps the serial and the receiver drops in-flight `DATA` for old serials.
- **Chunking**: send `DATA_HDR` then `DATA` frames (e.g. 32–64 KiB each). vsock per-stream credit flow control naturally back-pressures the sender, so a 200 MB image won't OOM either side; the agent streams into the compositor fd / a temp file as it arrives.
- **Format negotiation**: a static MIME↔UTI mapping table on the host (`public.utf8-plain-text`↔`text/plain;charset=utf-8`, `public.png`↔`image/png`, `public.tiff`↔`image/tiff`, `public.html`↔`text/html`, `public.file-url`↔`text/uri-list`). Unmapped types are dropped. Text is normalized to UTF-8; CRLF/LF left as-is (or normalized — decide in spike).

### 2.5 Threads

- Host: one clipboard thread owning the unix-socket connection + protocol codec; pasteboard access marshalled to the AppKit main run loop (or a dedicated NSPasteboard thread). vsock muxer runs in libkrun's existing device threads — no new VMM thread needed.
- Guest: one agent thread on the vsock fd + Wayland/X11 event loop (can be a single epoll loop over vsock fd + wayland fd).

---

## 3. Options inventory for limina

### Option A — Custom `limina-agent` over virtio-vsock (RECOMMENDED)
Guest daemon ↔ host `liminad`, vsock bridged to `AF_UNIX`, our own protocol bridging Wayland/X11 ↔ `NSPasteboard`.

- **Pros**: Uses the supported, already-working libkrun vsock path (`krun_add_vsock_port2`, `lib.rs:1477`); no libkrun patch required for the channel. Full control over format mapping, chunking, primary selection, security policy, lazy/promised transfers. One port, full-duplex. Standard virtio-vsock credit flow control for free. We own the guest image so shipping the agent is trivial.
- **Cons**: We write and maintain both ends + the Wayland data-control / XFIXES integration (the genuinely hard part is guest-side: getting clipboard access under GNOME/Mutter Wayland). No code reuse from existing ecosystems.

### Option B — spice-vdagent + vsock transport shim
Run upstream `spice-vdagent`/`spice-vdagentd` in the guest; bridge the SPICE VDI protocol over vsock to a tiny host-side SPICE-clipboard endpoint that talks `NSPasteboard`.

- **Pros**: `spice-vdagent` already solves Wayland (it uses GNOME's Mutter Wayland clipboard integration / X11) and handles text/image/file negotiation, primary selection, and large transfers. `spice-protocol` headers are installed on the host. Mature, battle-tested.
- **Cons**: spice-vdagent expects a **virtio-serial port** (`/dev/virtio-ports/com.redhat.spice.0`) or a SPICE socket. libkrun's `krun_add_console_port_inout` (`:1400`) can provide a named bidirectional `/dev/vportNpM` port that gets us close, but the host still has to re-implement a partial SPICE server speaking the VD_AGENT clipboard messages (`VD_AGENT_CLIPBOARD`, `VD_AGENT_CLIPBOARD_GRAB/REQUEST/RELEASE`) and translating to `NSPasteboard` (no macOS SPICE client to reuse). High glue cost for partial reuse; ties us to SPICE's message model and to udev/port-naming conventions vdagent expects. Not clearly less work than A.

### Option C — virtio-console / virtio-serial port
Carry a clipboard protocol over a dedicated virtio-console generic I/O port.

- **Status (corrected):** libkrun **does** expose this: `krun_add_virtio_console_multiport` (`:1361`) + `krun_add_console_port_inout(ctx, id, name, input_fd, output_fd)` (`:1400`) gives a named bidirectional `/dev/vportNpM` device. The host side is a pair of pipe fds.
- **Pros**: Available without patching libkrun. A named port (e.g. `name="com.limina.clipboard.0"`) is exactly the model spice-vdagent expects, so this is the natural transport **if** we go with Option B/spice.
- **Cons vs vsock**: single connection per port (no accept/reconnect semantics — if the agent restarts, the fd state is awkward), no per-connection muxing, host manages raw pipe fds, no credit-flow API surface beyond the virtqueue. For a bespoke agent (Option A), vsock is strictly more convenient (connect/accept, reconnect, multiple ports). Choose this only in service of Option B.

### Option D — wl-clipboard / xclip shims driven over vsock
Don't write a guest daemon with native protocol code; instead the host drives `wl-copy`/`wl-paste` (or `xclip`) in the guest via a thin command channel over vsock.

- **Pros**: Minimal guest code — reuse `wl-clipboard` for the Wayland data-control plumbing. Fast prototype.
- **Cons**: Still requires `wlr-data-control`/`ext-data-control` support in Mutter (same blocker as everything Wayland). Polling-based and racy (no event-driven selection-change signal without `wl-paste --watch`, which is itself a long-running process you must manage). Awkward for images/binary/large data (piping through CLI tools, temp files). No primary-selection event model. Good enough for a **text-only spike**, not for production. Effectively a degenerate Option A where the agent shells out to wl-clipboard instead of using libwayland directly — reasonable as a milestone-1 stopgap.

### Option E — Do nothing / reuse upstream as-is
- **Cons**: libkrun ships no clipboard. krunkit is headless and has no clipboard. There is nothing to reuse turnkey. Not viable for a Parallels replacement.

---

## 4. Recommendation

**Pursue Option A (custom `limina-agent` over vsock).** Rationale:

1. The transport already exists and is supported: `krun_add_vsock_port2(ctx, port, sock_path, listen=false)` (`lib.rs:1477`), backed by `UnixProxy` in the vsock muxer (`muxer.rs:563`). **No libkrun patch is needed for clipboard.** vsock gives connect/accept + reconnect + multi-port semantics that the virtio-console port (Option C) lacks.
2. We own the guest, so shipping a small daemon is cheap and gives full control over format negotiation, lazy/promised transfers, chunked large payloads, primary selection, and security policy.
3. vsock's per-stream credit flow control handles large/streaming transfers without custom backpressure.

**Phasing:**
- **M1 (text only, fastest):** Implement Option A protocol limited to `text/plain;charset=utf-8`, clipboard selection only, both directions. For the guest selection access, start with the **Option D shortcut** internally — have the agent shell out to `wl-paste --watch` / `wl-copy` (or xclip under XWayland) if `wlr/ext-data-control` is present; this de-risks the Wayland integration while validating the vsock channel + host NSPasteboard poller.
- **M2:** Replace shell-outs with direct `libwayland` `ext-data-control-v1`/`wlr-data-control` (or XFIXES fallback). Add image (`image/png`) support and chunking. Add `changeCount` polling tuning.
- **M3 (deferred):** files (`text/uri-list`) via virtio-fs staging + URL rewriting; primary selection; HTML/RTF.

**What we build (no libkrun patch required for the channel):**
- Host: `liminad` clipboard module — unix-socket server, protocol codec, NSPasteboard poller (changeCount), MIME↔UTI mapping, promised-data provider.
- Guest: `limina-agent` — vsock client, protocol codec, Wayland data-control / XFIXES integration, ship it in our guest image + a systemd user service.

**Possible (optional) libkrun patches** — only if needed:
- None for clipboard transport. (If we ever pursue spice-vdagent, the existing `krun_add_virtio_console_multiport`/`krun_add_console_port_inout` ports can host the `com.redhat.spice.0`-style channel without a new device — but we'd still write the host-side NSPasteboard bridge.)

---

## 5. Open questions / things to prototype

1. **Mutter clipboard access on Fedora 43 (the #1 risk).** Does GNOME/Mutter on Wayland implement `wlr-data-control-unstable-v1` or `ext-data-control-v1`? If not, can an unfocused agent read/set the clipboard at all without a Shell extension? Spike: run `wl-paste -l` and `wl-copy` from a non-focused process under GNOME 45+/46+ on Fedora 43.
2. **XWayland bridging.** Under GNOME Wayland, does an XFIXES-based agent see/set the unified clipboard (Mutter bridges XWayland↔Wayland)? If yes, an X11-only agent might suffice for M1/M2 and dodge the data-control question entirely. Spike: `xclip` under XWayland.
3. **vsock connect direction & timing.** Confirm the guest can `connect(AF_VSOCK, cid=2, port)` to a `listen=false` host port, and reconnect cleanly after agent restart (note muxer sends RST when connecting to a `listen=true` port, `muxer.rs:549-553`). Confirm host `AF_UNIX` path lifecycle (does libkrun bind on start, or expect liminad to listen first?). Validate against `tests/guest-agent`.
4. **NSPasteboard polling interval & loop-back.** Tune `changeCount` poll rate vs. CPU; ensure we ignore writes that originate from our own bridge (tag with serial / track last-set value) to avoid copy→OFFER→set→OFFER loops in both directions.
6. **Large/streaming transfers.** Validate a multi-hundred-MB transfer over vsock with chunking respects credit flow control without stalling the device threads; measure throughput. Decide on a max-size cap / temp-file staging on each side.
7. **Promised/lazy data on macOS.** Verify `NSPasteboard` promised-data provider (`provideDataForType:`) can block long enough to round-trip a guest `REQUEST`/`DATA` without AppKit timing out the paste; have a fallback to eager fetch for small text.
8. **Security policy.** Decide default: should guest→host paste be auto-trusted? Paste-injection risk is mainly the reverse of clipboard (a malicious guest can't inject keystrokes via clipboard, but can plant deceptive clipboard content). Consider a size cap, a "clipboard sharing" toggle in limina UI, and not auto-sharing primary selection. Decide which side may initiate (both, but receiver always pulls bytes lazily — never push unsolicited large payloads).
9. **Guest agent privilege.** data-control clients typically need to connect to the user session's Wayland socket; run `limina-agent` as a **user** systemd service (not system), matching the logged-in graphical session.

---

## 6. References

Local source (libkrun, `~/Projects/limina/third_party/libkrun`):
- `include/libkrun.h:986` `krun_add_vsock_port`; `:1000` `krun_add_vsock_port2`; `:998` `listen` semantics doc; `:1051` `krun_set_console_output`; `:1361` `krun_add_virtio_console_multiport`; `:1400` `krun_add_console_port_inout`. (No `krun_add_vsock_port3`/`flags` exists in this version.)
- `src/libkrun/src/lib.rs:178` `unix_ipc_port_map`; `:329` `add_vsock_port`; `:1467` `krun_add_vsock_port`; `:1477` `krun_add_vsock_port2`; `:2945`/`:2971` map → `VsockDeviceConfig`.
- `src/devices/src/virtio/vsock/` — `muxer.rs:99` `VsockMuxer`, `:135` `activate`, `:549-553` listen/RST check, `:563` `UnixProxy::new`; `unix.rs` (AF_UNIX bridge); `proxy.rs` (`Proxy` trait); `packet.rs` (credit flow control).
- `tests/guest-agent/src/main.rs` — minimal guest vsock-client agent example; reuse as the template for `limina-agent`'s vsock setup.
- Search confirming no SPICE/vdagent in libkrun: `grep -rli 'spice\|vdagent' src` → empty.

External (verify during spike; not fetched this session):
- virtio-vsock spec — credit-based flow control, OP_RW/OP_CREDIT_UPDATE.
- `wlr-data-control-unstable-v1`, `ext-data-control-v1` Wayland protocols (wayland-protocols).
- `wl-clipboard` (`wl-copy`/`wl-paste`) — uses data-control.
- spice-vdagent / spice VD_AGENT clipboard messages (`VD_AGENT_CLIPBOARD*`) — for Option B reference only.
- Apple `NSPasteboard` / UTType documentation; `changeCount` polling pattern.
