// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Start the VM worker from launchd instead of as the supervisor's child.
//!
//! **Why.** macOS Game Mode clamps every process in an app's process tree to priority 4 while a
//! game is fullscreen: timers ~100 ms late, CPU denied, CoreAudio callbacks stalled for up to a
//! second. The supervisor is an app, so a worker it posix_spawns is clamped with it, and nothing
//! inside a clamped process lifts it. A process launchd starts outside any app is left alone, as
//! long as its job declares `ProcessType=Interactive` (`spikes/game-mode-throttle/RESULTS.md`).
//!
//! **Shape.** The supervisor loads a transient gui-domain job ([`launch`]) whose program is
//! `limina-vmm --launcher <label> <supervisor pid>` and whose plist declares `<label>` as a
//! `MachServices` name. launchd holds that name from load, so the supervisor looks it up and sends
//! the handoff straight away; the message waits until the launcher checks in ([`launcher_main`]).
//! The handoff is one Mach message: every fd the worker needs as a fileport, plus its program,
//! argv, environment and working directory. The launcher accepts it only from the supervisor's
//! pid (the audit token), puts each fd back at the number the supervisor gave it, and spawns the
//! unchanged worker as its own child. So the worker sees exactly what a posix_spawn gave it.
//!
//! **The lifeline** is a socketpair whose launcher end rides the handoff. The launcher writes
//! `pid <n>` once the worker exists and `exit <raw wait status>` when it has reaped it, so the
//! supervisor keeps its exit codes (reboot, snapshot) without being the parent. When the
//! supervisor dies, its end closes and the launcher SIGKILLs the worker's process group: once
//! launchd is the parent, nothing else would ever end a VM that has lost its window.
//!
//! **Cleanup.** The supervisor deletes the plist once launchd has loaded it, and the launcher
//! unloads its own job as its last act. The supervisor cannot do it: it often leaves through
//! `process::exit`, which runs no destructor.
//!
//! The worker's listeners reach it the same way, without a path: see [`connect`].

#![allow(deprecated)] // libc deprecates mach_task_self in favour of the mach2 crate.

use std::ffi::{CString, OsStr, OsString, c_char, c_int};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

pub use libc::mach_port_t;

pub mod connect;

/// Labels (and Mach service names) of worker jobs: this prefix, the supervisor's pid, a dot and a
/// per-spawn counter. The test harness finds the launcher by it.
pub const JOB_PREFIX: &str = "eti.noronha.limina.worker.";

/// `limina-vmm`'s first argument when it runs as the launcher.
pub const LAUNCHER_FLAG: &str = "--launcher";

/// The target the lifeline travels under; every other target is an fd number in the worker.
const LIFELINE: i32 = -1;

/// The launcher parks received fds at or above this before putting them in place, so no target
/// can collide with one still waiting to be placed. Targets must stay below it.
const PARK_BASE: c_int = 1000;

/// `msgh_id` of the handoff message.
const HANDOFF_ID: i32 = 0x4c4c_4e43;

/// How long the launcher waits for the handoff after starting.
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(60);

// --- Mach / bootstrap / fileport FFI not covered by `libc` -----------------------------------------

const MACH_PORT_NULL: mach_port_t = 0;
const MACH_PORT_RIGHT_RECEIVE: u32 = 1;
const MACH_MSG_TYPE_COPY_SEND: u32 = 19;
#[cfg(test)]
const MACH_MSG_TYPE_MAKE_SEND: u32 = 20;
const MACH_MSG_PORT_DESCRIPTOR: u32 = 0;
const MACH_MSGH_BITS_COMPLEX: u32 = 0x8000_0000;
const MACH_SEND_MSG: i32 = 0x0000_0001;
const MACH_RCV_MSG: i32 = 0x0000_0002;
const MACH_SEND_TIMEOUT: i32 = 0x0000_0010;
const MACH_RCV_TIMEOUT: i32 = 0x0000_0100;
/// `MACH_RCV_TRAILER_ELEMENTS(MACH_RCV_TRAILER_AUDIT)`: append the sender's audit token.
const MACH_RCV_TRAILER_AUDIT: i32 = 3 << 24;
const MACH_RCV_TIMED_OUT: i32 = 0x1000_4003;

