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
    // `limina.usb_probe`: assert the guest-side USB/IP prerequisites are present (M7). The
    // USB/IP guest stack is 100% upstream — this proves our kernel carries it: the vhci_hcd
    // VIRTUAL host controller (no real EHCI/XHCI), the usbip_core/vhci modules, the usb bus,
    // and uinput. Emits one RESULT line per check for the harness to assert, then powers off.
    if cmdline_has("limina.usb_probe") {
        run_usb_probe();
        power_off();
    }
    // `limina.usb_attach=<port>`: the M7 mock-attach end-to-end. Connect a vsock to the host
    // USB/IP server (CID_HOST:<port>), do the import handshake, and hand the socket fd to the
    // kernel's vhci_hcd — which then runs URB traffic against the host server. The host's mock
    // CDC-ACM device enumerates and `cdc-acm` binds it as /dev/ttyACM0. Proves passthrough with
    // no physical USB. Emits RESULT markers; powers off.
    if let Some(port) = cmdline_value("limina.usb_attach") {
        if let Ok(p) = port.parse::<u32>() {
            run_usb_attach(p);
        }
        power_off();
    }
    // `limina.blob_probe`: create + mmap a host-visible virtio-gpu blob whose size is
    // 4 KiB- but NOT 16 KiB-aligned — the deterministic guest-side repro for the 16 KiB-host
    // hv_vm_map blob alignment bug (see tests/l1_blob_map.rs). Emits RESULT markers; powers off.
    if cmdline_has("limina.blob_probe") {
        run_blob_probe();
        power_off();
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
    // `limina.console_shell`: the next step up from echo — a tiny in-process command
    // interpreter so the host harness can "type a command, assert its output" over the
    // serial console (closes M2.5 Track A for the L1 guest; stock Fedora uses a real getty).
    if cmdline_has("limina.console_shell") {
        run_console_shell();
    }
    // `limina.counter`: the M9.1 suspend/resume oracle. Loop forever incrementing an
    // in-guest-RAM counter and emitting a heartbeat line to the console (PL011 ttyAMA0,
    // via /dev/kmsg → printk). The counter lives in guest RAM and the vCPU keeps executing,
    // so after a host-side snapshot + `--restore` the value MUST continue climbing from where
    // it was (RAM + vCPU state rode the snapshot), not reset to ~0 (which would mean a fresh
    // boot). PL011 is near-stateless output, so heartbeats keep flowing even before virtio
    // device-state restore (M9.2). Never returns.
    if cmdline_has("limina.counter") {
        run_counter();
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
        let mut cmd = std::process::Command::new("/limina-mock-mutter");
        cmd.env("DBUS_SESSION_BUS_ADDRESS", DBUS_ADDR)
            .env("LIMINA_MOCK_ID", &mock_id);
        // `limina.mock_bridge`: the mock also claims org.limina.Clipboard (the
        // clipboard@limina extension stand-in), so tests can drive/assert the
        // helper's middle backend and its tier preference.
        if cmdline_has("limina.mock_bridge") {
            cmd.env("LIMINA_MOCK_BRIDGE", "1");
        }
        match cmd.spawn() {
            Ok(_) => klog(b"[limina-init] spawned /limina-mock-mutter"),
            Err(_) => klog(b"[limina-init] failed to spawn /limina-mock-mutter"),
        }
    }
    if cmdline_has("limina.session_helper") {
        let mut cmd = std::process::Command::new("/limina-agent-session");
        cmd.env("DBUS_SESSION_BUS_ADDRESS", DBUS_ADDR);
        // The RemoteDesktop fallback is opt-in (it lights GNOME's screen-share
        // indicator, so production defaults it OFF); the L1 mock-mutter tests that
        // exercise that backend opt in via this cmdline token.
        if cmdline_has("limina.clipboard_rd") {
            cmd.env("LIMINA_CLIPBOARD_RD", "1");
        }
        match cmd.spawn() {
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

/// Marker the host harness waits for before sending commands (proves guest→host output works).
const SHELL_READY: &[u8] = b"LIMINA_SHELL_READY\n";

/// Interactive **command mode** over `/dev/console` — the step up from `run_console_echo`.
/// Reads a line, runs one of a small set of built-in commands ([`run_builtin`]), writes its
/// output, then a `LIMINA_SHELL_DONE rc=<n>` frame terminator the host keys on to slice out
/// exactly this command's output (see `Guest::console_command`). `exit`/`QUIT` leaves the loop
/// and falls through to power-off. The L1 rootfs holds only `/init`, so the commands are
/// interpreted in-process — the init *is* the shell. This is the "type a command at the guest,
/// assert its output" capability that closes M2.5 Track A for the L1 guest. Best-effort: any
/// failure just returns (we still power off).
fn run_console_shell() {
    unsafe {
        let fd = libc::open(c"/dev/console".as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
        if fd < 0 {
            klog(b"[limina-init] console_shell: cannot open /dev/console");
            return;
        }
        let mut tio: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut tio) == 0 {
            libc::cfmakeraw(&mut tio);
            libc::tcsetattr(fd, libc::TCSANOW, &tio);
        }
        write_all(fd, SHELL_READY);

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
                    let cmd = String::from_utf8_lossy(&line).trim().to_string();
                    line.clear();
                    if cmd.is_empty() {
                        continue;
                    }
                    if cmd == "exit" || cmd == "QUIT" {
                        break;
                    }
                    let rc = run_builtin(fd, &cmd);
                    write_all(fd, format!("LIMINA_SHELL_DONE rc={rc}\n").as_bytes());
                }
                b => {
                    line.push(b);
                    if line.len() > 4096 {
                        line.clear(); // guard against an unbounded line
                    }
                }
            }
        }
        libc::close(fd);
    }
}

