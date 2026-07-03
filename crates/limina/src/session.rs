// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Windowed VM session lifecycle: spawn the worker in `--display-window` mode, wire its
//! scanout/input/ack channels to the native window, and relaunch it across guest reboots
//! while the same NSWindow stays open.
//!
//! Split out of `main.rs` (2026-07-01 review, Part I recommendation 4): this spawn/relaunch
//! machinery is exactly what the future M9 `--restore` path must reuse — a session that can
//! (re)spawn a worker and retarget the live window's connection onto it.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::supervisor::{self, WorkerSpec};
use crate::window;
use crate::{control, gateway};

/// Everything a windowed session needs to boot and supervise its worker: the worker binary and
/// its base args (*without* the windowed fd flags — those are added per spawn, with fresh fd
/// numbers), the display geometry, and the supervisor-owned resources the session drives (NAT
/// gateway, control plane, resize socket, keymap policy). Groups what used to be
/// `run_windowed`'s nine positional arguments.
pub struct SessionConfig {
    pub vmm_bin: PathBuf,
    pub base_args: Vec<String>,
    pub grace: Duration,
    pub width: u32,
    pub height: u32,
    pub gateway: Option<gateway::Gateway>,
    pub control: Option<control::ControlPlane>,
    pub resize_socket: Option<PathBuf>,
    pub remap: limina_input::keymap::KeyRemap,
    /// Window title — the managed VM's name, or "Limina" for ephemeral flat-CLI VMs.
    pub title: String,
    /// Display mode policy: host (match the window's screen), dynamic (guest follows the
    /// window — the original behavior), or a fixed resolution. See `vmlib::schema`.
    pub mode: crate::vmlib::schema::DisplayResolution,
    /// Where to persist window state (the bundle's state.toml); None = no persistence.
    pub state_path: Option<PathBuf>,
    /// Remembered NSWindow frame to restore (screen points, Cocoa bottom-left origin).
    pub restore_frame: Option<[f64; 4]>,
}

/// Pack/unpack a `(width, height)` for the [`AtomicU64`] the window and the reboot-relaunch
/// monitor share: the window records every resolution it drives the guest to, so a worker
/// relaunched across a guest reboot boots at the *current* size, not the original one. (In
/// host/fixed mode the window no longer follows the guest, so a stale relaunch size would
/// sit letterboxed forever instead of being rescued by `setContentSize`.)
pub fn pack_size(width: u32, height: u32) -> u64 {
    (u64::from(width) << 32) | u64::from(height)
}

pub fn unpack_size(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, packed as u32)
}

/// One windowed worker plus the supervisor-side fds wiring the window to it. Re-created on a
/// guest reboot ([`spawn_windowed_worker`]); the new [`window::WorkerIo`] set is swapped into
/// the window's [`window::WorkerConn`] so the same NSWindow keeps showing whichever worker is
/// current.
struct WindowedWorker {
    child: std::process::Child,
    /// Worker→supervisor scanout control channel; handed to `spawn_reader`, which owns it and
    /// closes it on EOF.
    sup: OwnedFd,
    /// pid + supervisor→worker evdev input ends (the window writes here) + the shown-ack fd
    /// (a dup of `sup`): the owned, swappable per-worker set. Its fds close when the last
    /// snapshot of the set drops ([`window::WorkerConn::swap`]) — no manual close on relaunch,
    /// and no leak on an error path.
    io: window::WorkerIo,
}