const HEADER_BYTES: usize = 24;
const BODY_BYTES: usize = 4;
const PORT_DESCRIPTOR_BYTES: usize = 12;
/// Offset of the sender's pid in a `mach_msg_audit_trailer_t`: type, size, seqno, the 8-byte
/// security token, then `audit_token_t.val[5]`.
const AUDIT_PID_OFFSET: usize = 4 + 4 + 4 + 8 + 5 * 4;
const AUDIT_TRAILER_BYTES: usize = 52;
/// Receive buffer: the environment is the bulk of a handoff and stays well under this.
const RECV_BUFFER_BYTES: usize = 512 * 1024;

unsafe extern "C" {
    static bootstrap_port: mach_port_t;
    fn bootstrap_look_up(bp: mach_port_t, name: *const c_char, sp: *mut mach_port_t) -> i32;
    fn bootstrap_check_in(bp: mach_port_t, name: *const c_char, sp: *mut mach_port_t) -> i32;
    #[cfg(test)]
    fn mach_port_allocate(task: mach_port_t, right: u32, name: *mut mach_port_t) -> i32;
    #[cfg(test)]
    fn mach_port_insert_right(
        task: mach_port_t,
        name: mach_port_t,
        poly: mach_port_t,
        poly_poly: u32,
    ) -> i32;
    fn mach_port_deallocate(task: mach_port_t, name: mach_port_t) -> i32;
    fn mach_port_mod_refs(task: mach_port_t, name: mach_port_t, right: u32, delta: i32) -> i32;
    fn mach_msg(
        msg: *mut u32,
        option: i32,
        send_size: u32,
        rcv_size: u32,
        rcv_name: mach_port_t,
        timeout: u32,
        notify: mach_port_t,
    ) -> i32;
    fn mach_msg_destroy(msg: *mut u32);
    fn fileport_makeport(fd: c_int, port: *mut mach_port_t) -> c_int;
    fn fileport_makefd(port: mach_port_t) -> c_int;
}

fn task() -> mach_port_t {
    // SAFETY: `mach_task_self()` returns this task's self port; always valid.
    unsafe { libc::mach_task_self() }
}

fn kr_err(what: &str, kr: i32) -> io::Error {
    io::Error::other(format!("{what}: mach/bootstrap error {kr} (0x{kr:x})"))
}

fn cstring(s: &str) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::other(format!("{s:?} contains a NUL")))
}

// --- The handoff payload ---------------------------------------------------------------------------

/// Everything the worker's posix_spawn would have carried except the fds themselves: which fd
/// number each passed fd takes (`targets`, in the order the ports travel; [`LIFELINE`] marks the
/// launcher's own), the program, argv without argv[0], the whole environment, and the cwd.
#[derive(Debug, Default, PartialEq)]
pub struct Handoff {
    pub targets: Vec<i32>,
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

impl Handoff {
    /// Length-prefixed fields, little-endian counts.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.targets.len() as u32).to_le_bytes());
        for t in &self.targets {
            out.extend_from_slice(&t.to_le_bytes());
        }
        put_bytes(&mut out, self.program.as_os_str().as_bytes());
        put_bytes(&mut out, self.cwd.as_os_str().as_bytes());
        out.extend_from_slice(&(self.args.len() as u32).to_le_bytes());
        for a in &self.args {
            put_bytes(&mut out, a.as_bytes());
        }
        out.extend_from_slice(&(self.env.len() as u32).to_le_bytes());
        for (k, v) in &self.env {
            put_bytes(&mut out, k.as_bytes());
            put_bytes(&mut out, v.as_bytes());
        }
        out
    }

    pub fn decode(mut buf: &[u8]) -> io::Result<Self> {
        let short = || io::Error::other("truncated handoff payload");
        fn u32_of(buf: &mut &[u8]) -> Option<u32> {
            let (head, rest) = buf.split_first_chunk::<4>()?;
            *buf = rest;
            Some(u32::from_le_bytes(*head))
        }
        fn bytes_of(buf: &mut &[u8]) -> Option<OsString> {
            let n = u32_of(buf)? as usize;
            if buf.len() < n {
                return None;
            }
            let (head, rest) = buf.split_at(n);
            *buf = rest;
            Some(OsString::from_vec(head.to_vec()))
        }
        let mut h = Handoff::default();
        let n = u32_of(&mut buf).ok_or_else(short)?;
        for _ in 0..n {
            h.targets.push(u32_of(&mut buf).ok_or_else(short)? as i32);
        }
        h.program = bytes_of(&mut buf).ok_or_else(short)?.into();
        h.cwd = bytes_of(&mut buf).ok_or_else(short)?.into();
        let n = u32_of(&mut buf).ok_or_else(short)?;
        for _ in 0..n {
            h.args.push(bytes_of(&mut buf).ok_or_else(short)?);
        }
        let n = u32_of(&mut buf).ok_or_else(short)?;
        for _ in 0..n {
            let k = bytes_of(&mut buf).ok_or_else(short)?;
            let v = bytes_of(&mut buf).ok_or_else(short)?;
            h.env.push((k, v));
        }
        Ok(h)
    }
}

