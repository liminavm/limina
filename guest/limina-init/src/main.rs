// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! limina-init — PID 1 for the L1 tiny test guest, and the seed of the future limina-agent.
//!
//! Flow: prove userspace (console marker), then — if the kernel cmdline carries
//! `limina.agent_port=<N>` — run the **control-plane agent** ([`agent`]): it connects to
//! the host (`CID_HOST:N`) and speaks the limina-proto framed protocol (HELLO/WELCOME,
//! HEARTBEAT, SHUTDOWN/SHUTDOWN_ACK) so the host harness makes STRUCTURED assertions
//! (not console scraping). Finally power the VM off cleanly via PSCI. Every path ends
//! at [`power_off`]; as PID 1 it must never return or panic.

use std::ffi::CStr;

mod agent;

/// Marker the host harness asserts on. Keep in sync with `crates/limina-test`.
const MARKER: &[u8] = b"\n[limina-init] LIMINA_L1_USERSPACE_OK\n";

fn main() {
    mount_pseudo_fs();
    announce();
    // Mount host shares first: agents and test modes below may depend on them.
    mount_limina_shares();
    if let Some(name) = cmdline_value("limina.sharecheck") {
        run_sharecheck(&name);
    }
    if let Some(name) = cmdline_value("limina.sharecheck_ro") {
        run_sharecheck_ro(&name);
    }
    // `limina.real_agent`: spawn the PRODUCT agent binary (staged at /limina-agent by
    // build-test-guest.sh) — the L1 vehicle for testing the real limina-agent end-to-end.
    // It connects to the control plane on its own and powers the guest off itself on
    // SHUTDOWN (via raw reboot(2) here; no systemd in this world). Spawned BEFORE the
    // blocking seed below so both can be connected concurrently — the L1 vehicle for
    // the multi-connection control plane (root agent + session helpers on real distros).
    if cmdline_has("limina.real_agent") {
        match std::process::Command::new("/limina-agent").spawn() {
            Ok(_) => klog(b"[limina-init] spawned /limina-agent"),
            Err(_) => klog(b"[limina-init] failed to spawn /limina-agent"),
        }
    }
    // `limina.dbus`: bring up a session D-Bus bus (the Alpine-extracted dbus-daemon the
    // build script stages) — infrastructure for tests that need real D-Bus services
    // (first user: the limina-agent-session clipboard test against limina-mock-mutter).
    // `limina.mock_mutter` and `limina.session_helper` then run on that bus; the mock's
    // script/observation files are named by `limina.mock_id=<id>`.
    if cmdline_has("limina.dbus") {
        spawn_dbus_stack();
    }
    if let Some(port) = agent_port_from_cmdline() {
        // A host-ordered SHUTDOWN powers off NOW — it must override `limina.hold` (the
        // whole point is that closing the window ends a held/animating guest).
        if agent::run(port) == agent::AgentEnd::Shutdown {
            power_off();
        }
    }
    // `limina.console_echo`: prove the serial console works both ways for the host harness —
    // echo each line back as `ECHO:<line>` until `QUIT`. The seed of typing commands at the
    // guest and reading their output. Runs to completion, then falls through to power-off.
    if cmdline_has("limina.console_echo") {
        run_console_echo();
    }
    // `limina.hold`: keep the guest alive animating the framebuffer (for the interactive
    // window). Otherwise draw the one-shot test pattern (the capture oracle) and power off.
    if cmdline_has("limina.hold") {
        animate_forever();
    }
    draw_test_pattern();
    power_off();
}

/// True if `needle` appears as a whitespace-separated token on the kernel cmdline.
fn cmdline_has(needle: &str) -> bool {
    std::fs::read_to_string("/proc/cmdline")
        .map(|c| c.split_whitespace().any(|t| t == needle))
        .unwrap_or(false)
}

/// The value of a `key=value` kernel cmdline token, if present.
fn cmdline_value(key: &str) -> Option<String> {
    let cmdline = std::fs::read_to_string("/proc/cmdline").ok()?;
    let prefix = format!("{key}=");
    cmdline
        .split_whitespace()
        .find_map(|t| t.strip_prefix(&prefix).map(str::to_string))
}

/// Address of the session bus `spawn_dbus_stack` brings up.
const DBUS_ADDR: &str = "unix:path=/tmp/limina-bus";