/// Spawn a worker in `--display-window` mode and wire fresh scanout/input/ack socketpairs to
/// it. `base_args` are the worker args *without* the windowed fd flags (added here, with the
/// new fd numbers), so this can be called again to relaunch after a guest reboot.
fn spawn_windowed_worker(
    vmm_bin: &std::path::Path,
    base_args: &[String],
    grace: Duration,
    width: u32,
    height: u32,
    surface_port_name: Option<&str>,
) -> Result<WindowedWorker> {
    // Control channel (worker→supervisor scanout): sup stays here (CLOEXEC so the worker
    // can't inherit it); worker_fd is inherited and referenced by --control-fd. Stream type
    // since it carries newline-delimited text. All ends are OwnedFd from creation, so any
    // error path out of this function (including a failed spawn below) closes every end on
    // drop — the old raw-int version leaked all eight if `spawn_worker` failed.
    let (sup, worker_fd) = socketpair(libc::SOCK_STREAM)?;
    // Input channels (supervisor→worker evdev events): one datagram per event, so SOCK_DGRAM
    // preserves the 8-byte record boundaries the worker's backends rely on.
    let (kbd_sup, kbd_worker_fd) = socketpair(libc::SOCK_DGRAM)?;
    let (ptr_sup, ptr_worker_fd) = socketpair(libc::SOCK_DGRAM)?;
    // The relative-pointer (mouse) device for pointer-capture mode (M8) — same datagram model.
    let (rel_ptr_sup, rel_ptr_worker_fd) = socketpair(libc::SOCK_DGRAM)?;
    // Input events are tiny (8 bytes) but bursty (a key chord, a fast drag). Give the
    // datagram pipes a deep buffer (~32k events) so a momentary worker lag never drops
    // input, and make the supervisor *send* ends non-blocking so a pathological full
    // socket (a stalled/dead worker input thread) can never block the AppKit main thread
    // and freeze the whole UI — send() returns EAGAIN and we drop instead (see input.rs).
    for fd in [
        &kbd_sup,
        &kbd_worker_fd,
        &ptr_sup,
        &ptr_worker_fd,
        &rel_ptr_sup,
        &rel_ptr_worker_fd,
    ] {
        set_socket_buffer(fd.as_raw_fd(), 256 * 1024);
    }
    set_nonblocking(kbd_sup.as_raw_fd());
    set_nonblocking(ptr_sup.as_raw_fd());
    set_nonblocking(rel_ptr_sup.as_raw_fd());

    let mut args = base_args.to_vec();
    args.push("--display-window".into());
    args.push("--control-fd".into());
    args.push(worker_fd.as_raw_fd().to_string());
    args.push("--display-size".into());
    args.push(format!("{width}x{height}"));
    args.push("--input-kbd-fd".into());
    args.push(kbd_worker_fd.as_raw_fd().to_string());
    args.push("--input-ptr-fd".into());
    args.push(ptr_worker_fd.as_raw_fd().to_string());
    args.push("--input-rel-ptr-fd".into());
    args.push(rel_ptr_worker_fd.as_raw_fd().to_string());
    if let Some(name) = surface_port_name {
        // Scoped scanouts: the worker hands its (non-global) IOSurfaces to our surface-port
        // receiver by Mach port instead of making them globally lookup-able.
        args.push("--surface-port-name".into());
        args.push(name.to_string());
    }

    let spec = WorkerSpec {
        vmm_bin: vmm_bin.to_path_buf(),
        args,
        shutdown_grace: grace,
    };
    let child = supervisor::spawn_worker(
        &spec,
        &[
            worker_fd.as_raw_fd(),
            kbd_worker_fd.as_raw_fd(),
            ptr_worker_fd.as_raw_fd(),
            rel_ptr_worker_fd.as_raw_fd(),
        ],
    )?;
    let pid = child.id() as i32;
    // The worker holds its own copies now; drop ours so it sees EOF / owns the read ends
    // (OwnedFd drop closes them).
    drop(worker_fd);
    drop(kbd_worker_fd);
    drop(ptr_worker_fd);
    drop(rel_ptr_worker_fd);
    // Shown-ack write half (#8 leg 2): a dup of the control socketpair end. NOTE: a dup
    // shares the open file description — setting O_NONBLOCK here would make the reader
    // thread's blocking reads on `sup` fail too. The ack writes use MSG_DONTWAIT per send.
    // (`try_clone` also sets FD_CLOEXEC, so a *relaunched* worker no longer inherits the
    // previous worker's ack fd, which the raw `libc::dup` used to leak into it.)
    let ack = sup.try_clone().context("dup'ing the shown-ack fd")?;
    Ok(WindowedWorker {
        child,
        sup,
        io: window::WorkerIo::new(pid, kbd_sup, ptr_sup, rel_ptr_sup, ack),
    })
}