// --- One Mach message: port descriptors, then the payload inline ------------------------------------

fn round4(n: usize) -> usize {
    (n + 3) & !3
}

/// Send `ports` (copied, so the caller still owns its rights) and `payload` to `dest`.
fn send_message(dest: mach_port_t, ports: &[mach_port_t], payload: &[u8]) -> io::Result<()> {
    let size =
        HEADER_BYTES + BODY_BYTES + ports.len() * PORT_DESCRIPTOR_BYTES + 4 + round4(payload.len());
    let mut bytes = Vec::with_capacity(size);
    let words = |v: &mut Vec<u8>, w: &[u32]| w.iter().for_each(|x| v.extend(x.to_ne_bytes()));
    words(
        &mut bytes,
        &[
            MACH_MSGH_BITS_COMPLEX | MACH_MSG_TYPE_COPY_SEND,
            size as u32,
            dest,
            MACH_PORT_NULL,
            MACH_PORT_NULL,
            HANDOFF_ID as u32,
            ports.len() as u32,
        ],
    );
    for &p in ports {
        // `mach_msg_port_descriptor_t`: name, pad1, then pad2:16 | disposition:8 | type:8.
        words(
            &mut bytes,
            &[
                p,
                0,
                (MACH_MSG_TYPE_COPY_SEND << 16) | (MACH_MSG_PORT_DESCRIPTOR << 24),
            ],
        );
    }
    words(&mut bytes, &[payload.len() as u32]);
    bytes.extend_from_slice(payload);
    bytes.resize(size, 0);
    let mut buf = vec![0u32; size / 4];
    // SAFETY: `buf` holds exactly `size` bytes; the copy only fixes the alignment mach_msg wants.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf.as_mut_ptr() as *mut u8, size) };
    // SAFETY: a well-formed complex message of `size` bytes; send only.
    let kr = unsafe {
        mach_msg(
            buf.as_mut_ptr(),
            MACH_SEND_MSG | MACH_SEND_TIMEOUT,
            size as u32,
            0,
            MACH_PORT_NULL,
            5000,
            MACH_PORT_NULL,
        )
    };
    if kr != 0 {
        return Err(kr_err("mach_msg(send handoff)", kr));
    }
    Ok(())
}

/// A received handoff message: the send rights it carried, its payload, and who sent it.
struct Received {
    ports: Vec<mach_port_t>,
    payload: Vec<u8>,
    sender_pid: libc::pid_t,
}

impl Received {
    fn release(self) {
        for p in self.ports {
            // SAFETY: a send right this task received and still owns.
            unsafe { mach_port_deallocate(task(), p) };
        }
    }
}

/// Receive one message on `port`, with the sender's audit token. `Ok(None)` on timeout.
fn receive_message(port: mach_port_t, timeout: Duration) -> io::Result<Option<Received>> {
    let mut buf = vec![0u32; RECV_BUFFER_BYTES / 4];
    let ms = timeout.as_millis().clamp(1, u32::MAX as u128) as u32;
    // SAFETY: `buf` is RECV_BUFFER_BYTES long; receive only.
    let kr = unsafe {
        mach_msg(
            buf.as_mut_ptr(),
            MACH_RCV_MSG | MACH_RCV_TIMEOUT | MACH_RCV_TRAILER_AUDIT,
            0,
            RECV_BUFFER_BYTES as u32,
            port,
            ms,
            MACH_PORT_NULL,
        )
    };
    if kr == MACH_RCV_TIMED_OUT {
        return Ok(None);
    }
    if kr != 0 {
        return Err(kr_err("mach_msg(receive handoff)", kr));
    }
    // SAFETY: the kernel wrote a message of `msgh_size` bytes plus the trailer into `buf`.
    let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, RECV_BUFFER_BYTES) };
    let word = |at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap());
    let size = word(4) as usize;
    let malformed = |why: &str| {
        // SAFETY: the message is still in `buf`; destroying it releases every right it carried.
        unsafe { mach_msg_destroy(buf.as_ptr() as *mut u32) };
        io::Error::other(format!("malformed handoff message: {why}"))
    };
    if size + AUDIT_TRAILER_BYTES > RECV_BUFFER_BYTES || size < HEADER_BYTES + BODY_BYTES {
        return Err(malformed("bad size"));
    }
    let sender_pid = word(round4(size) + AUDIT_PID_OFFSET) as libc::pid_t;
    if word(0) & MACH_MSGH_BITS_COMPLEX == 0 || word(20) as i32 != HANDOFF_ID {
        return Err(malformed("not a handoff"));
    }
    let count = word(HEADER_BYTES) as usize;
    let payload_at = HEADER_BYTES + BODY_BYTES + count * PORT_DESCRIPTOR_BYTES;
    if payload_at + 4 > size {
        return Err(malformed("descriptors overrun the message"));
    }
    let mut ports = Vec::with_capacity(count);
    for i in 0..count {
        let at = HEADER_BYTES + BODY_BYTES + i * PORT_DESCRIPTOR_BYTES;
        if (word(at + 8) >> 24) != MACH_MSG_PORT_DESCRIPTOR {
            return Err(malformed("a descriptor is not a port"));
        }
        ports.push(word(at));
    }
    let len = word(payload_at) as usize;
    if payload_at + 4 + len > size {
        return Err(malformed("payload overruns the message"));
    }
    let payload = bytes[payload_at + 4..payload_at + 4 + len].to_vec();
    Ok(Some(Received {
        ports,
        payload,
        sender_pid,
    }))
}

