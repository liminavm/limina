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
//! socket to appear before returning. Teardown is layered three ways:
//!
//!  1. **Clean exit** — the supervisor exits via `process::exit` on both the headless and
//!     windowed paths (which skips `Drop`): [`Gateway`]'s `Drop` covers the headless path
//!     (explicit drop before exit), and the idempotent module-global [`cleanup`] covers the
//!     windowed timer's emergency exit + the SIGINT/SIGTERM-driven orderly stop. Both are
//!     safe to call more than once.
//!  2. **Immediate death-pact** (the un-catchable cases — SIGKILL, panic/abort, OOM — where no
//!     in-process handler can run) — alongside gvproxy we spawn a tiny re-exec'd watcher
//!     (`limina __reap-gateway`) holding the read end of a pipe whose write end the supervisor
//!     keeps open. When the supervisor dies by ANY means the kernel closes that write end, the
//!     watcher reads EOF and reaps gvproxy within milliseconds. macOS has no `PR_SET_PDEATHSIG`;
//!     this is the stand-in. See [`spawn_death_pact_watcher`] / [`run_reaper`].
//!  3. **Startup sweep** (the backstop, for power loss or the watcher itself dying) — each
//!     gateway registers a pid-file under [`registry_dir`] keyed by the *supervisor* pid; the
//!     next `limina --net` launch runs [`sweep_orphans`], which reaps any gvproxy whose owning
//!     supervisor is **gone**. It only touches strays of *dead* owners, so a concurrently-
//!     running VM's gvproxy is never killed — what makes running two or more VMs at once safe.
//!
//! Multiple concurrent VMs: the socket path is keyed on the supervisor pid (unique per
//! `limina` process), so two VMs never collide on it. The one remaining shared host resource —
//! gvproxy's built-in SSH-forward port — is made distinct per VM either explicitly (the
//! supervisor's `--ssh-port`) or, when that is omitted, by auto-allocating the first free port
//! from [`DEFAULT_SSH_PORT`] upward (see [`allocate_ssh_port`]) so two VMs that both leave it
//! unset still don't fight over 2222.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use std::os::unix::process::CommandExt;

/// Default Homebrew location; overridable with `LIMINA_GVPROXY_BIN`, else found on `PATH`.
const DEFAULT_GVPROXY: &str = "/opt/homebrew/bin/gvproxy";

/// gvproxy's own default host port for its built-in `127.0.0.1:<port> → <guest .2>:22`
/// forward. Each VM may override it (`--ssh-port`) so multiple VMs don't fight over one port.
pub const DEFAULT_SSH_PORT: u16 = 2222;

/// gvproxy's accepted SSH-forward port range (see `gvproxy --help`).
pub const SSH_PORT_MIN: u16 = 1024;

/// `argv[1]` sentinel selecting the death-pact reaper mode (see [`run_reaper`]). Spawned by
/// [`spawn_death_pact_watcher`]; handled in `main` before clap/logging/AppKit.
pub const REAP_GATEWAY_ARG: &str = "__reap-gateway";

/// The running gateway's pid (0 = none), for the windowed emergency-exit cleanup path.
static GVPROXY_PID: AtomicI32 = AtomicI32::new(0);
/// The gateway's socket path, so [`cleanup`] can remove it without a `Gateway` handle.
static GVPROXY_SOCK: Mutex<Option<PathBuf>> = Mutex::new(None);
/// The death-pact watcher's pid (0 = none) — the re-exec'd `limina __reap-gateway` child that
/// reaps gvproxy if this supervisor dies un-cleanly. Killed first by [`cleanup`].
static WATCHER_PID: AtomicI32 = AtomicI32::new(0);
/// Write end of the death-pact pipe (-1 = none). Held open for the VM's life; its closure on
/// ANY supervisor death is the EOF the watcher detects.
static DEATHPACT_WFD: AtomicI32 = AtomicI32::new(-1);

/// A running gvproxy gateway. Drop tears it down (kill + reap + remove the socket).
pub struct Gateway {
    socket_path: PathBuf,
    debug_log: Option<PathBuf>,
    ssh_port: u16,
    /// Custom guest MAC (normalized lowercase) — rebinds the static .2 lease via a
    /// generated gvproxy config file. None = gvproxy's built-in well-known-MAC config.
    guest_mac: Option<String>,
}

impl Gateway {
    /// The vfkit unixgram socket path to hand the worker via `--net-gvproxy`.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// The resolved host port of gvproxy's inbound `127.0.0.1:<port> → guest:22` forward — the
    /// one the caller requested, or the auto-allocated one when `--ssh-port` was omitted. The
    /// supervisor surfaces it so the user knows where to `ssh`.
    pub fn ssh_port(&self) -> u16 {
        self.ssh_port
    }

    /// Recycle gvproxy at the **same** socket path, so the worker's already-baked
    /// `--net-gvproxy` arg stays valid. Needed when the supervisor relaunches the worker after a
    /// guest reboot: gvproxy's vfkit socket is single-connection (it unlinks the socket once a VM
    /// connects — see libkrun `net/unixgram.rs`), so a fresh worker cannot reconnect to the old
    /// gvproxy. The socket path is keyed on the supervisor pid, so a restart reuses it.
    pub fn restart(&self) -> Result<()> {
        cleanup();
        spawn_gvproxy(
            &self.socket_path,
            self.debug_log.as_deref(),
            self.ssh_port,
            self.guest_mac.as_deref(),
        )
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        cleanup();
    }
}

