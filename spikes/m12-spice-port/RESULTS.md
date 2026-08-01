# M12 spike #1 — does a named `com.redhat.spice.0` port wake a stock guest's spice-vdagent?

**Date:** 2026-07-31 · **Verdict: GREEN on the gating question, with one blocking bug found.**

Vehicle: `LIMINA_SPICE_PORT=1` + `crates/limina-vmm/src/krun/console.rs::attach_spice_probe_port`
(env-gated; off in every normal boot). Guest: a fresh APFS clone of the **unmodified**
`Fedora-Workstation-43.accessible.raw` (`spice-vdagent-0.23.0-1.fc43.aarch64`), booted the default
way — `LIMINA_DISK=… spikes/venus-draw-probe/boot-enhanced-efi-kk.sh` (EFI → guest's own GRUB/kernel,
enforcing, windowed, `--net`). **Zero guest components installed.**

## What was answered

### 1. libkrun needs NO new device — the roadmap's task 1 premise was stale ✅

`docs/roadmap.md` M12 task 1 said a SPICE-style named multiport virtserial "is a **new libkrun
device/patch**". It is not. The named-multiport plumbing already exists in vendored libkrun:

- `PortConfig::InOut { name, input_fd, output_fd }` — `third_party/libkrun/src/vmm/src/resources.rs:124`
- consumed by `create_explicit_ports` — `third_party/libkrun/src/vmm/src/builder.rs:2529`
  (note: `input_to_raw_fd_dup` **dups**, so passing one socketpair fd as both in and out is safe)
- the name reaches the guest: on `VIRTIO_CONSOLE_PORT_READY` the device emits
  `VIRTIO_CONSOLE_PORT_NAME` — `devices/src/virtio/console/device.rs:224-228` → `console_control.rs:111`
- C API equivalents: `krun_add_virtio_console_multiport` / `krun_add_console_port_inout`
  (`include/libkrun.h:1361,1400`)

Adding the port is ~40 lines of limina code against the internal Rust API.

### 2. The stock udev → vdagentd chain fires, with zero guest changes ✅

```
/dev/virtio-ports/com.redhat.spice.0 -> ../vport5p0
/sys/class/virtio-ports/vport5p0/name: com.redhat.spice.0

udevadm: E: SYSTEMD_WANTS=spice-vdagentd.socket
         E: DEVLINKS=/dev/virtio-ports/com.redhat.spice.0
spice-vdagentd.socket: active (listening)
```

The stock rule `/usr/lib/udev/rules.d/70-spice-vdagentd.rules` matched exactly as predicted.

**Nuance: a graphical session is required.** `spice-vdagentd.service` is socket-activated and only
starts when the *session* agent connects. Headless (`--net`, no window), `seat0` is `CanGraphical=no`,
vdagentd logs `Error getting active session: No data available` and **exits**. Booted windowed, the
autologin GNOME session autostarts `/usr/bin/spice-vdagent`, which starts the daemon:

```
spice-vdagentd[1711]: opening vdagent virtio channel
/proc/<vdagentd>/fd/11 -> /dev/vport5p0          # our port, open
```

### 3. The protocol round-trips — the stock agent answered us ✅

`vdagentd` speaks only in reply, so the probe plays the SPICE server's role and sends
`VD_AGENT_ANNOUNCE_CAPABILITIES` (`request=1`). The guest answered with its own:

```
guest → host, 36 bytes:
  01 00 00 00  1c 00 00 00   VDIChunkHeader{port=VDP_CLIENT_PORT, size=28}
  01 00 00 00  06 00 00 00   VDAgentMessage{protocol=1, type=6 = ANNOUNCE_CAPABILITIES}
  00 00 00 00 00 00 00 00    opaque
  08 00 00 00                size=8
  01 00 00 00                request=1
  e7 8d 03 00                caps = 0x00038de7
```

Decoded caps the stock agent offers: `MOUSE_STATE`, `MONITORS_CONFIG`, `REPLY`,
**`CLIPBOARD_BY_DEMAND`**, **`CLIPBOARD_SELECTION`**, `SPARSE_MONITORS_CONFIG`, `GUEST_LINEEND_LF`,
`MAX_CLIPBOARD`, `AUDIO_VOLUME_SYNC`, `GRAPHICS_DEVICE_INFO`, `CLIPBOARD_NO_RELEASE_ON_REGRAB`,
`CLIPBOARD_GRAB_SERIAL`.

**Legacy `VD_AGENT_CAP_CLIPBOARD` (bit 3) is NOT offered** — the broker must speak the
by-demand + selection protocol, not the legacy one.

### 4. The guest clipboard reaches us — the Wayland risk is CLOSED ✅

With the port-reopen crash fixed (below) and the probe's announce made protocol-faithful, a
copy in the guest produced a real grab on the host:

```
guest → host, 36 bytes:
  01 00 00 00  1c 00 00 00   VDIChunkHeader{port=VDP_CLIENT_PORT, size=28}
  01 00 00 00  07 00 00 00   VDAgentMessage{protocol=1, type=7 = VD_AGENT_CLIPBOARD_GRAB}
  00 00 00 00 00 00 00 00    opaque
  08 00 00 00                size=8
  00 00 00 00                selection = 0 (VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD)
  01 00 00 00                types[0]  = 1 (VD_AGENT_CLIPBOARD_UTF8_TEXT)
```

So stock `spice-vdagent` on a **GNOME/Wayland** session does forward guest clipboard grabs to
us, through XWayland + mutter's X11↔Wayland selection bridging. Matches Boxes shipping this
on Wayland today.

**Two confounders had to be killed to get a trustworthy answer — both produced false
negatives first:**

1. **`wl-copy` over ssh does not set the clipboard.** It runs and stays resident (so it
   *looks* like it worked), but an unfocused Wayland client has no input-event serial to
   pass to `set_selection`, so mutter ignores it — `wl-paste` comes back empty. Verifying
   the copy actually took is what exposed this; the differential that worked was **`xclip`
   on `DISPLAY=:0`**, which goes through mutter's X11 selection path.
2. **A repeating announce timer suppresses clipboard traffic.** The probe originally
   re-announced every 3 s; `vdagentd -d -d` showed that as `sent client disconnected` /
   `New client connected` on every tick — each announce reads as a *new SPICE client*,
   resetting clipboard state continuously. Fixed by announcing until the agent first speaks,
   then answering its `request=1` greeting on demand — which is what a real broker does. **A
   broker must not re-announce on a timer.**

## Blocking bug found: reopening any virtio-console port panics the VMM 🔴 (FIXED)

A **guest-triggerable VMM abort**, pre-existing in libkrun and unrelated to SPICE:

```
thread 'main' panicked at third_party/libkrun/src/devices/src/virtio/console/device.rs:263:18:
port rx queue should exist
→ worker exits 101, the whole VM dies
```

**Mechanism** (`device.rs:230-276`): on `VIRTIO_CONSOLE_PORT_OPEN` the device does
`self.queues[rx_idx].take().expect("port rx queue should exist")`, moving the queues into the port.
The guest-close branch (`opened == false`) only logs and `continue`s — it never returns them. So the
**second** open of a port unwraps a `None` and aborts.

**Minimal repro** (nothing spice-specific — vdagentd had opened the port once at boot):

```bash
sudo systemctl stop spice-vdagentd.socket spice-vdagentd.service   # release the port
sudo dd if=/dev/vport5p0 of=/dev/null bs=1 count=1 iflag=nonblock  # second open → VM dies
```

**Why it blocks M12:** `systemctl restart spice-vdagentd`, a `spice-vdagent` package update, or the
daemon's own `Restart=on-failure` would each kill the guest. It also affects the *existing* ports
(`krun-stdin`/`krun-stdout`/`krun-stderr`, `hvc0`) — any reopen, so this is worth fixing and
upstreaming regardless of M12.

**FIXED — libkrun patch 0125**, RED-first: `crates/limina-test/tests/l1_port_reopen.rs` +
`limina.port_reopen` in the L1 init reproduced it in ~1 s (first open OK, second killed the VM), and
goes green with the fix. The io threads now take their queue by `&mut` and return it when they stop,
so `Port::shutdown()` hands both queues back and the device restores them; a port with no input (or
output) parks its unowned queue rather than dropping it. The guest-driven `expect()` is gone too —
a missing queue now logs and declines to start that port instead of aborting.

Re-validated on the real scenario: two `systemctl restart spice-vdagentd` in a row (each fatal
before) leave the VM healthy, and the agent still round-trips announces afterwards — so the port is
functionally reusable, not merely non-fatal.

## Notes for the broker (task 2)

- **Announce once per port open, never on a timer** — see confounder 2 above.
- Speak **by-demand + selection**, not legacy `CLIPBOARD` (the agent doesn't offer bit 3).
- Answer the agent's `ANNOUNCE_CAPABILITIES(request=1)` greeting: that is how a *restarted*
  `vdagentd` re-learns a client is connected.
- Chunk everything: `VD_AGENT_MAX_DATA_SIZE` is 2048, so even modest clipboard text arrives
  in pieces and must be reassembled.
- Feed the existing `crates/limina/src/clipboard.rs` owner — one pasteboard owner, two transports.

## Cost implication

M12's estimate should move: task 1 (the "gating unknown, one real libkrun patch") is **done** — the
device already existed, and what it actually needed was a small, independently-valuable libkrun fix
(0125, now landed). Both named risks are retired: the port wakes stock vdagent, and the guest
clipboard reaches the host. The remaining cost is the host-side vdagent broker (task 2), unchanged.