/// Execute one built-in command, writing its output to `fd`; returns a shell-style rc
/// (0 = ok, 127 = unknown). Deliberately tiny — enough to read real guest state (files,
/// kernel identity) over the console so tests can assert on it without a real shell binary.
unsafe fn run_builtin(fd: libc::c_int, cmd: &str) -> i32 {
    let mut parts = cmd.split_whitespace();
    let Some(prog) = parts.next() else {
        return 0;
    };
    match prog {
        // echo the rest of the line back verbatim (the simplest round-trip).
        "echo" => {
            let rest = cmd.strip_prefix("echo").unwrap_or("").trim_start();
            write_all(fd, rest.as_bytes());
            write_all(fd, b"\n");
            0
        }
        // cat a guest file (e.g. /proc/cmdline, /proc/meminfo) — reads real guest state.
        "cat" => match parts.next() {
            Some(path) => match std::fs::read(path) {
                Ok(bytes) => {
                    write_all(fd, &bytes);
                    if !bytes.ends_with(b"\n") {
                        write_all(fd, b"\n");
                    }
                    0
                }
                Err(_) => {
                    write_all(fd, format!("cat: {path}: cannot read\n").as_bytes());
                    1
                }
            },
            None => {
                write_all(fd, b"cat: missing path\n");
                1
            }
        },
        // ls a directory (names only, newline-separated) — lets tests discover dynamic sysfs
        // paths (e.g. the virtio-gpu DRM connector under /sys/class/drm) without a shell binary.
        "ls" => match parts.next() {
            Some(path) => match std::fs::read_dir(path) {
                Ok(entries) => {
                    for entry in entries.flatten() {
                        write_all(fd, entry.file_name().as_encoded_bytes());
                        write_all(fd, b"\n");
                    }
                    0
                }
                Err(_) => {
                    write_all(fd, format!("ls: {path}: cannot read\n").as_bytes());
                    1
                }
            },
            None => {
                write_all(fd, b"ls: missing path\n");
                1
            }
        },
        // uname(2): a syscall, not a file — proves the guest acts on the command.
        "uname" => {
            let mut u: libc::utsname = std::mem::zeroed();
            if libc::uname(&mut u) == 0 {
                let sys = cstr_to_string(u.sysname.as_ptr());
                let rel = cstr_to_string(u.release.as_ptr());
                write_all(fd, format!("{sys} {rel}\n").as_bytes());
                0
            } else {
                write_all(fd, b"uname: failed\n");
                1
            }
        }
        other => {
            write_all(
                fd,
                format!("limina-shell: unknown command: {other}\n").as_bytes(),
            );
            127
        }
    }
}

