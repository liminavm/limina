# 07 — Networking: NAT and Bridged

Scope: how limina gives its Linux guests network connectivity on a macOS / Apple-Silicon host through libkrun. Covers libkrun's two networking architectures — **TSI** (transparent socket impersonation, the default) and **virtio-net** with three host transports (`unixstream`, `unixgram`, `tap`) — their exact C API and Rust implementation, the macOS host-side glue (gvproxy, passt, Apple `vmnet.framework` via socket_vmnet / vmnet-helper), and how to deliver both NAT and true bridged networking.

All libkrun/krunkit facts below were read from this checkout; cited line numbers are accurate as of the `main` branch present locally (libkrun ~v1.18).

---

## 1. What exists today

libkrun offers **two** networking architectures:

1. **TSI — Transparent Socket Impersonation** (default; no real L2; app/socket level over virtio-vsock).
2. **virtio-net** — a real virtual NIC the guest drives with the stock Linux `virtio_net` driver, with three selectable host transports: `unixstream`, `unixgram`, `tap`.

Default selection logic (`src/libkrun/src/lib.rs:2954`): TSI is enabled only when
**no** virtio-net device was added (`vmr.net.list.is_empty()`) **and** no legacy
net config was set. Adding any `krun_add_net_*` device disables TSI. The header
documents the same (`include/libkrun.h:388-389`, `:430-431`, `:483`).

### 1.1 C API — verified against `include/libkrun.h` and `lib.rs`

| Function | Header | Impl | Notes |
|---|---|---|---|
| `krun_add_net_unixstream(ctx, c_path, fd, c_mac, features, flags)` | `libkrun.h:414` | `lib.rs:985` | virtio-net over **SOCK_STREAM**. Backends: **passt**, **socket_vmnet**. `c_path` XOR `fd` (`EINVAL` if both/neither, `lib.rs:1002-1007`). `c_mac` is **required** (6 bytes; `EINVAL` otherwise, `:1014`). Only `NET_FLAG_DHCP_CLIENT` accepted here — passing `NET_FLAG_VFKIT` returns `EINVAL` (`:1019`). |
| `krun_add_net_unixgram(ctx, c_path, fd, c_mac, features, flags)` | `libkrun.h:458` | `lib.rs:1044` | virtio-net over **SOCK_DGRAM**. Backends: **gvproxy**, **vmnet-helper**. Accepts `NET_FLAG_VFKIT | NET_FLAG_DHCP_CLIENT` (`NET_FLAG_ALL`, `:1077`). `VFKIT` → send 4-byte magic on connect. |
| `krun_add_net_tap(ctx, c_tap_name, c_mac, features, flags)` | `libkrun.h:489` | `lib.rs:1105` (linux) / `:1157` (stub) | virtio-net over a host TAP. **Linux host only** — the non-Linux build is a stub returning an error; the `Tap` backend itself is `#[cfg(target_os="linux")]` (`device.rs:68`). No `/dev/net/tun` on macOS. Extra rule: if any GUEST_TSO*/UFO feature is set, GUEST_CSUM must also be set (`:1129`). |
| `krun_set_port_map(ctx, port_map[])` | `libkrun.h:571` | `lib.rs:1238` | TSI port forwarding (`"host_port:guest_port"`). **Fails (`EINVAL`) if a virtio-net device was already added** (`set_port_map` errors when `net_index != 0`, `lib.rs:310-313`). Header also documents `-ENOTSUP` for the legacy passt path (`:556`). |
| `krun_set_passt_fd(ctx, fd)` | `libkrun.h:512` | `lib.rs:1170` | **DEPRECATED** → `krun_add_net_unixstream`. Stores `LegacyNetworkConfig::VirtioNetPasst(fd)`. |
| `krun_set_gvproxy_path(ctx, c_path)` | `libkrun.h:532` | `lib.rs:1192` | **DEPRECATED** → `krun_add_net_unixgram`. Stores `LegacyNetworkConfig::VirtioNetGvproxy(path)`; at start becomes `UnixgramPath(path, vfkit=true)` (`:2926-2928`). |
| `krun_set_net_mac(ctx, c_mac)` | `libkrun.h:544` | `lib.rs:1220` | Sets MAC for the **legacy** path only (`set_net_mac` → `legacy_mac`, `:306`). Default legacy MAC if unset: `5a:94:ef:e4:0c:ee` (`:2933`). New `krun_add_net_*` take `c_mac` directly. |
| `krun_add_vsock(ctx, tsi_features)` | `libkrun.h:1023` | — | Explicit vsock + TSI bitmask; requires `krun_disable_implicit_vsock` first. |
| `krun_disable_implicit_vsock(ctx)` | `libkrun.h:1277` | — | Disables the auto-created vsock device (which carries TSI). |
| `krun_has_feature(KRUN_FEATURE_NET)` | `libkrun.h:1110,1134` | — | `KRUN_FEATURE_NET == 0`. The whole net API is gated by Cargo feature `net` (`#[cfg(feature="net")]` on every fn above). |

