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
    if let Some(port) = agent_port_from_cmdline() {
        // A host-ordered SHUTDOWN powers off NOW — it must override `limina.hold` (the
        // whole point is that closing the window ends a held/animating guest).
        if agent::run(port) == agent::AgentEnd::Shutdown {
            power_off();
        }
    }
    // `limina.real_agent`: spawn the PRODUCT agent binary (staged at /limina-agent by
    // build-test-guest.sh) — the L1 vehicle for testing the real limina-agent end-to-end.
    // It owns the control channel and powers the guest off itself on SHUTDOWN (via raw
    // reboot(2) here; no systemd in this world). Init carries on (typically `limina.hold`).
    if cmdline_has("limina.real_agent") {
        match std::process::Command::new("/limina-agent").spawn() {
            Ok(_) => klog(b"[limina-init] spawned /limina-agent"),
            Err(_) => klog(b"[limina-init] failed to spawn /limina-agent"),
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

/// Mount devtmpfs on /dev and procfs on /proc (best-effort). The kernel may not have
/// opened an initial console for us, so we need /dev/kmsg ourselves; /proc/cmdline tells
/// us whether to run the agent.
fn mount_pseudo_fs() {
    mount(c"devtmpfs", c"/dev", c"devtmpfs");
    mount(c"proc", c"/proc", c"proc");
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