/// Bring up the test session-bus stack: dbus-daemon, then (per cmdline flags) the
/// mock-mutter compositor stand-in and the REAL limina-agent-session helper, all wired
/// via DBUS_SESSION_BUS_ADDRESS. Each child is best-effort: a missing binary klogs and
/// the test asserting on its effects fails loudly.
fn spawn_dbus_stack() {
    let _ = std::fs::create_dir_all("/tmp");
    match std::process::Command::new("/usr/bin/dbus-daemon")
        .args(["--session", "--nofork", "--nopidfile"])
        .arg(format!("--address={DBUS_ADDR}"))
        .spawn()
    {
        Ok(_) => klog(b"[limina-init] spawned dbus-daemon"),
        Err(_) => {
            klog(b"[limina-init] failed to spawn dbus-daemon");
            return;
        }
    }
    if cmdline_has("limina.mock_mutter") {
        let mock_id = cmdline_value("limina.mock_id").unwrap_or_else(|| "0".into());
        match std::process::Command::new("/limina-mock-mutter")
            .env("DBUS_SESSION_BUS_ADDRESS", DBUS_ADDR)
            .env("LIMINA_MOCK_ID", &mock_id)
            .spawn()
        {
            Ok(_) => klog(b"[limina-init] spawned /limina-mock-mutter"),
            Err(_) => klog(b"[limina-init] failed to spawn /limina-mock-mutter"),
        }
    }
    if cmdline_has("limina.session_helper") {
        match std::process::Command::new("/limina-agent-session")
            .env("DBUS_SESSION_BUS_ADDRESS", DBUS_ADDR)
            .spawn()
        {
            Ok(_) => klog(b"[limina-init] spawned /limina-agent-session"),
            Err(_) => klog(b"[limina-init] failed to spawn /limina-agent-session"),
        }
    }
}

/// Mount devtmpfs on /dev and procfs on /proc (best-effort). The kernel may not have
/// opened an initial console for us, so we need /dev/kmsg ourselves; /proc/cmdline tells
/// us whether to run the agent. sysfs feeds the virtiofs share-tag enumeration.
fn mount_pseudo_fs() {
    mount(c"devtmpfs", c"/dev", c"devtmpfs");
    mount(c"proc", c"/proc", c"proc");
    mount(c"sysfs", c"/sys", c"sysfs");
}

/// Auto-mount `limina-`-tagged virtiofs shares at `/media/<name>` — the product mount
/// convention (the real limina-agent does the same on a distro guest). Tags are
/// enumerated from sysfs (`/sys/fs/virtiofs/<id>/tag` — NOT the virtio-9p-style
/// `mount_tag` device attribute), not the cmdline, so the mechanism also works on EFI
/// boots where GRUB owns the cmdline. The rootfs share is tagged `/dev/root` and is
/// skipped by the prefix filter. Best-effort per share.
fn mount_limina_shares() {
    let Ok(devices) = std::fs::read_dir("/sys/fs/virtiofs") else {
        klog(b"[limina-init] no /sys/fs/virtiofs (no virtiofs devices?)");
        return;
    };
    for dev in devices.filter_map(|e| e.ok()) {
        let Ok(tag_raw) = std::fs::read(dev.path().join("tag")) else {
            continue;
        };
        let tag = String::from_utf8_lossy(&tag_raw)
            .trim_end_matches(['\0', '\n'])
            .to_string();
        let Some(name) = tag.strip_prefix("limina-") else {
            continue;
        };
        let target = format!("/media/{name}");
        let _ = std::fs::create_dir_all(&target);
        let (Ok(c_tag), Ok(c_target)) = (
            std::ffi::CString::new(tag.clone()),
            std::ffi::CString::new(target.clone()),
        ) else {
            continue;
        };
        let rc = unsafe {
            libc::mount(
                c_tag.as_ptr(),
                c_target.as_ptr(),
                c"virtiofs".as_ptr(),
                0,
                std::ptr::null(),
            )
        };
        if rc == 0 {
            klog(format!("[limina-init] mounted share {tag} at {target}").as_bytes());
        } else {
            klog(format!("[limina-init] failed to mount share {tag}").as_bytes());
        }
    }
}