/// Render a NUL-terminated C char array (e.g. a `utsname` field) as a String.
unsafe fn cstr_to_string(ptr: *const libc::c_char) -> String {
    CStr::from_ptr(ptr).to_string_lossy().into_owned()
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

/// Marker the host harness waits for before sampling heartbeats — proves the counter is live.
const COUNTER_READY: &[u8] = b"[limina-init] LIMINA_COUNTER_READY\n";

/// `limina.counter` (M9.1 suspend/resume oracle). Increment an in-RAM counter forever and
/// emit `LIMINA_COUNTER n=<N> mono_ms=<T>` heartbeats to the console. Both values live in
/// guest RAM / the vCPU's monotonic counter, so a host snapshot + `--restore` must resume
/// them mid-climb: `n` keeps rising (never resets toward 0) and `mono_ms` never jumps
/// backwards. Emitted over PL011 (`/dev/kmsg` → printk → ttyAMA0), which keeps working
/// across a fresh-worker restore before virtio device state is restored (M9.2). Never returns.
fn run_counter() -> ! {
    unsafe { write_to(c"/dev/kmsg", COUNTER_READY) };
    let mut n: u64 = 0;
    loop {
        n = n.wrapping_add(1);
        // Heartbeat roughly every ~100 ms (20 × 5 ms); frequent enough that the harness
        // catches several before and after a snapshot without flooding the console.
        if n.is_multiple_of(20) {
            klog(
                format!(
                    "[limina-init] LIMINA_COUNTER n={n} mono_ms={}",
                    monotonic_ms()
                )
                .as_bytes(),
            );
        }
        unsafe { sleep_ms(5) };
    }
}

/// `CLOCK_MONOTONIC` in milliseconds — the guest-visible monotonic clock (backed by CNTVCT
/// via the vtimer). Used by [`run_counter`] so the harness can assert monotonic time didn't
/// leap backwards across restore. Returns 0 if the syscall fails (best-effort, never panics).
fn monotonic_ms() -> u64 {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } == 0 {
        (ts.tv_sec as u64).wrapping_mul(1000) + (ts.tv_nsec as u64) / 1_000_000
    } else {
        0
    }
}

/// Probe the guest-side USB/IP prerequisites (M7) and emit a `RESULT: <name> <PRESENT|MISSING>`
/// line per check. The host harness asserts each is PRESENT. These are all upstream kernel
/// facilities our config (`build-test-kernel.sh` FRAG) turns on; the test proves they survived.
fn run_usb_probe() {
    // (marker, path that exists iff the facility is compiled in & initialised)
    let checks: &[(&str, &str)] = &[
        // The virtual host controller usbip attaches remote devices to (no real HCD needed).
        ("vhci_hcd", "/sys/devices/platform/vhci_hcd.0"),
        // usbip core + the vhci driver register under the platform bus / driver tree.
        ("usbip_vhci_driver", "/sys/bus/platform/drivers/vhci_hcd"),
        // The USB bus type itself (usbcore) — devices enumerate here once attached.
        ("usb_bus", "/sys/bus/usb"),
        // uinput — its absence has bitten guest UI scripting; folded into the same config.
        ("uinput", "/dev/uinput"),
    ];
    klog(b"[limina-init] usb_probe: begin");
    for (name, path) in checks {
        // A `vhci_hcd.0` platform device only appears once the driver's probe ran; for /dev
        // nodes devtmpfs must have created them. Both are simple existence checks.
        let present = std::path::Path::new(path).exists();
        let mut line = Vec::new();
        line.extend_from_slice(b"[limina-init] RESULT: ");
        line.extend_from_slice(name.as_bytes());
        line.extend_from_slice(if present { b" PRESENT" } else { b" MISSING" });
        klog(&line);
    }
    klog(b"[limina-init] usb_probe: done");
}