Exact new-API prototypes (verbatim, header):

```c
int32_t krun_add_net_unixstream(uint32_t ctx_id, const char *c_path, int fd,
                                uint8_t *const c_mac, uint32_t features, uint32_t flags); // :414
int32_t krun_add_net_unixgram(uint32_t ctx_id, const char *c_path, int fd,
                              uint8_t *const c_mac, uint32_t features, uint32_t flags);   // :458
int32_t krun_add_net_tap(uint32_t ctx_id, char *c_tap_name,
                         uint8_t *const c_mac, uint32_t features, uint32_t flags);        // :489
int32_t krun_set_port_map(uint32_t ctx_id, const char *const port_map[]);                // :571
```

Multiple `krun_add_net_*` calls are allowed; interfaces appear in add order as
`eth0`, `eth1`, … (`create_virtio_net` names `eth{net_index}`, `lib.rs:2110`).

### 1.2 Flags and feature bits — verified, header `:355-377`, impl `lib.rs:936-980`

`NET_FLAG_*` (the `flags` argument):

| Flag | Value | Meaning |
|---|---|---|
| `NET_FLAG_VFKIT` | `1 << 0` | Send the 4-byte **`"VFKT"`** magic right after connecting (gvproxy vfkit mode). Implemented in `unixgram.rs:15,90-93`. **unixgram only.** |
| `NET_FLAG_DHCP_CLIENT` | `1 << 1` | Run libkrun's built-in **guest-side DHCP client** for the interface. Sets `vmr.dhcp_client = true` → kernel cmdline `KRUN_DHCP=1` (`builder.rs:1084-1085`), which triggers the C DHCP client compiled into the guest init (`init/dhcp.c`). Accepted by all three `krun_add_net_*`. |

`KRUN_TSI_HIJACK_*` (`krun_add_vsock` bitmask), header `:360-362`:

| Flag | Value | Meaning |
|---|---|---|
| `KRUN_TSI_HIJACK_INET` | `1 << 0` | Intercept AF_INET socket syscalls; tunnel TCP/UDP over vsock. **This is the only flag the implicit/default TSI path enables** (`lib.rs:2962` → `TsiFlags::HIJACK_INET`). |
| `KRUN_TSI_HIJACK_UNIX` | `1 << 1` | Intercept AF_UNIX connections (host↔guest UNIX-socket bridging). Off by default. |

`NET_FEATURE_*` (the `features` arg; "Taken from uapi/linux/virtio_net.h"), header `:364-372`:

| Feature | Value |
|---|---|
| `NET_FEATURE_CSUM` | `1 << 0` |
| `NET_FEATURE_GUEST_CSUM` | `1 << 1` |
| `NET_FEATURE_GUEST_TSO4` | `1 << 7` |
| `NET_FEATURE_GUEST_TSO6` | `1 << 8` |
| `NET_FEATURE_GUEST_UFO` | `1 << 10` |
| `NET_FEATURE_HOST_TSO4` | `1 << 11` |
| `NET_FEATURE_HOST_TSO6` | `1 << 12` |
| `NET_FEATURE_HOST_UFO` | `1 << 14` |

Two convenience masks exist in `lib.rs`:
- `NET_COMPAT_FEATURES` (`lib.rs:966`) = CSUM + GUEST_CSUM + GUEST_TSO4 + GUEST_UFO + HOST_TSO4 + HOST_UFO. **IPv4 only** (no TSO6). This is exactly what the legacy `set_passt_fd`/`set_gvproxy_path` paths use (`:2934`). The header exposes the same as `COMPAT_NET_FEATURES` (`:374-377`).
- `NET_ALL_FEATURES` (`lib.rs:973`) = the COMPAT set **plus** GUEST_TSO6 + HOST_TSO6. `krun_add_net_*` reject any `features` bit outside `NET_ALL_FEATURES` (`EINVAL`, `:1024`,`:1073`,`:1125`). So **TSO6 is only reachable via the new APIs**, never via the legacy ones.