/// A live windowed VM session: the swappable connection to the current worker, the shared
/// present state, and the surface-port rendezvous — everything `run_windowed`'s locals used
/// to hold. [`WindowedSession::start`] spawns the first worker and the monitor thread that
/// relaunches it across guest reboots; [`WindowedSession::run`] hands the session to the
/// AppKit window on the main thread.
pub struct WindowedSession {
    conn: Arc<window::WorkerConn>,
    shared: Arc<Mutex<window::Shared>>,
    surface_map: window::SurfaceMap,
    control: Option<control::ControlPlane>,
    resize_socket: Option<PathBuf>,
    remap: limina_input::keymap::KeyRemap,
    title: String,
    mode: crate::vmlib::schema::DisplayResolution,
    state_path: Option<PathBuf>,
    restore_frame: Option<[f64; 4]>,
    initial_size: (u32, u32),
    desired_size: Arc<AtomicU64>,
}

impl WindowedSession {
    /// Spawn the first worker, wire its scanout/input/ack channels into a swappable conn, and
    /// start the background monitor that relaunches the worker across guest reboots.
    pub fn start(config: SessionConfig) -> Result<Self> {
        let SessionConfig {
            vmm_bin,
            base_args,
            grace,
            width,
            height,
            gateway,
            control,
            resize_socket,
            remap,
            title,
            mode,
            state_path,
            restore_frame,
        } = config;

        // The resolution the guest is currently driven to — written by the window on every
        // resize push, read at each reboot relaunch so the fresh worker boots at it.
        let desired_size = Arc::new(AtomicU64::new(pack_size(width, height)));

        // Capability-scope the scanout IOSurfaces: register a Mach surface-port receiver the
        // worker hands its NON-global scanouts to (so strangers can't IOSurfaceLookup the guest
        // screen). On failure, fall back to global surfaces (worker detects no receiver) so the
        // VM still runs.
        let (surface_port_name, surface_map) = match window::surface_rendezvous() {
            Ok((name, map)) => (Some(name), map),
            Err(e) => {
                log::error!(
                    "surface-port rendezvous failed ({e}); scanout IOSurfaces will be GLOBAL"
                );
                (None, window::empty_surface_map())
            }
        };

        // Spawn the first worker and wire its scanout/input/ack channels into a swappable conn.
        let WindowedWorker { child, sup, io } = spawn_windowed_worker(
            &vmm_bin,
            &base_args,
            grace,
            width,
            height,
            surface_port_name.as_deref(),
        )?;
        let conn = window::WorkerConn::new(io);
        let shared = window::Shared::new();
        window::spawn_reader(sup, shared.clone());

        // Monitor the worker on a background thread. A guest *reboot* (distinct exit code)
        // relaunches the worker — recycle gvproxy, spawn a fresh worker, swap its fds into
        // `conn`, start a new reader — keeping the SAME NSWindow open. Any other exit
        // (power-off, error, Ctrl-C, or a boot loop) marks the worker gone so the window loop
        // quits. The gateway handle lives in this thread (it owns gvproxy's restart) for the
        // VM's life.
        let monitor_shared = shared.clone();
        let monitor_conn = conn.clone();
        let monitor_control = control.clone();
        let monitor_port_name = surface_port_name.clone();
        let monitor_desired = desired_size.clone();
        std::thread::spawn(move || {
            let mut child = child;
            let mut guard = supervisor::RebootGuard::new();
            loop {
                let started = std::time::Instant::now();
                let code = supervisor::monitor(child, grace, monitor_control.as_ref()).unwrap_or(0);
                if !guard.should_relaunch(code, started.elapsed()) {
                    window::mark_worker_exited(&monitor_shared);
                    break;
                }
                // Recycle gvproxy (single-connection vfkit socket) before the fresh worker dials it.
                if let Some(gw) = &gateway {
                    if let Err(e) = gw.restart() {
                        log::error!(
                            "could not restart the NAT gateway for the reboot: {e:#}; stopping"
                        );
                        window::mark_worker_exited(&monitor_shared);
                        break;
                    }
                }
                // Boot the fresh worker at the resolution the window is CURRENTLY driving,
                // not the original one (see `pack_size`).
                let (width, height) = unpack_size(monitor_desired.load(Ordering::Relaxed));
                let next = match spawn_windowed_worker(
                    &vmm_bin,
                    &base_args,
                    grace,
                    width,
                    height,
                    monitor_port_name.as_deref(),
                ) {
                    Ok(n) => n,
                    Err(e) => {
                        log::error!("relaunch after guest reboot failed: {e:#}; stopping");
                        window::mark_worker_exited(&monitor_shared);
                        break;
                    }
                };
                // Publish the new worker's whole endpoint set atomically; this retires the old
                // WorkerIo, whose OwnedFds close when the last snapshot of it drops. A concurrent
                // input send on the main thread holds such a snapshot, which keeps the old fds
                // open (and their numbers un-reusable) for the send's duration — so the swap can
                // never race a send onto a closed or recycled fd, and there is no manual close
                // loop. (The scanout `sup` fd is separate: its reader thread owns it and closes
                // it on EOF.)
                let pid = next.io.pid();
                monitor_conn.swap(next.io);
                window::spawn_reader(next.sup, monitor_shared.clone());
                log::info!("guest rebooted → relaunched the windowed VM worker (pid {pid})");
                child = next.child;
            }
        });

        Ok(Self {
            conn,
            shared,
            surface_map,
            control,
            resize_socket,
            remap,
            title,
            mode,
            state_path,
            restore_frame,
            initial_size: (width, height),
            desired_size,
        })
    }