/// Drive the USB/IP **client** side directly (no `usbip` userspace tool): import the host's mock
/// device over vsock and hand the fd to `vhci_hcd`. The USB/IP op_ protocol here is hand-rolled
/// (a fixed 40-byte request, a 320-byte reply) to avoid pulling a crate into the guest; it mirrors
/// `crates/limina-usbip/src/proto.rs` exactly. `vhci_hcd`'s attach accepts any SOCK_STREAM fd
/// (verified in the kernel: `vhci_sysfs.c` checks only `SOCK_STREAM`, not the address family), so
/// an `AF_VSOCK` socket works with no kernel patch.
fn run_usb_attach(port: u32) {
    const BUSID: &str = "1-1"; // the host MockBackend's single device
    klog(b"[limina-init] usb_attach: begin");

    // --- connect a vsock to the host USB/IP server (with a brief retry, like the agent) ---
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        klog(b"[limina-init] usb_attach: socket() failed");
        return;
    }
    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    addr.svm_port = port;
    addr.svm_cid = libc::VMADDR_CID_HOST;
    let mut connected = false;
    for _ in 0..100 {
        let r = unsafe {
            libc::connect(
                fd,
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
            )
        };
        if r == 0 {
            connected = true;
            break;
        }
        unsafe { sleep_ms(20) };
    }
    if !connected {
        klog(b"[limina-init] usb_attach: vsock connect failed");
        unsafe { libc::close(fd) };
        return;
    }

    // --- OP_REQ_IMPORT: version 0x0111, code 0x8003, status 0, busid[32] (big-endian header) ---
    let mut req = Vec::with_capacity(40);
    req.extend_from_slice(&0x0111u16.to_be_bytes());
    req.extend_from_slice(&0x8003u16.to_be_bytes());
    req.extend_from_slice(&0u32.to_be_bytes());
    let mut busid = [0u8; 32];
    busid[..BUSID.len()].copy_from_slice(BUSID.as_bytes());
    req.extend_from_slice(&busid);
    if !fd_write_all(fd, &req) {
        klog(b"[limina-init] usb_attach: sending OP_REQ_IMPORT failed");
        unsafe { libc::close(fd) };
        return;
    }

    // --- OP_REP_IMPORT: 8-byte header + 0x138 device body = 320 bytes ---
    let mut rep = [0u8; 8 + 0x138];
    if !fd_read_exact(fd, &mut rep) {
        klog(b"[limina-init] usb_attach: short OP_REP_IMPORT");
        unsafe { libc::close(fd) };
        return;
    }
    let status = u32::from_be_bytes([rep[4], rep[5], rep[6], rep[7]]);
    if status != 0 {
        klog(b"[limina-init] usb_attach: server refused import");
        unsafe { libc::close(fd) };
        return;
    }
    // busnum/devnum/speed sit after header(8) + path[256] + busid[32] = offset 296.
    let be32 = |o: usize| u32::from_be_bytes([rep[o], rep[o + 1], rep[o + 2], rep[o + 3]]);
    let busnum = be32(296);
    let devnum = be32(300);
    let speed = be32(304);
    let devid = (busnum << 16) | (devnum & 0xffff);

    // --- hand the connected fd to vhci_hcd: "port sockfd devid speed" (all DECIMAL, per the
    // kernel's `sscanf(buf, "%u %u %u %u", ...)` in vhci_sysfs.c). Port 0 = first virtual port. ---
    let attach = format!("0 {fd} {devid} {speed}\n");
    unsafe {
        write_to(
            c"/sys/devices/platform/vhci_hcd.0/attach",
            attach.as_bytes(),
        )
    };
    klog(format!("[limina-init] usb_attach: attached devid={devid} speed={speed}").as_bytes());

    // --- the kernel now enumerates the device against the host; cdc-acm should create
    // /dev/ttyACM0. Poll for it (the import + enumeration take a beat). ---
    let mut appeared = false;
    for _ in 0..150 {
        if std::path::Path::new("/dev/ttyACM0").exists() {
            appeared = true;
            break;
        }
        unsafe { sleep_ms(20) };
    }
    if appeared {
        klog(b"[limina-init] RESULT: ttyACM0 PRESENT");
    } else {
        klog(b"[limina-init] RESULT: ttyACM0 MISSING");
    }
    klog(b"[limina-init] usb_attach: done");
    // Leave `fd` open: the kernel holds its own reference, but PID 1 closing it early is needless.
}