The device unconditionally also advertises `VIRTIO_NET_F_MAC`, `VIRTIO_RING_F_EVENT_IDX`, and `VIRTIO_F_VERSION_1` (`device.rs:92-95`).

### 1.3 TSI internals — `src/devices/src/virtio/vsock/`

TSI is implemented inside the vsock device:

- `device.rs`, `mod.rs`, `event_handler.rs` — virtio-vsock device + virtqueue handling.
- `muxer.rs` (25 KB), `muxer_rxq.rs`, `muxer_thread.rs` — connection multiplexer on its own thread.
- `tsi_stream.rs` (30 KB) — TCP impersonation; `tsi_dgram.rs` (15 KB) — UDP impersonation. These open real host sockets and bridge them to guest-side intercepted sockets over vsock.
- `proxy.rs`, `reaper.rs` (connection lifecycle), `timesync.rs`, `unix.rs`, `packet.rs`.

A guest-side TSI shim intercepts socket syscalls; payloads ride virtio-vsock to
the host muxer, which opens host sockets. **No Ethernet frame, no ARP/ICMP, no
guest IP-stack participation** for forwarded sockets. `krun_set_port_map`
programs the host↔guest port-forwarding table (`host_port_map` in
`VsockDeviceConfig`, `lib.rs:2944`). With `NULL` port_map libkrun tries to expose
**all** guest listening ports to the host; an empty array exposes none
(`header:559-562`). Exposed ports are reachable in the guest by their
**host_port** number too (`:564-566`).

### 1.4 virtio-net internals — `src/devices/src/virtio/net/` (1300 lines total)

| File | Lines | Role |
|---|---|---|
| `mod.rs` | 38 | constants: `MAX_BUFFER_SIZE = 65562`, `QUEUE_SIZE = 1024`, `NUM_QUEUES = 2` (rx,tx). `VNET_HDR_LEN = sizeof(virtio_net_hdr_v1)`; `write_virtio_net_hdr` zero-fills the header. |
| `device.rs` | 210 | `Net` virtio device: config space (MAC/status/max_vq_pairs), feature negotiation, `activate()` spawns one `NetWorker`. `VirtioNetBackend` enum = `{UnixstreamFd, UnixstreamPath, UnixgramFd, UnixgramPath(path,vfkit_bool), Tap(linux)}` (`:63-70`). |
| `backend.rs` | 54 | `NetBackend` trait: `read_frame`/`write_frame`/`has_unfinished_write`/`try_finish_write`/`raw_socket_fd`/`write_retry_delay_us`. `ConnectError`/`ReadError`/`WriteError` enums. |
| `unixstream.rs` | 222 | SOCK_STREAM transport (passt/socket_vmnet). **4-byte big-endian length prefix** per frame (`FRAME_HEADER_LEN=4`, `:17,191`). 16 MiB SO_SNDBUF. Supports **partial writes** (PartialWrite/try_finish_write). macOS uses `MSG_DONTWAIT`/`MSG_WAITALL` (no `MSG_NOSIGNAL`). |
| `unixgram.rs` | 173 | SOCK_DGRAM transport (gvproxy/vmnet-helper). One datagram per frame, no length prefix. Sends `"VFKT"` magic if `vfkit` set (`:90`). Binds a local `"{path}-krun.sock"`, connects to peer. macOS: `SO_NOSIGPIPE`, `SndBuf = MAX_BUFFER_SIZE - VNET_HDR_LEN` (datagram size limit, not queue), ENOBUFS→retry after 50 µs (`:170`). |
| `tap.rs` | 128 | Linux `/dev/net/tun`, `IFF_TAP|IFF_NO_PI|IFF_VNET_HDR`, vnet hdr size 12, maps GUEST_CSUM/TSO4/TSO6/UFO → TUN_F_* offloads. (Not built on macOS.) |
| `worker.rs` | 475 | One **`"virtio-net worker"` thread** per device. epoll over: rx queue eventfd, tx queue eventfd, backend socket (IN\|OUT\|EDGE_TRIGGERED\|READ_HANG_UP). Copies frames between virtqueues and the transport with a single `MAX_BUFFER_SIZE` rx/tx buffer each. On backend HANG_UP it logs `LIBKRUN VIRTIO-NET FATAL: Backend process seems to have quit ... Networking is now disabled!` and **does not reconnect** (`:146-147`). On macOS it uses a one-shot timer (`write_retry_delay_us`) to retry deferred TX. |