fn fileport_of(fd: RawFd) -> io::Result<mach_port_t> {
    let mut port = MACH_PORT_NULL;
    // SAFETY: `fd` is open in this process; on success we own a send right to its fileport.
    if unsafe { fileport_makeport(fd, &mut port) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(port)
}

fn fd_of(port: mach_port_t) -> io::Result<OwnedFd> {
    // SAFETY: `port` is a fileport send right this task owns; the new fd is ours.
    let fd = unsafe { fileport_makefd(port) };
    // SAFETY: we are done with the right either way.
    unsafe { mach_port_deallocate(task(), port) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

// --- The supervisor's side ---------------------------------------------------------------------------

/// Why [`launch`] gave up.
#[derive(Debug)]
pub enum LaunchError {
    /// Nothing was started (no gui domain, `launchctl` refused, the name did not resolve): the
    /// caller can posix_spawn instead.
    Unavailable(io::Error),
    /// The handoff was sent but no worker was reported: a launcher may exist, so starting a
    /// second worker is not safe.
    Failed(io::Error),
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LaunchError::Unavailable(e) => write!(f, "launchd path unavailable: {e}"),
            LaunchError::Failed(e) => write!(f, "launchd worker did not start: {e}"),
        }
    }
}

impl std::error::Error for LaunchError {}

/// What to start and with which fds. `fds` pairs a descriptor open in the supervisor with the
/// number it must have in the worker.
pub struct LaunchSpec<'a> {
    pub program: &'a Path,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
    pub fds: Vec<(RawFd, i32)>,
    /// Where the transient plist is written; removed once launchd has loaded it.
    pub plist_dir: &'a Path,
    /// How long to wait for the launcher to report the worker. The first exec of a freshly
    /// built binary can take a minute and more while it is scanned.
    pub start_timeout: Duration,
}

/// A worker started through launchd, seen through its launcher's lifeline.
pub struct Launched {
    label: String,
    pid: libc::pid_t,
    lifeline: UnixStream,
    pending: Vec<u8>,
    status: Option<ExitStatus>,
}

static SPAWNS: AtomicU32 = AtomicU32::new(0);

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn plist_for(label: &str, program: &Path, supervisor: u32) -> io::Result<String> {
    let program = program
        .to_str()
        .ok_or_else(|| io::Error::other(format!("{program:?} is not UTF-8")))?;
    let label = xml_escape(label);
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key><array>
    <string>{}</string><string>{LAUNCHER_FLAG}</string><string>{label}</string><string>{supervisor}</string>
  </array>
  <key>MachServices</key><dict><key>{label}</key><true/></dict>
  <key>ProcessType</key><string>Interactive</string>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><false/>
  <key>StandardInPath</key><string>/dev/null</string>
  <key>StandardOutPath</key><string>/dev/null</string>
  <key>StandardErrorPath</key><string>/dev/null</string>
</dict></plist>
"#,
        xml_escape(program)
    ))
}

/// The program as the job must name it. launchd starts a job from `/`, so a relative path is
/// resolved against the cwd the worker is given, which is where posix_spawn would have found it.
fn job_program(program: &Path, cwd: &Path) -> PathBuf {
    if program.is_relative() {
        cwd.join(program)
    } else {
        program.to_path_buf()
    }
}