// --- virtio-gpu blob-map probe (the 16 KiB-host alignment repro) ---------------------------
//
// A stock 4 KiB guest can create a host-visible blob whose size is a multiple of its OWN page
// size but not of the HOST's 16 KiB page (e.g. 0x21000 = 33 × 4 KiB). Mapping it into the
// guest's shm window goes through hv_vm_map on the host, which rejects non-16 KiB-granular
// sizes with HV_BAD_ARGUMENT — so the guest's mmap fails with EINVAL. This probe hand-rolls
// the exact sequence Mesa's virgl driver would issue (mirroring the hand-rolled USB/IP client
// above): virgl context init → an EXECBUFFER creating an untyped persistently-mappable
// PIPE_BUFFER carrying a blob_id → RESOURCE_CREATE_BLOB(HOST3D, MAPPABLE) referencing it →
// mmap. The first vram allocation of the boot lands at shm-window offset 0 (16 KiB-aligned),
// so ONLY the odd size is under test. Constants below are ABI (virtgpu_drm.h @ v6.12,
// virgl_protocol.h/virgl_hw.h) — stable wire format, safe to inline.

/// 33 × 4 KiB: 4 KiB-aligned, NOT 16 KiB-aligned — the size class that trips hv_vm_map.
const BLOB_ODD_SIZE: u64 = 0x21000;

/// Linux `_IOWR('d', nr, size)` — DRM ioctl request codes (dir RW=3, type 'd'=0x64).
/// musl's `ioctl` takes a C `int`, so the high dir bits wrap into the sign bit — as the
/// kernel expects (it truncates the request to 32 bits).
const fn drm_iowr(nr: u64, size: u64) -> libc::Ioctl {
    (((3u64 << 30) | (size << 16) | (0x64 << 8) | nr) as u32) as libc::Ioctl
}
/// Linux `_IOW('d', nr, size)` (dir W=1).
const fn drm_iow(nr: u64, size: u64) -> libc::Ioctl {
    (((1u64 << 30) | (size << 16) | (0x64 << 8) | nr) as u32) as libc::Ioctl
}

#[repr(C)]
struct VirtgpuContextSetParam {
    param: u64,
    value: u64,
}
#[repr(C)]
struct VirtgpuContextInit {
    num_params: u32,
    pad: u32,
    ctx_set_params: u64,
}
#[repr(C)]
struct VirtgpuGetCaps {
    cap_set_id: u32,
    cap_set_ver: u32,
    addr: u64,
    size: u32,
    pad: u32,
}
#[repr(C)]
struct VirtgpuExecbuffer {
    flags: u32,
    size: u32,
    command: u64,
    bo_handles: u64,
    num_bo_handles: u32,
    fence_fd: i32,
    ring_idx: u32,
    syncobj_stride: u32,
    num_in_syncobjs: u32,
    num_out_syncobjs: u32,
    in_syncobjs: u64,
    out_syncobjs: u64,
}
#[repr(C)]
struct VirtgpuResourceCreateBlob {
    blob_mem: u32,
    blob_flags: u32,
    bo_handle: u32,
    res_handle: u32,
    size: u64,
    pad: u32,
    cmd_size: u32,
    cmd: u64,
    blob_id: u64,
}
#[repr(C)]
struct VirtgpuMap {
    offset: u64,
    handle: u32,
    pad: u32,
}
#[repr(C)]
struct GemClose {
    handle: u32,
    pad: u32,
}

/// Emit a `RESULT: <name> <OK|FAIL errno=N>` marker the host harness asserts on.
fn blob_result(name: &str, ok: bool) {
    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    let line = if ok {
        format!("[limina-init] RESULT: {name} OK")
    } else {
        format!("[limina-init] RESULT: {name} FAIL errno={errno}")
    };
    klog(line.as_bytes());
}