The guest uses the in-tree Linux `virtio_net` driver — **no custom guest driver
needed** for any virtio-net transport (unlike TSI, which needs the guest hijack
shim).

### 1.5 DHCP client (guest-side) — `init/dhcp.c`

When `NET_FLAG_DHCP_CLIENT` is set, libkrun adds `KRUN_DHCP=1` to the kernel
cmdline (`builder.rs:1084`). The guest init then runs a **standalone C DHCP
client** (`init/dhcp.c`, "Translated from muvm/src/guest/net.rs") that
DISCOVER/OFFER/REQUEST/ACKs over a raw socket and configures the interface via
netlink. Use this when the L2 backend does **not** run its own DHCP server (e.g.
a raw vmnet datagram path). gvproxy/passt run their own DHCP, so DHCP_CLIENT is
typically unnecessary with them.

### 1.6 Build features

The entire net API is `#[cfg(feature = "net")]`; tap is additionally
`#[cfg(target_os = "linux")]`. Confirm the Homebrew libkrun has `net` enabled at
runtime via `krun_has_feature(KRUN_FEATURE_NET)`.

### 1.7 macOS host-side glue

| Tool | Installed (per brief) | Role | Mode | libkrun attach |
|---|---|---|---|---|
| **gvproxy** (gvisor-tap-vsock) | YES (on PATH) | Userspace L3 gateway: NAT + DHCP + DNS + REST port-forward; vfkit dgram framing | **NAT** | `krun_add_net_unixgram(path, -1, mac, feats, NET_FLAG_VFKIT)` |
| **passt / pasta** | qemu dep; **verify `which passt`** | Userspace TCP/UDP/ICMP NAT, no root, SOCK_STREAM | **NAT** | `krun_add_net_unixstream(path,…)` |
| **socket_vmnet** | **UNCONFIRMED** — brief lists `vde`, not socket_vmnet; `brew list socket_vmnet` | Root daemon owning the `vmnet` handle; SOCK_STREAM to clients | **NAT (vmnet shared)** / **bridged** | `krun_add_net_unixstream` |
| **vmnet-helper** | **UNCONFIRMED** — not in brief | Minimal privileged helper holding the `vmnet` handle; SOCK_DGRAM/vfkit framing | **NAT / bridged** | `krun_add_net_unixgram` |
| **vde** (`vde_vmnet`, `vde_switch`) | YES | VDE switch + optional `vde_vmnet` bridge to `vmnet.framework` | **NAT / bridged** (older path) | unix-socket transport |
| **krunkit** | YES | vfkit-REST front-end. Reference net wiring: parses `--device virtio-net,unixSocketPath=…,mac=…` and currently calls the **legacy** `krun_set_gvproxy_path` + `krun_set_net_mac` (`virtio.rs:240,247`). | (consumer) | — |

#### Apple `vmnet.framework` modes (only native L2 on macOS)

- `VMNET_HOST_MODE` — host-only (guest ↔ host, no internet).
- `VMNET_SHARED_MODE` — **NAT** with Apple's built-in DHCP (192.168.x.x), outbound internet (the "Internet Sharing" substrate).
- `VMNET_BRIDGED_MODE` — **bridge** the VM onto a chosen physical interface (e.g. `en0`); the guest gets a LAN DHCP lease and is a first-class LAN peer. **This is what "bridged" means for limina.**

Constraints:
- Bridged/advanced vmnet needs the **`com.apple.vm.networking`** entitlement,
  which is **Apple-managed** (special provisioning profile; not freely
  self-grantable for distribution). `VMNET_SHARED_MODE` is more permissive but a
  non-Virtualization.framework process driving vmnet directly still needs
  entitlements and often **root**.
- libkrun does **not** link `vmnet.framework`. vmnet is always reached through a
  **helper process** (socket_vmnet / vmnet-helper / vde_vmnet) that holds the
  handle and exposes a UNIX socket; libkrun attaches via `krun_add_net_unixstream`
  (socket_vmnet) or `krun_add_net_unixgram` (vmnet-helper, vfkit framing).

---

## 2. How it works end to end

### 2.1 TSI (default)