fn gui_domain() -> String {
    // SAFETY: getuid cannot fail.
    format!("gui/{}", unsafe { libc::getuid() })
}

fn bootout(label: &str) {
    let _ = Command::new("/bin/launchctl")
        .arg("bootout")
        .arg(format!("{}/{label}", gui_domain()))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Load a launcher job for `spec` and wait until it reports the worker's pid.
pub fn launch(spec: LaunchSpec<'_>) -> Result<Launched, LaunchError> {
    use LaunchError::{Failed, Unavailable};
    for &(_, target) in &spec.fds {
        if !(0..PARK_BASE).contains(&target) {
            return Err(Unavailable(io::Error::other(format!(
                "fd target {target} is outside 0..{PARK_BASE}"
            ))));
        }
    }
    let supervisor = std::process::id();
    let program = job_program(spec.program, &spec.cwd);
    let label = format!(
        "{JOB_PREFIX}{supervisor}.{}",
        SPAWNS.fetch_add(1, Ordering::Relaxed)
    );

    let (lifeline, far_end) = UnixStream::pair().map_err(Unavailable)?;
    // The worker's fds must reach the launcher only through the handoff, never through
    // `launchctl`, which would otherwise inherit them and hold their peers open. The posix_spawn
    // fallback clears CLOEXEC again in its own child. Stdio stays as it is: other children of the
    // supervisor inherit it.
    for &(fd, _) in &spec.fds {
        if fd > 2 {
            // SAFETY: fcntl on a descriptor the caller owns.
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }

    std::fs::create_dir_all(spec.plist_dir).map_err(Unavailable)?;
    let plist = spec.plist_dir.join(format!("{label}.plist"));
    std::fs::write(
        &plist,
        plist_for(&label, &program, supervisor).map_err(Unavailable)?,
    )
    .map_err(Unavailable)?;
    let out = Command::new("/bin/launchctl")
        .arg("bootstrap")
        .arg(gui_domain())
        .arg(&plist)
        .stdin(std::process::Stdio::null())
        .output();
    // launchd has read it (or refused it); either way the file has done its job.
    let _ = std::fs::remove_file(&plist);
    match out {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            return Err(Unavailable(io::Error::other(format!(
                "launchctl bootstrap {}: {} {}",
                gui_domain(),
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            ))));
        }
        Err(e) => return Err(Unavailable(e)),
    }

    let send = || -> io::Result<()> {
        let name = cstring(&label)?;
        let mut service = MACH_PORT_NULL;
        // SAFETY: plain lookup in our bootstrap namespace.
        let kr = unsafe { bootstrap_look_up(bootstrap_port, name.as_ptr(), &mut service) };
        if kr != 0 {
            return Err(kr_err("bootstrap_look_up", kr));
        }
        let mut ports = Vec::new();
        let mut targets = Vec::new();
        let result = (|| {
            for &(fd, target) in &spec.fds {
                ports.push(fileport_of(fd)?);
                targets.push(target);
            }
            ports.push(fileport_of(far_end.as_raw_fd())?);
            targets.push(LIFELINE);
            let handoff = Handoff {
                targets,
                program: program.clone(),
                args: spec.args.clone(),
                env: spec.env.clone(),
                cwd: spec.cwd.clone(),
            };
            send_message(service, &ports, &handoff.encode())
        })();
        for p in ports {
            // SAFETY: send rights we made and copied into the message; ours to drop.
            unsafe { mach_port_deallocate(task(), p) };
        }
        // SAFETY: the looked-up send right is ours.
        unsafe { mach_port_deallocate(task(), service) };
        result
    };
    if let Err(e) = send() {
        bootout(&label);
        return Err(Unavailable(e));
    }
    drop(far_end);

    let mut launched = Launched {
        label,
        pid: 0,
        lifeline,
        pending: Vec::new(),
        status: None,
    };
    let deadline = Instant::now() + spec.start_timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match launched.next_line(Some(left)) {
            Ok(Some(line)) => {
                if let Some(pid) = line.strip_prefix("pid ").and_then(|p| p.parse().ok()) {
                    launched.pid = pid;
                    return Ok(launched);
                }
            }
            Ok(None) if left.is_zero() => {
                bootout(&launched.label);
                return Err(Failed(io::Error::other(format!(
                    "no worker reported within {:?}",
                    spec.start_timeout
                ))));
            }
            Ok(None) => {}
            Err(e) => {
                bootout(&launched.label);
                return Err(Failed(e));
            }
        }
    }
}