    /// Runs the AppKit window on this (main) thread; never returns — on quit it asks the guest
    /// agent to power off (orderly), falling back to killing the current worker's process group
    /// (`conn.pid()`). The window captures NSEvents and writes evdev events to the current
    /// worker's input fds (read from `conn`). `resize_socket` lets the resize gesture push the
    /// new window size to the worker (which forwards it to the live virtio-gpu).
    pub fn run(self, mtm: objc2::MainThreadMarker) -> ! {
        window::run(
            self.shared,
            mtm,
            self.conn,
            self.control,
            self.surface_map,
            window::WindowOptions {
                resize_socket: self.resize_socket,
                remap: self.remap,
                title: self.title,
                mode: self.mode,
                initial_size: self.initial_size,
                restore_frame: self.restore_frame,
                state_path: self.state_path,
                desired_size: self.desired_size,
            },
        );
    }
}

/// Windowed run: socketpair control channel, spawn the worker in `--display-window` mode
/// (inheriting the worker end), then run the AppKit window on the main thread while a
/// background thread monitors the worker. Never returns (exits with the worker's code).
pub fn run_windowed(config: SessionConfig) -> Result<()> {
    use objc2::MainThreadMarker;

    let mtm = MainThreadMarker::new().context("the window must run on the main thread")?;
    let session = WindowedSession::start(config)?;
    session.run(mtm)
}

/// Create a socketpair of the given type; set CLOEXEC on the supervisor end (fd.0) so the
/// worker doesn't inherit it. Returns `(supervisor_end, worker_end)` as **owned** fds, so
/// every path that drops them (notably an error return before the spawn completes) closes
/// them instead of leaking.
fn socketpair(sock_type: libc::c_int) -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, sock_type, 0, fds.as_mut_ptr()) } != 0 {
        anyhow::bail!("socketpair: {}", std::io::Error::last_os_error());
    }
    // SAFETY: on success the kernel handed us two fresh, open fds; we take sole ownership.
    let (sup_fd, worker_fd) =
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    unsafe {
        let f = libc::fcntl(sup_fd.as_raw_fd(), libc::F_GETFD);
        libc::fcntl(sup_fd.as_raw_fd(), libc::F_SETFD, f | libc::FD_CLOEXEC);
        // Writing to a socket whose peer (the worker) has exited must fail with an error,
        // not raise SIGPIPE and kill the supervisor (macOS has no MSG_NOSIGNAL).
        let on: libc::c_int = 1;
        libc::setsockopt(
            sup_fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            &on as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
    Ok((sup_fd, worker_fd))
}

/// Mark `fd` non-blocking so a write to a full socket fails fast (EAGAIN) instead of
/// blocking the caller. Best-effort: a failure here only forfeits the non-blocking property.
fn set_nonblocking(fd: libc::c_int) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

/// Request `bytes` of send and receive buffer on `fd` (best-effort; the kernel may clamp).
fn set_socket_buffer(fd: libc::c_int, bytes: libc::c_int) {
    for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                opt,
                &bytes as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
}