```
guest app socket()/connect()  ->  guest TSI hijack shim (HIJACK_INET)
   -> virtio-vsock TX vq -> vsock muxer thread (muxer.rs) -> tsi_stream/tsi_dgram
   -> real host socket() -> host kernel -> network
   <- replies traverse the reverse path, demuxed by connection id
```

- No Ethernet, no ARP, no ICMP/ping, no guest IP stack for forwarded sockets.
- Inbound: `krun_set_port_map("8080:80")` → libkrun listens on host :8080 and
  routes accepted connections into the guest; in-guest the service is reached on
  the **host_port** (8080). Must be set **before** any virtio-net device.

### 2.2 virtio-net + userspace NAT (gvproxy / passt)

```
guest virtio_net driver -> TX vq (Ethernet frames, prefixed with virtio_net_hdr)
   -> "virtio-net worker" thread (worker.rs) -> transport
      -> unixgram: 1 datagram/frame (+ "VFKT" handshake) -> gvproxy
         unixstream: 4-byte BE length + frame             -> passt
      -> userspace L3 stack: NAT + DHCP + DNS, opens real host sockets
   -> host kernel -> internet
   <- inbound frames pushed back -> RX vq -> guest
```

- Guest runs a normal kernel IP stack on `eth0`; gvproxy/passt answer DHCP/DNS
  internally, so the guest's dhclient/networkd just works (no `NET_FLAG_DHCP_CLIENT`
  needed).
- Port-forward (NAT inbound) is configured on the **gateway**, not in libkrun:
  gvproxy's REST `/services/forwarder/expose`, or passt `-t/-u` maps.
  `krun_set_port_map` is TSI-only and will `EINVAL` once a net device exists.
- **Failure mode:** if the gateway process dies, the worker sees HANG_UP and
  permanently disables that NIC (no auto-reconnect). limina must supervise/restart
  the gateway and recreate the VM net path as needed.

### 2.3 virtio-net + vmnet (bridged or shared)

```
guest virtio_net -> TX vq -> net worker -> UNIX socket -> vmnet helper (root)
   -> vmnet.framework (SHARED=NAT or BRIDGED=L2 on en0) -> physical NIC
```

- In BRIDGED mode the guest's frames go straight onto the chosen physical
  segment; the guest MAC is visible on the LAN and it pulls a LAN DHCP lease.
- The helper (root, entitled) owns the vmnet handle; libkrun (unprivileged) only
  sees the UNIX socket — the privilege separation macOS forces.

---

## 3. Options inventory for limina

### A — TSI only (do-nothing / upstream default)
- **Pros:** zero host daemons, zero entitlements, lowest overhead, smallest
  attack surface, works the moment `net` is built in. Ideal for first boot.
- **Cons:** not real networking. No ping/ICMP, no raw sockets, no
  broadcast/multicast/mDNS-Bonjour, VPN clients break, guest never visible on LAN,
  bridged impossible. Default hijack is INET-only.
- **Verdict:** bring-up default only.

### B — virtio-net + gvproxy (NAT)  ← recommended NAT
- **Pros:** gvproxy already installed; pure userspace, no root, no entitlements;
  full guest IP stack; built-in DHCP/DNS; REST port-forward; proven (podman
  machine, krunkit). One call: `krun_add_net_unixgram(path,-1,mac,feats,NET_FLAG_VFKIT)`.
- **Cons:** userspace TCP stack ⇒ throughput/latency cost; NAT only; we supervise
  gvproxy + socket lifecycle + forward rules; no worker auto-reconnect on crash.
- **Verdict:** strong default for NAT. (Prefer the new `unixgram` API over the
  legacy `set_gvproxy_path` krunkit still uses, so we can pass MAC + TSO6 directly.)

### C — virtio-net + passt (NAT)
- **Pros:** no root/entitlements, small, good perf, IPv6; `krun_add_net_unixstream`
  with proper 4-byte length framing already in libkrun.
- **Cons:** less common on macOS than gvproxy; confirm a working arm64 brew build;
  NAT only; `krun_set_port_map` unsupported.
- **Verdict:** viable NAT fallback; pick gvproxy first (confirmed installed).

### D — virtio-net + vmnet SHARED (NAT via Apple)
- **Pros:** Apple-native NAT, kernel-fast, Apple DHCP/NAT; reuses bridged helper.
- **Cons:** privileged helper (socket_vmnet/vmnet-helper — install status
  unconfirmed) + entitlements + root/setuid. Heavier than gvproxy for plain NAT.
