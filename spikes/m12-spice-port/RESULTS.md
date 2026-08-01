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

## Blocking bug found: reopening any virtio-console port panics the VMM 🔴

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

**Fix shape:** return the queues to `self.queues[idx]` when the port stops (guest close), or don't
`take()` at all — keep the queues in the device and lend them to the port. Wants a RED test first
(open/close/open a named port; assert the VM survives), per the project's fix discipline.

## Still open — the Wayland clipboard risk got *sharper*, not resolved 🟡

F43's `spice-vdagent` 0.23.0 clipboard is **X11-only**: it links `libX11`, its clipboard code is
`src/vdagent/x11.c`, and strings show no `zwlr_data_control`, no `RemoteDesktop`. The only
Wayland-aware piece is `org.gnome.Mutter.DisplayConfig` — used for *resolutions* (the display path
M12 explicitly excludes).

In the session it does run on XWayland (`DISPLAY=:0`, `Xwayland :0 -rootless` present,
`loginctl Type=wayland Active=yes`), so clipboard would have to ride **mutter's Wayland↔X11 selection
bridging**. That is the same wall our own guest agent hit and solved with three tiers
(ext-data-control → extension bridge → RemoteDesktop; see `docs/images.md`).

A `wl-copy` in the guest produced **no** `VD_AGENT_CLIPBOARD_GRAB` on the host — but that run is
**not** a clean verdict: the attempt to enable agent debug logging is what tripped the reopen panic
above, so no instrumented run completed. **Re-test after the libkrun fix**, with
`spice-vdagentd -d -d` + `spice-vdagent -d` logging, before committing to the broker.

## Cost implication

M12's estimate should move: task 1 (the "gating unknown, one real libkrun patch") is mostly *done* —
replaced by a much smaller, independently-valuable libkrun fix. The remaining cost is the host-side
vdagent broker (task 2), which is unchanged.