/// `limina.sharecheck=<name>`: prove the `/media/<name>` share end-to-end for the L1
/// share test — read `ping` (staged by the host), write its content back to `pong`
/// with a guest suffix, and log a marker carrying the ping content.
fn run_sharecheck(name: &str) {
    let dir = format!("/media/{name}");
    match std::fs::read_to_string(format!("{dir}/ping")) {
        Ok(ping) => {
            let ok = std::fs::write(format!("{dir}/pong"), format!("{ping}+guest")).is_ok();
            if ok {
                klog(format!("[limina-init] LIMINA_SHARE_OK {ping}").as_bytes());
            } else {
                klog(b"[limina-init] LIMINA_SHARE_FAIL writing pong");
            }
        }
        Err(e) => {
            klog(format!("[limina-init] LIMINA_SHARE_FAIL reading ping: {e}").as_bytes());
        }
    }
}

/// `limina.sharecheck_ro=<name>`: prove a `:ro` share refuses writes — reads must work,
/// writes must FAIL (that failing is the assertion, hence the inverted marker).
fn run_sharecheck_ro(name: &str) {
    let dir = format!("/media/{name}");
    let readable = std::fs::read_to_string(format!("{dir}/ping")).is_ok();
    let write_failed = std::fs::write(format!("{dir}/intruder"), b"nope").is_err();
    if readable && write_failed {
        klog(b"[limina-init] LIMINA_SHARE_RO_OK");
    } else {
        klog(
            format!(
                "[limina-init] LIMINA_SHARE_RO_FAIL readable={readable} write_failed={write_failed}"
            )
            .as_bytes(),
        );
    }
}

fn mount(src: &CStr, target: &CStr, fstype: &CStr) {
    unsafe {
        libc::mount(
            src.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            std::ptr::null(),
        );
    }
}

/// Write the marker on every channel we can (kmsg → printk → serial), best-effort.
fn announce() {
    unsafe {
        write_to(c"/dev/kmsg", MARKER);
        write_to(c"/dev/console", MARKER);
        write_all(1, MARKER);
    }
}

/// Parse `limina.agent_port=<u32>` from /proc/cmdline, if present.
fn agent_port_from_cmdline() -> Option<u32> {
    let cmdline = std::fs::read_to_string("/proc/cmdline").ok()?;
    for tok in cmdline.split_whitespace() {
        if let Some(val) = tok.strip_prefix("limina.agent_port=") {
            return val.parse().ok();
        }
    }
    None
}

/// Marker the host harness waits for before sending input — proves guest→host output works.
const ECHO_READY: &[u8] = b"LIMINA_CONSOLE_ECHO_READY\n";

/// Interactive console echo loop over `/dev/console` (whatever the cmdline `console=`
/// selects — the harness wires this over virtio-console `hvc0`). Sets the tty raw so input
/// isn't line-edited or echoed by the line discipline (a deterministic round-trip),
/// announces readiness, then replies `ECHO:<line>` to every line until `QUIT`. Best-effort:
/// any failure just returns (we still power off). Validates input AND output of the console
/// through the real binaries; typed commands and their responses build on it.
fn run_console_echo() {
    unsafe {
        // /dev/console is the active console (hvc0 under the test harness); devtmpfs creates
        // it for whatever device backs console=. announce() already proved it's writable.
        let fd = libc::open(c"/dev/console".as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
        if fd < 0 {
            klog(b"[limina-init] console_echo: cannot open /dev/console");
            return;
        }
        let mut tio: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut tio) == 0 {
            libc::cfmakeraw(&mut tio);
            libc::tcsetattr(fd, libc::TCSANOW, &tio);
        }
        write_all(fd, ECHO_READY);

        let mut line: Vec<u8> = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = libc::read(fd, byte.as_mut_ptr() as *mut libc::c_void, 1);
            if n <= 0 {
                sleep_ms(5); // EOF/EAGAIN shouldn't happen on a raw tty; don't spin hard
                continue;
            }
            match byte[0] {
                b'\r' | b'\n' => {
                    if line == b"QUIT" {
                        break;
                    }
                    let mut resp = b"ECHO:".to_vec();
                    resp.extend_from_slice(&line);
                    resp.push(b'\n');
                    write_all(fd, &resp);
                    line.clear();
                }
                b => {
                    line.push(b);
                    if line.len() > 1024 {
                        line.clear(); // guard against an unbounded line
                    }
                }
            }
        }
        libc::close(fd);
    }
}