- **Verdict:** only if we already ship vmnet for bridged.

### E — virtio-net + vmnet BRIDGED (bridged)  ← required for the bridged feature
- **Pros:** the only true bridged path on macOS; guest is a first-class LAN peer
  (LAN DHCP, ping, discovery) — matches Parallels bridged.
- **Cons:** **`com.apple.vm.networking`** (Apple-managed entitlement) + root/setuid
  helper holding the vmnet handle; signing/distribution friction; Wi-Fi (`en0`)
  bridging has known macOS MAC limitations.
- **Verdict:** mandatory for bridged; gate behind helper + entitlement; later
  milestone.

### F — virtio-net + tap
- **Cons:** **no TAP on macOS** — the backend is `#[cfg(linux)]` and the macOS
  `krun_add_net_tap` is a stub.
- **Verdict:** N/A.

---

## 4. Recommendation

1. **Milestone 1 (boot the Fedora image):** use **TSI (A)** — zero glue, gets
   DNF/SSH working immediately; add `krun_set_port_map` for any test ports
   *before* any net device. First confirm `krun_has_feature(KRUN_FEATURE_NET)`.
2. **Product default — NAT:** ship **virtio-net + gvproxy (B)** via the **new**
   API: `krun_add_net_unixgram(c_path, -1, mac, features, NET_FLAG_VFKIT)`. Spawn
   and supervise gvproxy; allocate a stable per-VM MAC (locally-administered);
   map a limina port-forward UI to gvproxy's REST. Start with `NET_COMPAT_FEATURES`
   offloads; evaluate adding `NET_FEATURE_GUEST_TSO6 | NET_FEATURE_HOST_TSO6`
   (reachable only via the new API) once verified non-corrupting with gvproxy.
   Handle the no-auto-reconnect HANG_UP failure mode.
3. **Bridged (later) — vmnet bridged (E):** ship a privileged vmnet helper
   (vmnet-helper + `unixgram/vfkit`, or socket_vmnet + `unixstream`) plus the
   `com.apple.vm.networking` entitlement/signing work. Reuse the helper for vmnet
   SHARED (D) if we want Apple-native NAT later.

### What limina must build / ship (glue)
- **Network-backend manager**: spawn/monitor the host helper (gvproxy / passt /
  vmnet-helper), own the UNIX socket path + lifecycle, detect death and
  restart/recreate (libkrun does **not** reconnect), surface health to UI.
- **MAC allocation** (stable per-VM, locally-administered) passed as `c_mac`.
- **Port-forward control plane**: limina rules → gvproxy REST for NAT; remember
  `krun_set_port_map` is TSI-only and rejected once a net device exists.
- **DHCP/DNS wiring per backend**: gvproxy/passt internal; vmnet SHARED = Apple;
  vmnet bridged = LAN; raw datagram path = `NET_FLAG_DHCP_CLIENT` (guest `dhcp.c`).
- **For bridged**: the vmnet helper binary, its entitlement plist, a privileged
  install/authorization flow (SMAppService / SMJobBless or setuid-root), and a
  physical-interface picker.