impl Launched {
    /// The worker's pid (the launcher's child, the leader of its own process group).
    pub fn id(&self) -> u32 {
        self.pid as u32
    }

    /// The job's label, for logs.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The next whole line from the lifeline. `Ok(None)` when `wait` elapses first; an EOF is
    /// an error. `None` for `wait` blocks.
    fn next_line(&mut self, wait: Option<Duration>) -> io::Result<Option<String>> {
        loop {
            if let Some(nl) = self.pending.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.pending.drain(..=nl).collect();
                return Ok(Some(String::from_utf8_lossy(&line).trim().to_string()));
            }
            let ms = match wait {
                None => -1,
                Some(d) => d.as_millis().min(i32::MAX as u128) as c_int,
            };
            let mut pfd = libc::pollfd {
                fd: self.lifeline.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd.
            let n = unsafe { libc::poll(&mut pfd, 1, ms) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if n == 0 {
                return Ok(None);
            }
            let mut chunk = [0u8; 256];
            match self.lifeline.read(&mut chunk) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the launcher closed its lifeline",
                    ));
                }
                Ok(k) => self.pending.extend_from_slice(&chunk[..k]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    fn poll_status(&mut self, wait: Option<Duration>) -> io::Result<Option<ExitStatus>> {
        if let Some(s) = self.status {
            return Ok(Some(s));
        }
        loop {
            match self.next_line(wait) {
                Ok(Some(line)) => {
                    if let Some(raw) = line.strip_prefix("exit ").and_then(|r| r.parse().ok()) {
                        self.status = Some(ExitStatus::from_raw(raw));
                        return Ok(self.status);
                    }
                }
                Ok(None) => return Ok(None),
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    // The launcher is gone without reporting; it only exits after reaping, so
                    // this is the launcher itself being killed. The worker went with it or is
                    // about to (the launcher's own group kill), so report it as killed.
                    self.status = Some(ExitStatus::from_raw(libc::SIGKILL));
                    return Ok(self.status);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Like `Child::try_wait`.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.poll_status(Some(Duration::ZERO))
    }

    /// Like `Child::wait`.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        loop {
            if let Some(s) = self.poll_status(None)? {
                return Ok(s);
            }
        }
    }

    /// Like `Child::kill`: SIGKILL the worker.
    pub fn kill(&mut self) -> io::Result<()> {
        if self.status.is_some() {
            return Ok(());
        }
        // SAFETY: a plain signal to a pid the launcher has not reported reaped.
        if unsafe { libc::kill(self.pid, libc::SIGKILL) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

// --- The launcher's side -----------------------------------------------------------------------------

fn raise_fd_limit() {
    // Above PARK_BASE, for the parked fds; the worker raises its own again.
    const TARGET: libc::rlim_t = 10240; // OPEN_MAX
    // SAFETY: plain rlimit calls.
    unsafe {
        let mut lim: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) == 0 && lim.rlim_cur < TARGET {
            lim.rlim_cur = TARGET.min(lim.rlim_max);
            let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
        }
    }
}

/// Wait for the supervisor's handoff on `label`, accepting it only from `supervisor`.
fn accept_handoff(label: &str, supervisor: libc::pid_t) -> io::Result<(Handoff, Vec<OwnedFd>)> {
    let name = cstring(label)?;
    let mut service = MACH_PORT_NULL;
    // SAFETY: checking in the MachServices name our own plist declared.
    let kr = unsafe { bootstrap_check_in(bootstrap_port, name.as_ptr(), &mut service) };
    if kr != 0 {
        return Err(kr_err("bootstrap_check_in", kr));
    }
    let deadline = Instant::now() + HANDOFF_TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::other("no handoff from the supervisor"));
        }
        let Some(msg) = receive_message(service, left)? else {
            continue;
        };
        // Any same-user process can look the name up; only the supervisor's message counts.
        if msg.sender_pid != supervisor {
            eprintln!(
                "limina-launcher: ignoring a handoff from pid {} (expected {supervisor})",
                msg.sender_pid
            );
            msg.release();
            continue;
        }
        let handoff = match Handoff::decode(&msg.payload) {
            Ok(h) if h.targets.len() == msg.ports.len() => h,
            Ok(_) => {
                msg.release();
                return Err(io::Error::other("handoff targets and ports disagree"));
            }
            Err(e) => {
                msg.release();
                return Err(e);
            }
        };
        let mut fds = Vec::with_capacity(msg.ports.len());
        for &p in &msg.ports {
            fds.push(fd_of(p)?);
        }
        // SAFETY: the receive right was ours; the handoff is the only message it carries.
        unsafe { mach_port_mod_refs(task(), service, MACH_PORT_RIGHT_RECEIVE, -1) };
        return Ok((handoff, fds));
    }
}