// --- framebuffer test pattern (M2 display oracle) ---------------------------------

const FBIOGET_VSCREENINFO: libc::c_int = 0x4600;
const FBIOGET_FSCREENINFO: libc::c_int = 0x4602;
const FBIOPAN_DISPLAY: libc::c_int = 0x4606;
const FBIOPUT_VSCREENINFO: libc::c_int = 0x4601;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FbBitfield {
    offset: u32,
    length: u32,
    msb_right: u32,
}

#[repr(C)]
struct FbVarScreeninfo {
    xres: u32,
    yres: u32,
    xres_virtual: u32,
    yres_virtual: u32,
    xoffset: u32,
    yoffset: u32,
    bits_per_pixel: u32,
    grayscale: u32,
    red: FbBitfield,
    green: FbBitfield,
    blue: FbBitfield,
    transp: FbBitfield,
    nonstd: u32,
    activate: u32,
    height: u32,
    width: u32,
    accel_flags: u32,
    pixclock: u32,
    left_margin: u32,
    right_margin: u32,
    upper_margin: u32,
    lower_margin: u32,
    hsync_len: u32,
    vsync_len: u32,
    sync: u32,
    vmode: u32,
    rotate: u32,
    colorspace: u32,
    reserved: [u32; 4],
}

#[repr(C)]
struct FbFixScreeninfo {
    id: [u8; 16],
    smem_start: libc::c_ulong,
    smem_len: u32,
    type_: u32,
    type_aux: u32,
    visual: u32,
    xpanstep: u16,
    ypanstep: u16,
    ywrapstep: u16,
    line_length: u32,
    mmio_start: libc::c_ulong,
    mmio_len: u32,
    accel: u32,
    capabilities: u16,
    reserved: [u16; 2],
}

/// A mapped `/dev/fb0`. `base`/`len` are the mmap; `bpp` is bytes per pixel. `fd`/`var`
/// are kept so we can force a scanout flush (FBIOPAN_DISPLAY) — on the virtio-gpu DRM
/// fbdev, mmap writes alone are not pushed to the host until something pans/closes it.
struct Fb {
    fd: libc::c_int,
    var: FbVarScreeninfo,
    base: *mut u8,
    len: usize,
    stride: usize,
    bpp: usize,
    xres: usize,
    yres: usize,
}

impl Fb {
    /// Force the DRM fbdev to push the current framebuffer to the host scanout. On the
    /// virtio-gpu fbdev, mmap writes alone don't issue a RESOURCE_FLUSH; FBIOPUT_VSCREENINFO
    /// (set_par) does. (FBIOPAN_DISPLAY is a no-op here — single-buffered — but harmless.)
    unsafe fn flush(&mut self) {
        libc::ioctl(self.fd, FBIOPAN_DISPLAY, &mut self.var);
        libc::ioctl(self.fd, FBIOPUT_VSCREENINFO, &mut self.var);
    }
}

/// Open + mmap `/dev/fb0`, returning its geometry. Best-effort: `None` if there's no
/// framebuffer (non-display guest) or anything fails.
fn open_framebuffer() -> Option<Fb> {
    unsafe {
        let fd = libc::open(c"/dev/fb0".as_ptr(), libc::O_RDWR);
        if fd < 0 {
            return None;
        }
        let mut var: FbVarScreeninfo = std::mem::zeroed();
        let mut fix: FbFixScreeninfo = std::mem::zeroed();
        if libc::ioctl(fd, FBIOGET_VSCREENINFO, &mut var) < 0
            || libc::ioctl(fd, FBIOGET_FSCREENINFO, &mut fix) < 0
        {
            libc::close(fd);
            return None;
        }
        let len = fix.smem_len as usize;
        let stride = fix.line_length as usize;
        let bpp = (var.bits_per_pixel / 8) as usize;
        let (xres, yres) = (var.xres as usize, var.yres as usize);
        if len == 0 || bpp == 0 || stride == 0 {
            libc::close(fd);
            return None;
        }
        let base = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        );
        // Keep the fd open for the surface's lifetime by leaking it (PID 1 never exits).
        if base == libc::MAP_FAILED {
            libc::close(fd);
            return None;
        }
        Some(Fb {
            fd,
            var,
            base: base as *mut u8,
            len,
            stride,
            bpp,
            xres,
            yres,
        })
    }
}