### What likely needs patching in libkrun
- **NAT: nothing** — gvproxy/passt paths are complete and correct.
- **Bridged: probably no libkrun patch** if the helper speaks unixstream/unixgram;
  the entitlement + helper work is ours. Patch libkrun only if `NET_FEATURE_*`
  negotiation, MTU, or the single-buffer worker proves incompatible with vmnet,
  or if we need worker **reconnect-on-crash** (a small, self-contained patch to
  `worker.rs`'s HANG_UP handling — recommended regardless for robustness).

---

## 5. Open questions / things to prototype

1. **Confirm net is built in** the Homebrew libkrun (`krun_has_feature(KRUN_FEATURE_NET)`).
2. **gvproxy framing match:** does the installed gvproxy speak the `"VFKT"` dgram
   handshake `unixgram.rs` sends? Spike: boot guest, get DHCP lease, `curl` out.
3. **Which vmnet helper is installed?** `brew list socket_vmnet vmnet-helper`;
   does `vde` provide `vde_vmnet`? Determines unixstream vs unixgram transport.
4. **TSO6 offloads:** do GUEST_TSO6/HOST_TSO6 (new-API only) improve iperf3
   throughput without corruption, per backend (gvproxy/passt/vmnet)?
5. **`com.apple.vm.networking` reality:** can we obtain/ship it for a non-App-Store
   app, or must users install a separately-signed root helper? Prototype with a
   self-signed helper run as root.
6. **Wi-Fi bridging:** does vmnet BRIDGED over `en0` (Wi-Fi) work on this M1 Max,
   or only over wired/USB Ethernet?
7. **Worker robustness:** the single-thread/single-buffer worker disables the NIC
   permanently on backend HANG_UP. Decide whether limina restarts the whole VM net
   path or we patch `worker.rs` to reconnect. Also check the macOS datagram size
   limit (`SndBuf = MAX_BUFFER_SIZE - VNET_HDR_LEN`) vs jumbo frames / large GSO.
8. **TSI gaps inventory:** enumerate exactly what breaks under TSI (ICMP/ping,
   mDNS/Bonjour, VPNs, multicast) to set product expectations vs the gvproxy default.
9. **Dynamic memory interaction:** the worker holds two fixed `MAX_BUFFER_SIZE`
   (~64 KiB) buffers per device — negligible, no balloon conflict expected. Confirm.
10. **krunkit modernization:** krunkit still uses the legacy `set_gvproxy_path` +
    `set_net_mac`. limina should use the new `krun_add_net_unixgram` directly to get
    MAC + full feature control in one call.

---

## 6. Verification / re-check checklist

```sh
# C API + flags (verified; re-confirm anchors if header changes)
grep -nE 'krun_(set_port_map|add_net_unixstream|add_net_unixgram|add_net_tap|set_gvproxy_path|set_passt_fd|set_net_mac)' \
  ~/Projects/limina/third_party/libkrun/include/libkrun.h
grep -nE 'NET_(FLAG|FEATURE|COMPAT|ALL)|TSI_HIJACK|VFKIT|DHCP|create_virtio_net|enable_tsi' \
  ~/Projects/limina/third_party/libkrun/src/libkrun/src/lib.rs
# DHCP client path
grep -nE 'dhcp_client|KRUN_DHCP' ~/Projects/limina/third_party/libkrun/src/vmm/src/builder.rs
# transports
sed -n '15,20p;90,93p' ~/Projects/limina/third_party/libkrun/src/devices/src/virtio/net/unixgram.rs
sed -n '15,17p;191,195p'  ~/Projects/limina/third_party/libkrun/src/devices/src/virtio/net/unixstream.rs
# host helpers present?
which gvproxy passt pasta socket_vmnet vmnet-helper vde_vmnet
brew list socket_vmnet vmnet-helper 2>/dev/null
```

---

## 7. References

Local source (verified this session):
- `include/libkrun.h` — C API: net at `:355-571`, `:1023`, `:1110`, `:1277`.
- `src/libkrun/src/lib.rs` — `krun_add_net_unixstream` `:985`, `unixgram` `:1044`, `tap` `:1105/1157`, `set_passt_fd` `:1170`, `set_gvproxy_path` `:1192`, `set_net_mac` `:1220/306`, `set_port_map` `:1238/310`, flag/feature consts `:936-980`, `create_virtio_net` `:2103`, legacy/TSI start logic `:2922-2977`.
- `src/devices/src/virtio/net/` — `mod.rs`, `device.rs:63-95,173-205`, `backend.rs:40-54`, `unixstream.rs:15-17,90-196`, `unixgram.rs:15,69-109,169-172`, `tap.rs:28-73`, `worker.rs:92-185` (one thread, epoll, HANG_UP at `:146`).
- `src/devices/src/virtio/vsock/` — TSI: `muxer.rs`, `tsi_stream.rs`, `tsi_dgram.rs`, `proxy.rs`, `reaper.rs`.
- `src/vmm/src/builder.rs:1084` and `src/vmm/src/resources.rs:208` — `dhcp_client` → `KRUN_DHCP=1`; `init/dhcp.c` — guest DHCP client.
- `third_party/krunkit/src/virtio.rs:240,247` — reference net wiring (legacy gvproxy path + MAC).

External (verify online):
- containers/libkrun README + CHANGELOG (net API evolution).
- gvisor-tap-vsock (gvproxy): vfkit dgram framing + REST forwarder API.
- passt/pasta docs: SOCK_STREAM backend, forward maps.
- lima-vm/socket_vmnet and vmnet-helper: privilege model, framing.
- Apple `vmnet.framework`: `VMNET_{HOST,SHARED,BRIDGED}_MODE`, `com.apple.vm.networking`.