/// Resolve the gvproxy binary, in priority order: `$LIMINA_GVPROXY_BIN`, then a copy vendored
/// next to the supervisor in the app bundle (`Contents/MacOS/gvproxy` — what makes `--net` work
/// on a Mac with no Homebrew), then the Homebrew path if present, else bare `gvproxy` from `PATH`.
fn gvproxy_bin() -> PathBuf {
    let env_override = std::env::var_os("LIMINA_GVPROXY_BIN").map(PathBuf::from);
    let brew = {
        let p = PathBuf::from(DEFAULT_GVPROXY);
        p.exists().then_some(p)
    };
    resolve_gvproxy_bin(env_override, bundled_gvproxy(), brew)
}

/// gvproxy vendored alongside the supervisor binary (`Contents/MacOS/gvproxy` in the app
/// bundle), if it exists. This is what lets `limina --net` run on a Mac without Homebrew —
/// `scripts/build-app.sh` copies gvproxy in next to `limina`.
fn bundled_gvproxy() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let cand = exe.parent()?.join("gvproxy");
    cand.exists().then_some(cand)
}

/// Pure gvproxy-resolution policy (unit-tested): explicit override > bundled-in-app > Homebrew >
/// bare `gvproxy` on `PATH`. Bundled beats Homebrew so a self-contained app prefers its own
/// vendored copy over whatever a dev machine happens to have on `PATH`/in `/opt/homebrew`.
fn resolve_gvproxy_bin(
    env_override: Option<PathBuf>,
    bundled: Option<PathBuf>,
    brew: Option<PathBuf>,
) -> PathBuf {
    env_override
        .or(bundled)
        .or(brew)
        .unwrap_or_else(|| PathBuf::from("gvproxy"))
}

/// This supervisor's gvproxy socket path. Keyed on the supervisor pid: unique per `limina`
/// process (so two concurrent VMs never collide), and stable across a gvproxy restart.
fn default_socket_path() -> PathBuf {
    std::env::temp_dir().join(format!("limina-gvproxy-{}.sock", std::process::id()))
}