/// Drive the raw virtio-gpu blob-map sequence and report each step. Best-effort: any failed
/// step reports FAIL and returns (the harness sees which step broke); we still power off.
fn run_blob_probe() {
    klog(b"[limina-init] blob_probe: begin");

    // virtio-gpu's DRM probe is asynchronous; give the node a moment to appear.
    let card = c"/dev/dri/card0";
    let mut fd = -1;
    for _ in 0..250 {
        fd = unsafe { libc::open(card.as_ptr(), libc::O_RDWR) };
        if fd >= 0 {
            break;
        }
        unsafe { sleep_ms(20) };
    }
    if fd < 0 {
        blob_result("blob_card0", false);
        return;
    }
    blob_result("blob_card0", true);

    unsafe {
        // Bind the DRM context to the virgl2 capset (vrend) — what Mesa's virgl driver does.
        // VIRTGPU_CONTEXT_PARAM_CAPSET_ID = 1; capset id 2 = virgl2, fall back to 1 = virgl.
        let mut inited = false;
        for capset in [2u64, 1] {
            let param = VirtgpuContextSetParam {
                param: 1,
                value: capset,
            };
            let init = VirtgpuContextInit {
                num_params: 1,
                pad: 0,
                ctx_set_params: &param as *const _ as u64,
            };
            if libc::ioctl(fd, drm_iowr(0x4b, 16), &init) == 0 {
                inited = true;
                break;
            }
        }
        blob_result("blob_ctx_init", inited);
        if !inited {
            libc::close(fd);
            return;
        }

        // Read the virgl capset like Mesa does before any 3D work. Load-bearing beyond
        // realism: vrend initializes its map-caching type (which becomes every buffer's
        // map_info, the blob-mappability gate) lazily inside the caps fill — skip this and
        // even a mappable buffer reports map_info NONE.
        let mut caps = [0u8; 4096];
        let mut got_caps = false;
        for capset in [2u32, 1] {
            let get = VirtgpuGetCaps {
                cap_set_id: capset,
                cap_set_ver: 2,
                addr: caps.as_mut_ptr() as u64,
                size: caps.len() as u32,
                pad: 0,
            };
            if libc::ioctl(fd, drm_iowr(0x49, 24), &get) == 0 {
                got_caps = true;
                break;
            }
        }
        blob_result("blob_get_caps", got_caps);
        if !got_caps {
            libc::close(fd);
            return;
        }

        // Blob 1 — the SIZE half of the alignment bug: 4 KiB- but not 16 KiB-aligned size at
        // window offset 0 (16 KiB-aligned, first allocation of the boot). Fixed host-side by
        // libkrun patch 0043 (hv map/unmap size rounding).
        let one = probe_one_blob(fd, 1, BLOB_ODD_SIZE, "blob");

        // Blob 2 — the OFFSET half: with blob 1's 0x21000-byte node still occupying the head
        // of the window, an unaligned guest kernel packs this node at offset 0x21000
        // (guest_addr%16k=4096 → hv_vm_map HV_BAD_ARGUMENT no matter the size), while a
        // kernel with 16 KiB-aligned host-visible allocation (patches/linux/0004 / the
        // limina-virtio-gpu DKMS module) places it at 0x24000 and it maps. The size (0x4000)
        // is itself 16 KiB-aligned so ONLY the offset is under test.
        let two = probe_one_blob(fd, 2, 0x4000, "blob2");

        // Release the bos — drives the host-side UNMAP_BLOB (remove_mapping) path too.
        for handle in [one, two].into_iter().flatten() {
            let close = GemClose { handle, pad: 0 };
            libc::ioctl(fd, drm_iow(0x09, 8), &close);
        }
        libc::close(fd);
    }
    klog(b"[limina-init] blob_probe: done");
}