/// Move `fd` to the lowest free number at or above [`PARK_BASE`], close-on-exec.
fn park(fd: OwnedFd) -> io::Result<OwnedFd> {
    // SAFETY: fcntl on an fd we own; the result is a new fd we own.
    let parked = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, PARK_BASE) };
    if parked < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(parked) })
}

/// `limina-vmm --launcher <label> <supervisor pid>`: take the handoff, spawn the worker, report
/// its pid and exit status on the lifeline, and unload the job. Returns the launcher's exit code.
pub fn launcher_main(argv: &[OsString]) -> i32 {
    let (Some(label), Some(supervisor)) = (
        argv.get(2).and_then(|l| l.to_str()),
        argv.get(3)
            .and_then(|p| p.to_str())
            .and_then(|p| p.parse::<libc::pid_t>().ok()),
    ) else {
        eprintln!("usage: limina-vmm {LAUNCHER_FLAG} <label> <supervisor pid>");
        return 2;
    };
    let code = run_launcher(label, supervisor).unwrap_or_else(|e| {
        eprintln!("limina-launcher {label}: {e}");
        1
    });
    // Last act: the job is transient, and nothing else will unload it (see the module docs).
    bootout(label);
    code
}

fn run_launcher(label: &str, supervisor: libc::pid_t) -> io::Result<i32> {
    raise_fd_limit();
    let (handoff, fds) = accept_handoff(label, supervisor)?;
    let mut lifeline = None;
    let mut placed = Vec::new();
    for (fd, &target) in fds.into_iter().zip(&handoff.targets) {
        let fd = park(fd)?;
        if target == LIFELINE {
            lifeline = Some(UnixStream::from(fd));
        } else {
            placed.push((fd, target));
        }
    }
    let mut lifeline = lifeline.ok_or_else(|| io::Error::other("the handoff had no lifeline"))?;
    // Our own reports go where the worker's stderr goes.
    for (fd, target) in &placed {
        if *target == 1 || *target == 2 {
            // SAFETY: dup2 onto this process's stdout/stderr.
            unsafe { libc::dup2(fd.as_raw_fd(), *target) };
        }
    }

    let moves: Vec<(RawFd, c_int)> = placed.iter().map(|(fd, t)| (fd.as_raw_fd(), *t)).collect();
    let mut cmd = Command::new(&handoff.program);
    cmd.args(&handoff.args)
        .env_clear()
        .envs(
            handoff
                .env
                .iter()
                .map(|(k, v)| (k.as_os_str(), v.as_os_str())),
        )
        .current_dir(&handoff.cwd)
        .process_group(0);
    // SAFETY: only async-signal-safe dup2 calls between fork and exec. Every source is parked
    // above PARK_BASE and every target below it, so no move clobbers a pending source, and dup2
    // leaves the target without CLOEXEC.
    unsafe {
        cmd.pre_exec(move || {
            for &(from, to) in &moves {
                if libc::dup2(from, to) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| io::Error::other(format!("spawning {:?}: {e}", handoff.program)))?;
    drop(placed);
    let pid = child.id() as libc::pid_t;
    lifeline.write_all(format!("pid {pid}\n").as_bytes())?;

    // The supervisor's end closing means it is gone: end the VM rather than orphan it.
    let reaped = Arc::new(AtomicBool::new(false));
    {
        let reaped = reaped.clone();
        let mut watch = lifeline.try_clone()?;
        std::thread::spawn(move || {
            let mut sink = [0u8; 64];
            while matches!(watch.read(&mut sink), Ok(n) if n > 0) {}
            if !reaped.load(Ordering::Acquire) {
                eprintln!("limina-launcher: the supervisor is gone; killing worker {pid}");
                // SAFETY: the worker leads its own process group and has not been reaped.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
        });
    }

    let status = child.wait()?;
    reaped.store(true, Ordering::Release);
    let _ = lifeline.write_all(format!("exit {}\n", status.into_raw()).as_bytes());
    Ok(0)
}

/// Is this `limina-vmm` invocation the launcher?
pub fn is_launcher_invocation(first_arg: Option<&OsStr>) -> bool {
    first_arg == Some(OsStr::new(LAUNCHER_FLAG))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handoff_survives_encoding() {
        let h = Handoff {
            targets: vec![0, 1, 2, 7, LIFELINE],
            program: "/Applications/Limina.app/Contents/MacOS/limina-vmm".into(),
            args: vec![
                "--control-fd".into(),
                "7".into(),
                OsString::from_vec(vec![0xff]),
            ],
            env: vec![
                ("A".into(), "".into()),
                ("RUST_LOG".into(), "warn,x=info".into()),
            ],
            cwd: "/".into(),
        };
        assert_eq!(Handoff::decode(&h.encode()).unwrap(), h);
    }

    #[test]
    fn a_truncated_handoff_is_refused_not_misread() {
        let bytes = Handoff {
            targets: vec![3],
            program: "/p".into(),
            ..Default::default()
        }
        .encode();
        for cut in 0..bytes.len() {
            assert!(Handoff::decode(&bytes[..cut]).is_err(), "cut at {cut}");
        }
    }

    /// The whole Mach leg without launchd: an fd rides a fileport through a real message, and
    /// the audit trailer names the sender.
    #[test]
    fn an_fd_crosses_a_mach_message_and_the_sender_is_known() {
        let t = task();
        let mut port = MACH_PORT_NULL;
        // SAFETY: allocate a receive right and a send right to it, both ours.
        unsafe {
            assert_eq!(mach_port_allocate(t, MACH_PORT_RIGHT_RECEIVE, &mut port), 0);
            assert_eq!(
                mach_port_insert_right(t, port, port, MACH_MSG_TYPE_MAKE_SEND),
                0
            );
        }
        let (mut near, far) = UnixStream::pair().unwrap();
        let fileport = fileport_of(far.as_raw_fd()).unwrap();
        drop(far);
        let payload = Handoff {
            targets: vec![9],
            program: "/bin/true".into(),
            ..Default::default()
        }
        .encode();
        send_message(port, &[fileport], &payload).unwrap();
        // SAFETY: our copy of the fileport right.
        unsafe { mach_port_deallocate(t, fileport) };

        let msg = receive_message(port, Duration::from_secs(1))
            .unwrap()
            .expect("the message");
        assert_eq!(msg.sender_pid, std::process::id() as libc::pid_t);
        assert_eq!(Handoff::decode(&msg.payload).unwrap().targets, vec![9]);
        assert_eq!(msg.ports.len(), 1);
        let fd = fd_of(msg.ports[0]).unwrap();
        let mut crossed = UnixStream::from(fd);
        crossed.write_all(b"hi").unwrap();
        let mut got = [0u8; 2];
        near.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hi");
        // SAFETY: tear down the receive right (and with it our send right's target).
        unsafe {
            mach_port_mod_refs(t, port, MACH_PORT_RIGHT_RECEIVE, -1);
            mach_port_deallocate(t, port);
        }
    }

    #[test]
    fn nothing_arrives_means_a_timeout_not_an_error() {
        let t = task();
        let mut port = MACH_PORT_NULL;
        // SAFETY: a receive right of our own.
        unsafe { assert_eq!(mach_port_allocate(t, MACH_PORT_RIGHT_RECEIVE, &mut port), 0) };
        assert!(
            receive_message(port, Duration::from_millis(20))
                .unwrap()
                .is_none()
        );
        // SAFETY: ours to drop.
        unsafe { mach_port_mod_refs(t, port, MACH_PORT_RIGHT_RECEIVE, -1) };
    }

    /// launchd starts a job from `/`, so a relative program (`--vmm-bin target/debug/limina-vmm`,
    /// as `cargo xtask run` passes it) must be resolved where posix_spawn would have resolved it:
    /// against the cwd the worker is given.
    #[test]
    fn a_relative_program_is_resolved_against_the_cwd() {
        assert_eq!(
            job_program(
                Path::new("target/debug/limina-vmm"),
                Path::new("/src/limina")
            ),
            Path::new("/src/limina/target/debug/limina-vmm")
        );
        assert_eq!(
            job_program(
                Path::new("/Applications/L.app/limina-vmm"),
                Path::new("/src")
            ),
            Path::new("/Applications/L.app/limina-vmm")
        );
    }

    #[test]
    fn the_plist_escapes_what_it_embeds() {
        let p = plist_for("a&b", Path::new("/x/<y>/limina-vmm"), 42).unwrap();
        assert!(p.contains("<string>a&amp;b</string>"));
        assert!(p.contains("/x/&lt;y&gt;/limina-vmm"));
        assert!(p.contains("<key>ProcessType</key><string>Interactive</string>"));
    }
}
