// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! limina-init — PID 1 for the L1 tiny test guest, and the seed of the future limina-agent.
//!
//! Flow: prove userspace (console marker), then — if the kernel cmdline carries
//! `limina.agent_port=<N>` — run a tiny **vsock agent** that connects to the host
//! (`CID_HOST:N`) and speaks a line protocol so the host test harness can make
//! STRUCTURED assertions (not console scraping). Finally power the VM off cleanly via
//! PSCI. Every path ends at [`power_off`]; as PID 1 it must never return or panic.
//!
//! vsock protocol (guest → host on connect):
//!   send: `READY pagesize=<N>\n`
//!   recv: one line (e.g. `POWEROFF`) — content ignored; it just gates shutdown.

use std::ffi::CStr;

/// Marker the host harness asserts on. Keep in sync with `crates/limina-test`.
const MARKER: &[u8] = b"\n[limina-init] LIMINA_L1_USERSPACE_OK\n";

fn main() {
    mount_pseudo_fs();
    announce();
    if let Some(port) = agent_port_from_cmdline() {
        run_agent(port);
    }
    // Draw a known pattern to the framebuffer last, so nothing overwrites it before the
    // host captures the scanout (the M2 display oracle). Best-effort: no fb -> skip.
    draw_test_pattern();
    power_off();
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

/// Connect to the host over vsock (`CID_HOST:port`) and run the tiny agent protocol.
/// Best-effort: any failure just returns (we still power off).
fn run_agent(port: u32) {
    unsafe {
        let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return;
        }
        let mut addr: libc::sockaddr_vm = std::mem::zeroed();
        addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
        addr.svm_port = port;
        addr.svm_cid = libc::VMADDR_CID_HOST;

        // The host listener + libkrun muxer may need a moment after boot; retry briefly.
        let mut connected = false;
        for _ in 0..100 {
            let r = libc::connect(
                fd,
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
            );
            if r == 0 {
                connected = true;
                break;
            }
            sleep_ms(20);
        }
        if !connected {
            libc::close(fd);
            return;
        }

        let pagesize = libc::sysconf(libc::_SC_PAGESIZE);
        let hello = format!("READY pagesize={pagesize}\n");
        write_all(fd, hello.as_bytes());

        // Wait for a command line from the host (content ignored; gates shutdown).
        let mut buf = [0u8; 64];
        let _ = libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len());
        libc::close(fd);
    }
}

// --- framebuffer test pattern (M2 display oracle) ---------------------------------

const FBIOGET_VSCREENINFO: libc::c_int = 0x4600;
const FBIOGET_FSCREENINFO: libc::c_int = 0x4602;

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

/// Fill `/dev/fb0` with a deterministic pattern — top half red, bottom half blue — and
/// `msync` to force the deferred-IO writeback so the host presents (and captures) the
/// frame. Best-effort and panic-free: if there's no framebuffer, just return.
///
/// The virtio-gpu fbdev format is BGRX (bytes `[B, G, R, X]`). Two solid bands give the
/// host a frame with >1 colour and known top/bottom pixels to assert on.
fn draw_test_pattern() {
    unsafe {
        let fd = libc::open(c"/dev/fb0".as_ptr(), libc::O_RDWR);
        if fd < 0 {
            return; // no framebuffer (non-display guest) — nothing to draw.
        }
        let mut var: FbVarScreeninfo = std::mem::zeroed();
        let mut fix: FbFixScreeninfo = std::mem::zeroed();
        if libc::ioctl(fd, FBIOGET_VSCREENINFO, &mut var) < 0
            || libc::ioctl(fd, FBIOGET_FSCREENINFO, &mut fix) < 0
        {
            libc::close(fd);
            return;
        }

        let len = fix.smem_len as usize;
        let stride = fix.line_length as usize;
        let bpp = (var.bits_per_pixel / 8) as usize;
        let (xres, yres) = (var.xres as usize, var.yres as usize);
        if len == 0 || bpp == 0 || stride == 0 {
            libc::close(fd);
            return;
        }

        let base = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        );
        if base == libc::MAP_FAILED {
            libc::close(fd);
            return;
        }

        let p = base as *mut u8;
        let top = [0u8, 0, 255, 0]; // BGRX -> red
        let bot = [255u8, 0, 0, 0]; // BGRX -> blue
        for y in 0..yres {
            let color = if y < yres / 2 { &top } else { &bot };
            let row = p.add(y * stride);
            for x in 0..xres {
                let px = row.add(x * bpp);
                for (b, &v) in color.iter().enumerate().take(bpp.min(4)) {
                    *px.add(b) = v;
                }
            }
        }

        // Force the framebuffer's deferred-IO writeback -> virtio-gpu transfer+flush.
        libc::msync(base, len, libc::MS_SYNC);
        libc::munmap(base, len);
        libc::close(fd);
        // Give the async present a moment to reach the host before we power off.
        sleep_ms(300);
    }
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