/// Create one host-visible mappable blob of `size` bytes (an untyped persistently-mappable
/// vrend PIPE_BUFFER tagged `blob_id`), mmap it, and prove it holds live memory. Emits
/// `RESULT: <tag>_{execbuffer,create,map_offset,map,rw}` markers; returns the bo handle on
/// success (the mapping is released, the bo — and its drm_mm window node — stays alive so a
/// later blob packs after it).
unsafe fn probe_one_blob(fd: libc::c_int, blob_id: u64, size: u64, tag: &str) -> Option<u32> {
    // EXECBUFFER: VIRGL_CCMD_PIPE_RESOURCE_CREATE(48), len 11 dwords — an untyped
    // PIPE_BUFFER (target 0, format R8_UNORM=64, bind VERTEX_BUFFER=1<<4), flags
    // MAP_PERSISTENT|MAP_COHERENT (1<<1 | 1<<2) so vrend gives it persistently mappable
    // glBufferStorage storage, tagged for the CREATE_BLOB below.
    let cmds: [u32; 12] = [
        48 | (11 << 16),     // VIRGL_CMD0(PIPE_RESOURCE_CREATE, 0, 11)
        0,                   // target = PIPE_BUFFER
        64,                  // format = VIRGL_FORMAT_R8_UNORM
        1 << 4,              // bind = VIRGL_BIND_VERTEX_BUFFER
        size as u32,         // width = size in bytes
        1,                   // height
        1,                   // depth
        1,                   // array_size
        0,                   // last_level
        0,                   // nr_samples
        (1 << 1) | (1 << 2), // flags = MAP_PERSISTENT | MAP_COHERENT
        blob_id as u32,      // blob_id
    ];
    let exec = VirtgpuExecbuffer {
        flags: 0,
        size: (cmds.len() * 4) as u32,
        command: cmds.as_ptr() as u64,
        bo_handles: 0,
        num_bo_handles: 0,
        fence_fd: 0,
        ring_idx: 0,
        syncobj_stride: 0,
        num_in_syncobjs: 0,
        num_out_syncobjs: 0,
        in_syncobjs: 0,
        out_syncobjs: 0,
    };
    let ok = libc::ioctl(fd, drm_iowr(0x42, 64), &exec) == 0;
    blob_result(&format!("{tag}_execbuffer"), ok);
    if !ok {
        return None;
    }

    // RESOURCE_CREATE_BLOB: HOST3D (2) + USE_MAPPABLE (1), referencing the blob_id. The
    // kernel maps the vram bo into the host-visible window right here (the host-side
    // MAP_BLOB → hv_vm_map), but records failure asynchronously — mmap below is where a
    // host-side failure surfaces (map_state != OK → EINVAL).
    let mut blob = VirtgpuResourceCreateBlob {
        blob_mem: 2,
        blob_flags: 1,
        bo_handle: 0,
        res_handle: 0,
        size,
        pad: 0,
        cmd_size: 0,
        cmd: 0,
        blob_id,
    };
    let ok = libc::ioctl(fd, drm_iowr(0x4a, 48), &mut blob) == 0;
    blob_result(&format!("{tag}_create"), ok);
    if !ok {
        return None;
    }

    // The DRM mmap fake offset for the bo.
    let mut map = VirtgpuMap {
        offset: 0,
        handle: blob.bo_handle,
        pad: 0,
    };
    let ok = libc::ioctl(fd, drm_iowr(0x41, 16), &mut map) == 0;
    blob_result(&format!("{tag}_map_offset"), ok);
    if !ok {
        return Some(blob.bo_handle);
    }

    // THE ASSERTION: mmap succeeds only if the host hv_vm_map'ed the blob into the shm
    // window. EINVAL = the host rejected the mapping (misaligned size pre-libkrun-0043,
    // misaligned window offset pre-guest-alignment).
    let ptr = libc::mmap(
        std::ptr::null_mut(),
        size as usize,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_SHARED,
        fd,
        map.offset as libc::off_t,
    );
    let mapped = ptr != libc::MAP_FAILED;
    blob_result(&format!("{tag}_map"), mapped);

    if mapped {
        // Prove the pages are live host memory end to end: write + read back through the
        // mapping, including the last byte of the tail page.
        let base = ptr as *mut u8;
        *base = 0xa5;
        *base.add((size - 1) as usize) = 0x5a;
        let rw = *base == 0xa5 && *base.add((size - 1) as usize) == 0x5a;
        blob_result(&format!("{tag}_rw"), rw);
        libc::munmap(ptr, size as usize);
    }
    Some(blob.bo_handle)
}

/// Write the whole buffer to a raw fd, returning false on any error. Used by the USB/IP client
/// before the fd is handed to the kernel.
fn fd_write_all(fd: libc::c_int, mut buf: &[u8]) -> bool {
    while !buf.is_empty() {
        let n = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if n <= 0 {
            return false;
        }
        buf = &buf[n as usize..];
    }
    true
}

/// Read exactly `buf.len()` bytes from a raw fd, returning false on EOF/error.
fn fd_read_exact(fd: libc::c_int, mut buf: &mut [u8]) -> bool {
    while !buf.is_empty() {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            return false;
        }
        buf = &mut buf[n as usize..];
    }
    true
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
