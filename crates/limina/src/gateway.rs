// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! gvproxy gateway lifecycle for user-mode NAT (M3).
//!
//! gvproxy ([gvisor-tap-vsock]) is a userspace network gateway: DHCP, DNS and NAT to the
//! host network with no root and no entitlement. We spawn it listening on a vfkit-style
//! UNIX *datagram* socket; the worker connects libkrun's virtio-net backend to that socket
//! (the `--net-gvproxy` flag → `UnixgramPath(_, vfkit=true)`). See `spikes/m3-gvproxy`.
//!
//! The gateway must be up before the guest activates its NIC; [`start`] waits for the
//! socket to appear before returning. Teardown is handled two ways because the supervisor
//! exits via `process::exit` on both the headless and windowed paths (which skips `Drop`):
//! [`Gateway`]'s `Drop` covers the headless path (explicit drop before exit), and the
//! idempotent module-global [`cleanup`] covers the windowed timer's emergency exit. Both
//! are safe to call more than once.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use std::os::unix::process::CommandExt;

/// Default Homebrew location; overridable with `LIMINA_GVPROXY_BIN`, else found on `PATH`.
const DEFAULT_GVPROXY: &str = "/opt/homebrew/bin/gvproxy";

/// The running gateway's pid (0 = none), for the windowed emergency-exit cleanup path.
static GVPROXY_PID: AtomicI32 = AtomicI32::new(0);
/// The gateway's socket path, so [`cleanup`] can remove it without a `Gateway` handle.
static GVPROXY_SOCK: Mutex<Option<PathBuf>> = Mutex::new(None);

/// A running gvproxy gateway. Drop tears it down (kill + reap + remove the socket).
pub struct Gateway {
    socket_path: PathBuf,
}

impl Gateway {
    /// The vfkit unixgram socket path to hand the worker via `--net-gvproxy`.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        cleanup();
    }
}

/// Resolve the gvproxy binary: `$LIMINA_GVPROXY_BIN`, else the Homebrew path if present,
/// else `gvproxy` from `PATH`.
fn gvproxy_bin() -> PathBuf {
    if let Ok(p) = std::env::var("LIMINA_GVPROXY_BIN") {
        return PathBuf::from(p);
    }
    let brew = PathBuf::from(DEFAULT_GVPROXY);
    if brew.exists() {
        return brew;
    }
    PathBuf::from("gvproxy")
}

/// Spawn gvproxy and wait for its socket to be ready.
///
/// `debug_log`, if set, runs gvproxy with `-debug` and redirects its output there (the
/// host-side network oracle — DHCP/DNS/NAT packets — used by tests and diagnostics).
/// Without it, gvproxy logs at info to the supervisor's inherited stderr.
pub fn start(debug_log: Option<&Path>) -> Result<Gateway> {
    // A unique, ABSOLUTE socket path. Absolute is required: gvproxy parses
    // `unixgram://host/path`, so a relative path's first component is mistaken for the
    // URL host (`bind: no such file or directory`). See spikes/m3-gvproxy/RESULTS.md.
    let socket_path =
        std::env::temp_dir().join(format!("limina-gvproxy-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(local_bind_path(&socket_path));

    let bin = gvproxy_bin();
    let mut cmd = Command::new(&bin);
    // Own process group so a terminal Ctrl-C (SIGINT to the foreground group) doesn't kill
    // gvproxy out from under the guest before we drive an orderly shutdown.
    cmd.process_group(0);
    if let Some(log) = debug_log {
        let f =
            std::fs::File::create(log).with_context(|| format!("creating gvproxy log {log:?}"))?;
        let f2 = f.try_clone().context("cloning gvproxy log handle")?;
        cmd.arg("-debug").stdout(f).stderr(f2);
    }
    cmd.arg("-listen-vfkit")
        .arg(format!("unixgram://{}", socket_path.display()));

    let child = cmd.spawn().with_context(|| {
        format!("spawning gvproxy ({bin:?}); set LIMINA_GVPROXY_BIN if not installed")
    })?;
    let pid = child.id() as i32;
    GVPROXY_PID.store(pid, Ordering::SeqCst);
    *GVPROXY_SOCK.lock().unwrap() = Some(socket_path.clone());
    // We track the gateway via the pid (for the global cleanup path); reaping is handled by
    // cleanup()'s waitpid, so we deliberately drop the Child handle here.
    drop_child_keep_pid(child);

    wait_for_socket(&socket_path, pid, Duration::from_secs(5))?;
    log::info!("gvproxy gateway up (pid {pid}, socket {socket_path:?})");
    Ok(Gateway { socket_path })
}

/// Kill and reap the gateway and remove its socket. Idempotent and safe to call from any
/// exit path (headless `Drop` or the windowed timer's `process::exit`).
pub fn cleanup() {
    let pid = GVPROXY_PID.swap(0, Ordering::SeqCst);
    if pid > 0 {
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        // Brief reap window; escalate to SIGKILL if it lingers, then reap to avoid a zombie.
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            let mut status = 0;
            let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if r == pid || r < 0 {
                break;
            }
            if Instant::now() >= deadline {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    libc::waitpid(pid, &mut status, 0);
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    if let Some(sock) = GVPROXY_SOCK.lock().unwrap().take() {
        let _ = std::fs::remove_file(local_bind_path(&sock));
        let _ = std::fs::remove_file(&sock);
    }
}

/// libkrun binds its own local datagram address next to the peer socket (`<path>-krun.sock`,
/// see net/unixgram.rs); clean it up too.
fn local_bind_path(socket_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}-krun.sock", socket_path.display()))
}

/// Poll until `socket_path` is a bound socket, erroring if gvproxy dies or it never appears.
fn wait_for_socket(socket_path: &Path, pid: i32, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if socket_path.exists() {
            return Ok(());
        }
        // Did gvproxy die already? (e.g. bad args / port in use)
        let mut status = 0;
        if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } == pid {
            GVPROXY_PID.store(0, Ordering::SeqCst);
            bail!("gvproxy exited before creating its socket (check the gateway log)");
        }
        if Instant::now() >= deadline {
            bail!("gvproxy did not create {socket_path:?} within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Forget the `Child` handle without reaping (cleanup()'s waitpid does the reaping). This
/// keeps the child running; we manage its lifetime entirely through the pid global.
fn drop_child_keep_pid(child: Child) {
    // Leak the Child so its Drop (which does nothing on Unix anyway) can't close anything we
    // still rely on; the process keeps running and is reaped by cleanup().
    std::mem::forget(child);
}