/// Fill the framebuffer: top half red, bottom half blue (BGRX), and `msync` to force the
/// deferred-IO writeback so the host presents (and captures) the frame. Two solid bands
/// give a frame with >1 colour and known top/bottom pixels to assert on.
fn draw_test_pattern() {
    let Some(fb) = open_framebuffer() else {
        return;
    };
    unsafe {
        let top = [0u8, 0, 255, 0]; // BGRX -> red
        let bot = [255u8, 0, 0, 0]; // BGRX -> blue
        for y in 0..fb.yres {
            let color = if y < fb.yres / 2 { &top } else { &bot };
            let row = fb.base.add(y * fb.stride);
            for x in 0..fb.xres {
                let px = row.add(x * fb.bpp);
                for (b, &v) in color.iter().enumerate().take(fb.bpp.min(4)) {
                    *px.add(b) = v;
                }
            }
        }
        libc::msync(fb.base as *mut libc::c_void, fb.len, libc::MS_SYNC);
        // The present reaches the host via the driver flush on power-off (this is the
        // one-shot capture oracle, not the live path — no FBIOPUT, which would clear the fb).
        sleep_ms(300);
    }
}

/// Animate scrolling colour bands forever (never returns) — keeps a guest alive with live
/// frames so the host window has something to show. Used when `limina.hold` is on the cmdline.
fn animate_forever() -> ! {
    let mut fb = open_framebuffer();
    match &fb {
        Some(fb) => klog(
            format!(
                "[limina-init] hold: animating {}x{} bpp={} stride={}",
                fb.xres,
                fb.yres,
                fb.bpp * 8,
                fb.stride
            )
            .as_bytes(),
        ),
        None => klog(b"[limina-init] hold: no /dev/fb0 (cannot animate)"),
    }
    // BGRX bands: red, green, blue.
    let bands = [[0u8, 0, 255, 0], [0, 255, 0, 0], [255, 0, 0, 0]];
    let mut frame = 0usize;
    loop {
        if let Some(fb) = &mut fb {
            unsafe {
                for y in 0..fb.yres {
                    let color = &bands[((y + frame) / 48) % bands.len()];
                    let row = fb.base.add(y * fb.stride);
                    for x in 0..fb.xres {
                        let px = row.add(x * fb.bpp);
                        for (b, &v) in color.iter().enumerate().take(fb.bpp.min(4)) {
                            *px.add(b) = v;
                        }
                    }
                }
                fb.flush();
            }
        }
        frame += 3;
        unsafe { sleep_ms(33) };
    }
}

/// Write a line to /dev/kmsg (printk -> serial), best-effort.
fn klog(msg: &[u8]) {
    let mut line = Vec::with_capacity(msg.len() + 1);
    line.extend_from_slice(msg);
    line.push(b'\n');
    unsafe { write_to(c"/dev/kmsg", &line) };
}

unsafe fn sleep_ms(ms: i64) {
    let ts = libc::timespec {
        tv_sec: ms / 1000,
        tv_nsec: (ms % 1000) * 1_000_000,
    };
    libc::nanosleep(&ts, std::ptr::null_mut());
}

/// Open `path` write-only and write `buf`, ignoring any failure.
unsafe fn write_to(path: &CStr, buf: &[u8]) {
    let fd = libc::open(path.as_ptr(), libc::O_WRONLY);
    if fd >= 0 {
        write_all(fd, buf);
        libc::close(fd);
    }
}

/// Write the whole buffer, ignoring partial writes/errors (best-effort, never panics).
unsafe fn write_all(fd: libc::c_int, mut buf: &[u8]) {
    while !buf.is_empty() {
        let n = libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len());
        if n <= 0 {
            break;
        }
        buf = &buf[n as usize..];
    }
}

/// Clean PSCI power-off. Never returns; spins as a last resort so PID 1 can't die.
fn power_off() -> ! {
    unsafe {
        libc::sync();
        libc::reboot(libc::RB_POWER_OFF);
        loop {
            libc::pause();
        }
    }
}