/// Spawn gvproxy and wait for its socket to be ready, with `ssh_port` for the built-in SSH
/// forward (distinct per VM lets several VMs run at once).
///
/// `debug_log`, if set, runs gvproxy with `-debug` and redirects its output there (the
/// host-side network oracle — DHCP/DNS/NAT packets — used by tests and diagnostics).
/// Without it, gvproxy logs at info to the supervisor's inherited stderr.
///
/// `ssh_port`: an explicit host port for gvproxy's inbound SSH forward, or `None` to
/// auto-allocate the first free port from [`DEFAULT_SSH_PORT`] upward (so two VMs that both omit
/// `--ssh-port` don't collide). The resolved port is readable via [`Gateway::ssh_port`].
///
/// `guest_mac`: a custom guest MAC (managed VMs pass their persistent per-VM MAC) — the
/// gateway then runs gvproxy with a generated config file that rebinds the static .2 lease
/// (and the SSH forward, which moves into the config) to that MAC. `None` keeps gvproxy's
/// built-in well-known-vfkit-MAC behavior.
///
/// First reaps any gvproxy orphaned by a previously-crashed supervisor (see [`sweep_orphans`]).
pub fn start(
    debug_log: Option<&Path>,
    ssh_port: Option<u16>,
    guest_mac: Option<&str>,
) -> Result<Gateway> {
    // Reap leftovers from a supervisor that died un-cleanly before it could run its own
    // teardown. Only strays whose owning supervisor is GONE are killed, so a concurrent VM's
    // gvproxy is untouched.
    sweep_orphans(&registry_dir());

    let socket_path = default_socket_path();
    // The free-port probe is inherently TOCTOU (see `port_is_free`): between our scan and
    // gvproxy binding the forward, a concurrently-starting VM can grab the same port — rare
    // when VMs start seconds apart, routine when a parallel test runner spawns several at
    // once. An explicitly `--ssh-port`ed VM still fails fast (the user asked for THAT port);
    // an auto-allocated one retries the scan from just above the lost port.
    let requested = ssh_port;
    let mut scan_base = DEFAULT_SSH_PORT;
    let mut last_err: Option<anyhow::Error> = None;
    for _ in 0..GVPROXY_PORT_RETRIES {
        let ssh_port = match requested {
            Some(_) => allocate_ssh_port(requested)?,
            None => first_free_port(scan_base, port_is_free)
                .with_context(|| format!("no free host port at or above {scan_base}"))?,
        };
        match spawn_gvproxy(&socket_path, debug_log, ssh_port, guest_mac) {
            Ok(()) => {
                return Ok(Gateway {
                    socket_path,
                    debug_log: debug_log.map(Path::to_path_buf),
                    ssh_port,
                    guest_mac: guest_mac.map(str::to_string),
                });
            }
            Err(e) if requested.is_none() => {
                log::warn!(
                    "gvproxy did not come up on auto-allocated port {ssh_port} \
                     (lost a port race?): {e:#}; retrying above it"
                );
                scan_base = ssh_port.saturating_add(1);
                last_err = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("gvproxy failed to start")))
}

/// How many auto-allocated ports we try before giving up on starting gvproxy. Each retry
/// rescans from above the port that was lost, so distinct attempts probe distinct ports.
const GVPROXY_PORT_RETRIES: usize = 5;

/// Spawn (or respawn) gvproxy listening on `socket_path`, recording its pid/socket in the
/// statics used by [`cleanup`] and a pid-file in the registry used by [`sweep_orphans`].
/// Shared by [`start`] and [`Gateway::restart`].
fn spawn_gvproxy(
    socket_path: &Path,
    debug_log: Option<&Path>,
    ssh_port: u16,
    guest_mac: Option<&str>,
) -> Result<()> {
    let _ = fs::remove_file(socket_path);
    let _ = fs::remove_file(local_bind_path(socket_path));

    let child = spawn_gvproxy_process(socket_path, debug_log, ssh_port, guest_mac)?;
    let pid = child.id() as i32;
    GVPROXY_PID.store(pid, Ordering::SeqCst);
    *GVPROXY_SOCK.lock().unwrap() = Some(socket_path.to_path_buf());
    // Register (owner = our pid → gvproxy pid + socket) so a later supervisor can reap this
    // gvproxy if WE die un-cleanly. Best-effort: a missing record only forfeits the sweep.
    write_record(&registry_dir(), std::process::id() as i32, pid, socket_path);
    // Tie gvproxy to THIS supervisor with an EOF death-pact so it dies immediately if we are
    // killed un-cleanly (the sweep above is only the next-launch backstop).
    spawn_death_pact_watcher(pid);
    // We track the gateway via the pid (for the global cleanup path); reaping is handled by
    // cleanup()'s waitpid, so we deliberately drop the Child handle here.
    drop_child_keep_pid(child);

    log::info!("gvproxy gateway up (pid {pid}, socket {socket_path:?}, ssh-port {ssh_port})");
    Ok(())
}

/// Spawn gvproxy and wait for its socket, returning the live [`Child`] without touching any
/// global state. Factored out so tests can stand up more than one instance at once (the
/// "two VMs coexist" check) and so the arg construction stays pure ([`gvproxy_args`]).
fn spawn_gvproxy_process(
    socket_path: &Path,
    debug_log: Option<&Path>,
    ssh_port: u16,
    guest_mac: Option<&str>,
) -> Result<Child> {
    // A custom guest MAC needs a gvproxy config file: the static .2 lease is only
    // configurable there, and once `-config` is in play `-ssh-port` is ignored — so the
    // SSH forward moves into the file too (see `gvproxy_config_yaml`).
    let config_path = match guest_mac {
        Some(mac) => {
            let p = socket_path.with_extension("yaml");
            fs::write(&p, gvproxy_config_yaml(mac, ssh_port))
                .with_context(|| format!("writing gvproxy config {p:?}"))?;
            Some(p)
        }
        None => None,
    };
    let bin = gvproxy_bin();
    let mut cmd = Command::new(&bin);
    // Own process group so a terminal Ctrl-C (SIGINT to the foreground group) doesn't kill
    // gvproxy out from under the guest before we drive an orderly shutdown.
    cmd.process_group(0);
    if let Some(log) = debug_log {
        let f = fs::File::create(log).with_context(|| format!("creating gvproxy log {log:?}"))?;
        let f2 = f.try_clone().context("cloning gvproxy log handle")?;
        cmd.stdout(f).stderr(f2);
    }
    cmd.args(gvproxy_args(
        socket_path,
        ssh_port,
        debug_log.is_some(),
        config_path.as_deref(),
    ));

    let child = cmd.spawn().with_context(|| {
        format!("spawning gvproxy ({bin:?}); set LIMINA_GVPROXY_BIN if not installed")
    })?;
    let pid = child.id() as i32;
    wait_for_socket(socket_path, pid, Duration::from_secs(5))?;
    Ok(child)
}

/// Build gvproxy's argv (pure — unit-tested). `-listen-vfkit` must take an ABSOLUTE
/// `unixgram://` URL (gvproxy parses `unixgram://host/path`, so a relative path's first
/// component is mistaken for the URL host → `bind: no such file or directory`). `-ssh-port`
/// sets the host port for gvproxy's built-in `127.0.0.1:<port> → <guest .2>:22` forward —
/// the knob that lets multiple VMs run at once without fighting over one host port.
/// With a `config` file, `-ssh-port` is ignored by gvproxy (warned, not honored) — the
/// forward lives in the file instead, so the flag is omitted to keep the argv honest.
fn gvproxy_args(
    socket_path: &Path,
    ssh_port: u16,
    debug: bool,
    config: Option<&Path>,
) -> Vec<OsString> {
    let mut v: Vec<OsString> = Vec::new();
    if debug {
        v.push("-debug".into());
    }
    v.push("-listen-vfkit".into());
    v.push(OsString::from(format!(
        "unixgram://{}",
        socket_path.display()
    )));
    match config {
        Some(c) => {
            v.push("-config".into());
            v.push(c.as_os_str().to_os_string());
        }
        None => {
            v.push("-ssh-port".into());
            v.push(OsString::from(ssh_port.to_string()));
        }
    }
    v
}

/// The gvproxy config for a custom guest MAC (pure — unit-tested). gvproxy defaults
/// subnet/gateway/NAT/mtu even in config mode, but forwards, static leases and the DNS
/// zones only exist in its no-config path — so this reproduces the stock vfkit config
/// (upstream `cmd/gvproxy/config.yaml`) with the .2 lease bound to `mac` and the SSH
/// forward at `ssh_port`.
fn gvproxy_config_yaml(mac: &str, ssh_port: u16) -> String {
    format!(
        r#"stack:
    dhcpStaticLeases:
        192.168.127.2: {mac}
    forwards:
        127.0.0.1:{ssh_port}: 192.168.127.2:22
    dns:
        - name: containers.internal.
          records:
            - name: gateway
              ip: 192.168.127.1
            - name: host
              ip: 192.168.127.254
        - name: docker.internal.
          records:
            - name: gateway
              ip: 192.168.127.1
            - name: host
              ip: 192.168.127.254
"#
    )
}

// ---------------------------------------------------------------------------
// Host SSH-forward port allocation — pick a free port when the VM didn't request one, so several
// VMs can run in parallel without the caller hand-picking non-colliding ports.
// ---------------------------------------------------------------------------

/// True if `port` can be bound on loopback right now (nothing else holds it). A transient probe:
/// we bind then immediately drop the listener so gvproxy can take the port. Inherently TOCTOU,
/// but VMs start seconds apart and gvproxy binds its forward within ms, so in practice the next
/// VM's scan already sees this one's port held.
fn port_is_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// First free port at or above `base` (scanning up to 65535) per the injected `is_free` probe.
/// Pure (probe injected) so the scan policy is unit-tested without binding real sockets.
fn first_free_port(base: u16, is_free: impl Fn(u16) -> bool) -> Option<u16> {
    (base..=u16::MAX).find(|&p| is_free(p))
}

/// Resolve a gateway's host SSH-forward port: honor an explicit `requested` port (erroring early
/// if it is already in use, so the user gets a clear message instead of a silently-dead forward),
/// or auto-allocate the first free port from [`DEFAULT_SSH_PORT`] upward when `None` — what lets
/// two VMs that both omit `--ssh-port` run in parallel without colliding on 2222.
fn allocate_ssh_port(requested: Option<u16>) -> Result<u16> {
    match requested {
        Some(p) if port_is_free(p) => Ok(p),
        Some(p) => bail!(
            "host port {p} is already in use; pick another --ssh-port or omit it to auto-allocate"
        ),
        None => first_free_port(DEFAULT_SSH_PORT, port_is_free)
            .with_context(|| format!("no free host port at or above {DEFAULT_SSH_PORT}")),
    }
}

/// Kill and reap the death-pact watcher + the gateway, and remove its socket + registry
/// record. Idempotent and safe to call from any exit path (headless `Drop`, the windowed
/// timer's `process::exit`, the SIGINT/SIGTERM-driven orderly stop).
pub fn cleanup() {
    // Stand the death-pact watcher down FIRST, so it can't fire on a reused pid after we reap
    // our gvproxy. On a clean exit the watcher therefore never acts; only an un-cleanly-killed
    // supervisor (which never reaches cleanup) lets it do its job.
    kill_watcher();
    let pid = GVPROXY_PID.swap(0, Ordering::SeqCst);
    if pid > 0 {
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        // Brief reap window; escalate to SIGKILL if it lingers, then reap to avoid a zombie.
        // (gvproxy is OUR child here, so we waitpid it.)
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
        let _ = fs::remove_file(local_bind_path(&sock));
        // The generated config for a custom guest MAC, if one was written.
        let _ = fs::remove_file(sock.with_extension("yaml"));
        let _ = fs::remove_file(&sock);
    }
    // Drop our registry record so a future sweep doesn't consider us an orphan.
    let _ = fs::remove_file(record_path(&registry_dir(), std::process::id() as i32));
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

// ---------------------------------------------------------------------------
// Death-pact — tie gvproxy's life to the supervisor's so it dies immediately if we are killed
// un-cleanly (the macOS stand-in for PR_SET_PDEATHSIG, which Darwin lacks).
// ---------------------------------------------------------------------------

/// Set (or best-effort skip) `FD_CLOEXEC` on `fd`.
fn set_cloexec(fd: libc::c_int) {
    unsafe {
        let f = libc::fcntl(fd, libc::F_GETFD);
        if f >= 0 {
            libc::fcntl(fd, libc::F_SETFD, f | libc::FD_CLOEXEC);
        }
    }
}

/// Spawn the death-pact watcher tying `gvproxy_pid` to this supervisor. Create a pipe, keep the
/// write end here (CLOEXEC, so neither the worker nor gvproxy inherits it — *only* the
/// supervisor holds it), and hand the read end to a re-exec'd `limina __reap-gateway` child. On
/// ANY supervisor death the kernel closes the write end and the watcher (a separate, reparented
/// process) sees EOF and reaps gvproxy. Best-effort: any failure logs and falls back to the
/// startup [`sweep_orphans`]; we never fail the VM over a missing watcher.
fn spawn_death_pact_watcher(gvproxy_pid: i32) {
    // Retire any previous generation's watcher (e.g. across a reboot/restart).
    kill_watcher();

    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        log::warn!(
            "death-pact pipe() failed: {}; relying on the startup sweep",
            std::io::Error::last_os_error()
        );
        return;
    }
    let (rfd, wfd) = (fds[0], fds[1]);
    // Both CLOEXEC by default so nothing inherits them accidentally; the watcher re-opens rfd
    // by clearing CLOEXEC just in its own forked child (pre_exec below).
    set_cloexec(rfd);
    set_cloexec(wfd);

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            log::warn!("death-pact current_exe failed: {e}; relying on the startup sweep");
            unsafe {
                libc::close(rfd);
                libc::close(wfd);
            }
            return;
        }
    };
    let mut cmd = Command::new(exe);
    cmd.arg(REAP_GATEWAY_ARG)
        .arg(rfd.to_string())
        .arg(gvproxy_pid.to_string());
    // Own process group so a terminal Ctrl-C doesn't kill the watcher before it can act.
    cmd.process_group(0);
    // SAFETY: only an async-signal-safe fcntl between fork and exec — clear CLOEXEC on rfd so
    // the watcher inherits the read end (wfd stays CLOEXEC, so the watcher does NOT inherit it).
    unsafe {
        cmd.pre_exec(move || {
            let f = libc::fcntl(rfd, libc::F_GETFD);
            if f < 0 || libc::fcntl(rfd, libc::F_SETFD, f & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    match cmd.spawn() {
        Ok(child) => {
            let wpid = child.id() as i32;
            WATCHER_PID.store(wpid, Ordering::SeqCst);
            DEATHPACT_WFD.store(wfd, Ordering::SeqCst);
            drop_child_keep_pid(child);
            log::info!(
                "death-pact watcher up (pid {wpid}) tying gvproxy {gvproxy_pid} to this supervisor"
            );
        }
        Err(e) => {
            log::warn!("death-pact watcher spawn failed: {e}; relying on the startup sweep");
            unsafe { libc::close(wfd) };
        }
    }
    // The supervisor never reads the pipe; only the watcher does. Drop our read-end copy so the
    // watcher's is the only one — otherwise EOF could not fire on our death.
    unsafe { libc::close(rfd) };
}

/// Kill + reap the death-pact watcher (our child) and close the death-pact write end. Called
/// first by [`cleanup`] and before spawning a fresh watcher.
fn kill_watcher() {
    let wpid = WATCHER_PID.swap(0, Ordering::SeqCst);
    if wpid > 0 {
        unsafe { libc::kill(wpid, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_millis(300);
        loop {
            let mut status = 0;
            let r = unsafe { libc::waitpid(wpid, &mut status, libc::WNOHANG) };
            if r == wpid || r < 0 {
                break;
            }
            if Instant::now() >= deadline {
                unsafe {
                    libc::kill(wpid, libc::SIGKILL);
                    libc::waitpid(wpid, &mut status, 0);
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let wfd = DEATHPACT_WFD.swap(-1, Ordering::SeqCst);
    if wfd >= 0 {
        unsafe { libc::close(wfd) };
    }
}

/// Death-pact reaper entry point — the hidden `limina __reap-gateway <read_fd> <gvproxy_pid>`
/// mode, dispatched from `main` before clap/logging/AppKit. Watches the inherited pipe read end
/// and reaps gvproxy on EOF (supervisor death). Never returns. See [`spawn_death_pact_watcher`].
pub fn run_reaper() -> ! {
    let mut it = std::env::args().skip(2);
    let read_fd: i32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(-1);
    let gvproxy_pid: i32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(-1);
    if read_fd < 0 || gvproxy_pid <= 0 {
        std::process::exit(2);
    }
    watch_and_reap(read_fd, gvproxy_pid);
    std::process::exit(0);
}

/// Block on `read_fd` until EOF (the supervisor's write end closed ⇒ it died), then reap
/// `gvproxy_pid` if it is still a gvproxy (identity-checked, to dodge pid reuse). Factored out
/// of [`run_reaper`] so the EOF→reap mechanism is exercised end-to-end by `tests/death_pact.rs`.
fn watch_and_reap(read_fd: i32, gvproxy_pid: i32) {
    let mut buf = [0u8; 64];
    loop {
        let n = unsafe { libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n == 0 {
            break; // EOF: the supervisor is gone.
        }
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break; // broken pipe / error: treat as supervisor-gone.
        }
        // n > 0: we don't use the pipe as a data channel — keep waiting for EOF.
    }
    unsafe { libc::close(read_fd) };
    if process_is_gvproxy(gvproxy_pid) {
        reap_foreign(gvproxy_pid);
    }
}

// ---------------------------------------------------------------------------
// Orphan registry + sweep — reaping gvproxy left behind by an un-cleanly-killed supervisor.
// ---------------------------------------------------------------------------

/// Directory of gateway pid-files, one per supervisor (`<owner_pid>.gw`). In the temp dir so
/// a reboot of the host clears it; created best-effort.
fn registry_dir() -> PathBuf {
    let d = std::env::temp_dir().join("limina-gateways");
    let _ = fs::create_dir_all(&d);
    d
}

/// Path of a supervisor's gateway record. Keyed on the OWNER (supervisor) pid so a sweep can
/// tell, from the filename alone, whose gvproxy it is — and whether that owner still lives.
fn record_path(dir: &Path, owner_pid: i32) -> PathBuf {
    dir.join(format!("{owner_pid}.gw"))
}

/// Write `<gvproxy_pid>\n<socket>\n` to the owner's record. Best-effort.
fn write_record(dir: &Path, owner_pid: i32, gvproxy_pid: i32, socket: &Path) {
    let _ = fs::create_dir_all(dir);
    let write = || -> std::io::Result<()> {
        let mut f = fs::File::create(record_path(dir, owner_pid))?;
        writeln!(f, "{gvproxy_pid}")?;
        writeln!(f, "{}", socket.display())?;
        Ok(())
    };
    let _ = write();
}

/// Parse a record file into `(gvproxy_pid, socket)`.
fn read_record(path: &Path) -> (Option<i32>, Option<PathBuf>) {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return (None, None),
    };
    let mut lines = content.lines();
    let pid = lines.next().and_then(|s| s.trim().parse::<i32>().ok());
    let sock = lines.next().map(PathBuf::from);
    (pid, sock)
}

/// Reap gvproxy processes whose owning supervisor is gone, and clear their records/sockets.
///
/// Safety for concurrent VMs: a gvproxy is reaped only when its owner pid is no longer alive
/// AND the target pid still resolves to a gvproxy executable (guarding against pid reuse). A
/// live (possibly concurrent) supervisor's gvproxy is always left alone.
fn sweep_orphans(dir: &Path) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(OsStr::to_str) != Some("gw") {
            continue;
        }
        let owner = match path
            .file_stem()
            .and_then(OsStr::to_str)
            .and_then(|s| s.parse::<i32>().ok())
        {
            Some(o) => o,
            None => continue,
        };
        // A live owner (this process, or a concurrently-running VM) keeps its gvproxy.
        if !is_orphan(owner, pid_is_alive) {
            continue;
        }
        let (gpid, socket) = read_record(&path);
        if let Some(gpid) = gpid {
            if gpid > 0 && process_is_gvproxy(gpid) {
                reap_foreign(gpid);
                log::info!("swept orphaned gvproxy (pid {gpid}) from dead supervisor {owner}");
            }
        }
        if let Some(sock) = socket {
            let _ = fs::remove_file(local_bind_path(&sock));
            let _ = fs::remove_file(&sock);
        }
        let _ = fs::remove_file(&path);
    }
}

/// Pure decision used by [`sweep_orphans`]: a gateway record is an orphan to reap iff its
/// owning supervisor is no longer alive. A live owner — this process, or a concurrently-running
/// VM — keeps its gvproxy. Split out (with an injectable liveness probe) so the
/// concurrency-safety invariant that makes running multiple VMs at once safe is unit-testable.
fn is_orphan(owner_pid: i32, is_alive: impl Fn(i32) -> bool) -> bool {
    !is_alive(owner_pid)
}

/// True if `pid` exists. `kill(pid, 0)` returns 0 if we may signal it, `EPERM` if it exists
/// but we may not, `ESRCH` if it's gone.
fn pid_is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// True if `pid`'s executable basename looks like gvproxy — guards the sweep against killing
/// an unrelated process that happens to have reused a dead gvproxy's pid.
fn process_is_gvproxy(pid: i32) -> bool {
    let mut buf = [0u8; 4096];
    let n =
        unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
    if n <= 0 {
        return false;
    }
    let path = OsStr::from_bytes(&buf[..n as usize]);
    Path::new(path)
        .file_name()
        .and_then(OsStr::to_str)
        .map(|s| s.contains("gvproxy"))
        .unwrap_or(false)
}

/// SIGTERM→(≤500ms)→SIGKILL a process that is NOT our child (a reparented orphan), so we
/// cannot `waitpid` it — its death is confirmed by probing with signal 0 (init reaps it).
fn reap_foreign(pid: i32) {
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_millis(500);
    while pid_is_alive(pid) {
        if Instant::now() >= deadline {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Pure, always-run -------------------------------------------------

    #[test]
    fn ssh_port_and_listen_are_in_the_argv() {
        let args = gvproxy_args(Path::new("/tmp/x.sock"), 2345, false, None);
        let sp = args
            .iter()
            .position(|a| a == "-ssh-port")
            .expect("-ssh-port present");
        assert_eq!(args[sp + 1], OsString::from("2345"));
        let lp = args
            .iter()
            .position(|a| a == "-listen-vfkit")
            .expect("-listen-vfkit present");
        // Absolute unixgram URL (relative would be mis-parsed as a host).
        assert_eq!(args[lp + 1], OsString::from("unixgram:///tmp/x.sock"));
        assert!(
            !args.iter().any(|a| a == "-debug"),
            "no -debug without a log"
        );
    }

    #[test]
    fn debug_flag_added_only_when_logging() {
        assert!(gvproxy_args(Path::new("/x"), 2222, true, None)
            .iter()
            .any(|a| a == "-debug"));
    }

    #[test]
    fn config_mode_swaps_ssh_port_for_config() {
        // With `-config`, gvproxy ignores `-ssh-port` (the forward lives in the file), so
        // the argv must carry `-config` and NOT `-ssh-port`.
        let args = gvproxy_args(
            Path::new("/tmp/x.sock"),
            2345,
            false,
            Some(Path::new("/c.yaml")),
        );
        let cp = args
            .iter()
            .position(|a| a == "-config")
            .expect("-config present");
        assert_eq!(args[cp + 1], OsString::from("/c.yaml"));
        assert!(!args.iter().any(|a| a == "-ssh-port"));
    }

    #[test]
    fn config_yaml_binds_lease_and_forward() {
        let y = gvproxy_config_yaml("aa:bb:cc:dd:ee:ff", 2345);
        // The .2 static lease is rebound to the custom MAC…
        assert!(y.contains("192.168.127.2: aa:bb:cc:dd:ee:ff"), "{y}");
        // …and the SSH forward (unavailable as a flag in config mode) is in the file.
        assert!(y.contains("127.0.0.1:2345: 192.168.127.2:22"), "{y}");
        // The DNS zones gvproxy only installs in no-config mode are reproduced.
        assert!(y.contains("containers.internal."), "{y}");
    }

    #[test]
    fn only_dead_owners_are_orphans() {
        // A dead owner (crashed supervisor) is an orphan to reap; a live owner (this process,
        // or a concurrently-running VM) is NOT — the invariant that keeps multi-VM safe.
        let alive = |p: i32| p == 111;
        assert!(
            is_orphan(222, alive),
            "a dead owner's gvproxy must be swept"
        );
        assert!(
            !is_orphan(111, alive),
            "a live owner's gvproxy must never be swept (would kill a running VM)"
        );
    }

    #[test]
    fn gvproxy_resolution_order_is_override_then_bundled_then_brew() {
        let ovr = PathBuf::from("/env/gvproxy");
        let bundled = PathBuf::from("/app/Contents/MacOS/gvproxy");
        let brew = PathBuf::from(DEFAULT_GVPROXY);
        // An explicit override beats everything.
        assert_eq!(
            resolve_gvproxy_bin(Some(ovr.clone()), Some(bundled.clone()), Some(brew.clone())),
            ovr
        );
        // No override: the bundled copy wins over Homebrew, so a self-contained app on a
        // Homebrew-less Mac runs its own vendored gvproxy.
        assert_eq!(
            resolve_gvproxy_bin(None, Some(bundled.clone()), Some(brew.clone())),
            bundled
        );
        // No override, nothing bundled (a bare dev build): fall back to Homebrew.
        assert_eq!(resolve_gvproxy_bin(None, None, Some(brew.clone())), brew);
        // Nothing resolvable anywhere: bare name, left for PATH lookup at spawn time.
        assert_eq!(
            resolve_gvproxy_bin(None, None, None),
            PathBuf::from("gvproxy")
        );
    }

    #[test]
    fn first_free_port_returns_the_first_unused_from_the_base() {
        // Auto-allocation scans UP from the base, taking the first free port — so the first VM
        // gets the familiar 2222 and each later VM the next slot, with no manual port-picking.
        assert_eq!(
            first_free_port(2222, |_| true),
            Some(2222),
            "all free → base"
        );
        assert_eq!(
            first_free_port(2222, |p| p != 2222),
            Some(2223),
            "base taken → next"
        );
        assert_eq!(
            first_free_port(2222, |p| p >= 2224),
            Some(2224),
            "base + next taken → skip both"
        );
        assert_eq!(
            first_free_port(65535, |_| false),
            None,
            "nothing free in range → None"
        );
    }

    // --- Real-process, gated on LIMINA_HVF_TESTS (run via scripts/test-boot.sh) ----------

    fn gated() -> bool {
        std::env::var_os("LIMINA_HVF_TESTS").is_some()
    }

    fn unique_tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).expect("mktmp");
        d
    }

    /// A pid that is definitely dead (spawn `true`, reap it, reuse its pid).
    fn a_dead_pid() -> i32 {
        let mut c = Command::new("true").spawn().expect("spawn true");
        let pid = c.id() as i32;
        let _ = c.wait();
        pid
    }

    /// Launch gvproxy ORPHANED (reparented to init) to mimic a crashed supervisor's leftover:
    /// a child `sh` backgrounds gvproxy and exits, so gvproxy's parent becomes init. Returns
    /// gvproxy's pid (read from its own `-pid-file`).
    fn launch_orphan_gvproxy(dir: &Path, socket: &Path, ssh_port: u16) -> i32 {
        let pidfile = dir.join(format!("gv-{ssh_port}.pid"));
        let _ = fs::remove_file(&pidfile);
        let bin = gvproxy_bin();
        let mut launcher = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "{} -listen-vfkit unixgram://{} -ssh-port {} -pid-file {} &",
                bin.display(),
                socket.display(),
                ssh_port,
                pidfile.display()
            ))
            .spawn()
            .expect("launch orphan gvproxy");
        let _ = launcher.wait();
        // gvproxy writes its pid-file shortly after start.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(s) = fs::read_to_string(&pidfile) {
                if let Ok(pid) = s.trim().parse::<i32>() {
                    if pid > 0 && pid_is_alive(pid) {
                        return pid;
                    }
                }
            }
            assert!(Instant::now() < deadline, "orphan gvproxy never came up");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn sweep_reaps_an_orphaned_gvproxy() {
        if !gated() {
            eprintln!("skipping sweep_reaps_an_orphaned_gvproxy (set LIMINA_HVF_TESTS=1)");
            return;
        }
        let dir = unique_tmp_dir("limina-gw-sweep");
        let sock = dir.join("gw.sock");
        let gpid = launch_orphan_gvproxy(&dir, &sock, DEFAULT_SSH_PORT);
        assert!(
            process_is_gvproxy(gpid),
            "identity check recognizes gvproxy"
        );
        // The supervisor that owned it has "crashed" — record under a dead owner pid.
        write_record(&dir, a_dead_pid(), gpid, &sock);

        sweep_orphans(&dir);

        assert!(
            !pid_is_alive(gpid),
            "an orphaned gvproxy (dead supervisor) must be reaped by the sweep"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_spares_a_live_owners_gvproxy() {
        if !gated() {
            eprintln!("skipping sweep_spares_a_live_owners_gvproxy (set LIMINA_HVF_TESTS=1)");
            return;
        }
        let dir = unique_tmp_dir("limina-gw-live");
        let sock = dir.join("gw.sock");
        let gpid = launch_orphan_gvproxy(&dir, &sock, DEFAULT_SSH_PORT);
        // Owner = OUR pid (alive) — the sweep must NOT touch a live VM's gvproxy.
        write_record(&dir, std::process::id() as i32, gpid, &sock);

        sweep_orphans(&dir);

        let still = pid_is_alive(gpid);
        unsafe { libc::kill(gpid, libc::SIGKILL) }; // cleanup regardless
        let _ = fs::remove_dir_all(&dir);
        assert!(still, "a live supervisor's gvproxy must survive the sweep");
    }

    #[test]
    fn two_gateways_coexist_on_distinct_ssh_ports() {
        if !gated() {
            eprintln!(
                "skipping two_gateways_coexist_on_distinct_ssh_ports (set LIMINA_HVF_TESTS=1)"
            );
            return;
        }
        let dir = unique_tmp_dir("limina-gw-coexist");
        // Two VMs = two gvproxy on distinct sockets AND distinct ssh-ports (the resource that
        // used to collide). Both must come up and stay up.
        let a = spawn_gvproxy_process(&dir.join("a.sock"), None, 2222, None).expect("gateway A");
        let b = spawn_gvproxy_process(&dir.join("b.sock"), None, 2223, None).expect("gateway B");
        let (pa, pb) = (a.id() as i32, b.id() as i32);
        let both = pid_is_alive(pa) && pid_is_alive(pb);
        // Reap our children.
        for mut c in [a, b] {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = fs::remove_dir_all(&dir);
        assert!(
            both,
            "two VMs' gateways must run at once on distinct ssh-ports"
        );
    }

    #[test]
    fn two_auto_allocated_gateways_get_distinct_ports() {
        if !gated() {
            eprintln!(
                "skipping two_auto_allocated_gateways_get_distinct_ports (set LIMINA_HVF_TESTS=1)"
            );
            return;
        }
        let dir = unique_tmp_dir("limina-gw-autoport");
        // VM A: no `--ssh-port` → auto-allocate the first free port from the base.
        let pa = allocate_ssh_port(None).expect("allocate port for A");
        let a = spawn_gvproxy_process(&dir.join("a.sock"), None, pa, None).expect("gateway A");
        // gvproxy binds its TCP ssh-port at startup; wait until A actually holds `pa` so B's scan
        // is forced to skip it (in real use VMs start seconds apart, but the test serializes).
        let deadline = Instant::now() + Duration::from_secs(5);
        while port_is_free(pa) {
            assert!(
                Instant::now() < deadline,
                "gvproxy A never bound its ssh-port {pa}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        // VM B: auto-allocate again → must NOT reuse the port A is holding.
        let pb = allocate_ssh_port(None).expect("allocate port for B");
        let b = spawn_gvproxy_process(&dir.join("b.sock"), None, pb, None).expect("gateway B");
        let (pida, pidb) = (a.id() as i32, b.id() as i32);
        let both = pid_is_alive(pida) && pid_is_alive(pidb);
        for mut c in [a, b] {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = fs::remove_dir_all(&dir);
        assert_ne!(
            pa, pb,
            "auto-allocation must give the second VM a different port (got {pa} then {pb})"
        );
        assert!(both, "both auto-allocated gateways must run at once");
    }
}
