//! The NIC backend on a `limina-netd` this worker spawns (`docs/design/multi-vm-networking.md`
//! §3.2), handed to libkrun as an external backend.
//!
//! The worker owns netd: it starts it with one end of a socketpair as descriptor 3, shares the
//! two frame rings with it (`limina_netd_wire`), and starts a fresh one when the old one dies.
//! Nothing else can reach the channel, and netd sees the worker's death as EOF and exits.
//!
//! Waking follows the rings' handshake. A frame for netd rings its doorbell only when netd has
//! parked; on the way back, the worker parks the guest ring when it runs dry and netd rings the
//! socket, which libkrun's net worker watches as this backend's descriptor.
//!
//! Port forwards are the worker's too ([`Forwards`]): it binds each listening socket and hands
//! netd a copy over the control pair (descriptor 4), and again to every netd that replaces it, so
//! a forward's port stays taken for the VM's life.

use std::ffi::CString;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddrV4, TcpListener};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use devices::virtio::net::backend::{
    ConnectError, ExternalBackend, NetBackend, ReadError, Reconnect, SysError, WriteError,
};
use limina_launch::connect::ListenAt;
use limina_netd_wire::{
    CHANNEL_FD, CONTROL_FD, Control, TAG_FRAME, WorkerEnd, control_socketpair, drain, ring,
    send_control, socketpair,
};
use limina_ring::PushError;

use super::lease::Lease;
use super::switch::{Side, SwitchControl};

struct Forward {
    id: u32,
    host: SocketAddrV4,
    guest_port: u16,
    listener: TcpListener,
}

#[derive(Default)]
struct Table {
    next_id: u32,
    forwards: Vec<Forward>,
    /// The running netd's control pair.
    ctl: Option<OwnedFd>,
    /// Where the guest is, when netd is not the one that leased it its address.
    guest: Option<([u8; 6], [u8; 4])>,
}

/// The NIC's port forwards, shared between the backend (which hands them to each netd it
/// starts) and the control link (which changes them while the VM runs).
#[derive(Default)]
pub struct Forwards(Mutex<Table>);

impl Forwards {
    fn table(&self) -> std::sync::MutexGuard<'_, Table> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Binds `host` and carries its connections to the guest's `guest_port`. A port that is
    /// taken fails here, and nothing changes.
    pub fn add(&self, host: SocketAddrV4, guest_port: u16) -> io::Result<()> {
        let mut t = self.table();
        if t.forwards.iter().any(|f| f.host == host) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("{host} is already forwarded"),
            ));
        }
        let listener = TcpListener::bind(host)?;
        let id = t.next_id;
        t.next_id += 1;
        if let Some(ctl) = &t.ctl {
            // A dead netd gets the forward with the rest when its replacement starts.
            let _ = send_control(
                ctl.as_fd(),
                Control::Listen { id, guest_port },
                Some(listener.as_fd()),
            );
        }
        log::info!("virtio-net: forwarding {host} to guest port {guest_port}");
        t.forwards.push(Forward {
            id,
            host,
            guest_port,
            listener,
        });
        Ok(())
    }

    /// Stops forwarding `host`; connections already carried stay up.
    pub fn remove(&self, host: SocketAddrV4) -> io::Result<()> {
        let mut t = self.table();
        let i = t
            .forwards
            .iter()
            .position(|f| f.host == host)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("{host} is not forwarded"))
            })?;
        let f = t.forwards.remove(i);
        if let Some(ctl) = &t.ctl {
            let _ = send_control(ctl.as_fd(), Control::Close { id: f.id }, None);
        }
        log::info!("virtio-net: no longer forwarding {host}");
        Ok(())
    }

    /// Tells netd the guest is at `ip` with `mac`, when that changes: on vmnet the guest's
    /// address is vmnet's, and netd carries the forwards to it there.
    pub fn tell_guest(&self, mac: [u8; 6], ip: [u8; 4]) {
        let mut t = self.table();
        if t.guest == Some((mac, ip)) {
            return;
        }
        t.guest = Some((mac, ip));
        if let Some(ctl) = &t.ctl {
            let _ = send_control(ctl.as_fd(), Control::Guest { mac, ip }, None);
        }
    }

    /// Hands every forward, and where the guest is, to a freshly started netd on its control
    /// pair.
    fn attach(&self, ctl: OwnedFd) {
        let mut t = self.table();
        for f in &t.forwards {
            let msg = Control::Listen {
                id: f.id,
                guest_port: f.guest_port,
            };
            if let Err(e) = send_control(ctl.as_fd(), msg, Some(f.listener.as_fd())) {
                log::warn!("virtio-net: handing netd the forward of {}: {e}", f.host);
            }
        }
        if let Some((mac, ip)) = t.guest {
            let _ = send_control(ctl.as_fd(), Control::Guest { mac, ip }, None);
        }
        t.ctl = Some(ctl);
    }
}

/// Serves the NIC's control link: one thread, one request line at a time, answered `ok` or
/// `error: …`. `watch` is answered with the guest's address and then each change
/// ([`Lease::watch`]), for as long as the client reads; the connection carries nothing else after
/// it. A NIC on netd takes `forward` and `unforward`, and one that switches sides takes `switch
/// vmnet|netd` too, answered once the move is under way.
pub fn serve_control(
    at: ListenAt,
    forwards: Option<Arc<Forwards>>,
    switch: Option<Arc<SwitchControl>>,
    lease: Arc<Lease>,
) -> anyhow::Result<()> {
    let listener = at.listen()?;
    std::thread::Builder::new()
        .name("net-control".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(s) => serve_client(s, forwards.as_deref(), switch.as_deref(), &lease),
                    Err(e) => log::error!("net-control: accept failed: {e}"),
                }
            }
        })?;
    Ok(())
}

fn serve_client(
    stream: UnixStream,
    forwards: Option<&Forwards>,
    switch: Option<&SwitchControl>,
    lease: &Lease,
) {
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        if line.trim() == "watch" {
            if let Err(e) = lease.watch(out) {
                log::warn!("net-control: a watcher went away at once: {e}");
            }
            return;
        }
        let reply = match answer(&line, forwards, switch) {
            Ok(()) => "ok\n".to_string(),
            Err(e) => format!("error: {e}\n"),
        };
        if out.write_all(reply.as_bytes()).is_err() {
            break;
        }
    }
}

fn answer(
    line: &str,
    forwards: Option<&Forwards>,
    switch: Option<&SwitchControl>,
) -> Result<(), String> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let host = |s: &str| {
        s.parse::<SocketAddrV4>()
            .map_err(|e| format!("{s:?} is not HOST_IP:PORT: {e}"))
    };
    let forwards = || forwards.ok_or("this NIC has no forwards (it is not on limina-netd)");
    match words.as_slice() {
        ["forward", h, guest] => {
            let guest: u16 = guest
                .parse()
                .map_err(|e| format!("{guest:?} is not a port: {e}"))?;
            forwards()?.add(host(h)?, guest).map_err(|e| e.to_string())
        }
        ["unforward", h] => forwards()?.remove(host(h)?).map_err(|e| e.to_string()),
        ["switch", side] => {
            let switch = switch.ok_or("this NIC does not switch sides")?;
            switch.request(side.parse::<Side>()?, "asked to");
            Ok(())
        }
        _ => Err(format!(
            "unknown request {line:?} (have: watch, forward HOST_IP:PORT GUEST_PORT, unforward \
             HOST_IP:PORT, switch vmnet|netd)"
        )),
    }
}

/// netd's binary: next to this one, as cargo and the app bundle both lay them out.
pub fn netd_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("limina-netd"))
}

/// The libkrun backend builder for a NIC on netd.
pub fn backend(exe: PathBuf, forwards: Arc<Forwards>) -> ExternalBackend {
    Arc::new(move |_features| {
        Netd::start(&exe, &forwards)
            .map(|n| Box::new(n) as Box<dyn NetBackend + Send>)
            .map_err(|e| ConnectError::External(format!("starting {}: {e}", exe.display())))
    })
}

/// One running netd and this worker's end of its channel.
pub struct Netd {
    exe: PathBuf,
    forwards: Arc<Forwards>,
    /// `None` once it has been killed and reaped: a reaped pid may already name someone else.
    pid: Option<libc::pid_t>,
    end: WorkerEnd,
}

impl Netd {
    pub fn start(exe: &Path, forwards: &Arc<Forwards>) -> io::Result<Self> {
        let (ours, theirs) = socketpair()?;
        let (ctl_ours, ctl_theirs) = control_socketpair()?;
        // The region goes out before netd exists; the socket holds the setup message for it, as
        // the control pair holds the forwards.
        let end = WorkerEnd::new(ours)?;
        let pid = spawn(exe, theirs, ctl_theirs)?;
        log::info!("virtio-net: started limina-netd (pid {pid})");
        forwards.attach(ctl_ours);
        Ok(Self {
            exe: exe.to_path_buf(),
            forwards: forwards.clone(),
            pid: Some(pid),
            end,
        })
    }

    /// Whether netd has closed its end, which is how it dies.
    fn gone(&self) -> bool {
        drain(self.end.sock.as_fd()).unwrap_or(true)
    }
}

impl NetBackend for Netd {
    fn read_frame(&mut self, buf: &mut [u8]) -> Result<usize, ReadError> {
        self.end.to_guest.unpark();
        loop {
            match self.end.to_guest.read_into(buf) {
                Ok(Some((TAG_FRAME, len))) => return Ok(len),
                Ok(Some(_)) => continue,
                Ok(None) => {}
                Err(e) => {
                    // netd wrote something the ring cannot hold. It is untrustworthy now: stop it,
                    // and the hang-up brings a fresh one.
                    log::error!(
                        "virtio-net: limina-netd corrupted its ring ({e:?}); restarting it"
                    );
                    kill(&mut self.pid);
                    return Err(ReadError::NothingRead);
                }
            }
            // Doorbells are drained before parking, so a ring after this point is a new edge.
            if self.gone() || self.end.to_guest.park() {
                return Err(ReadError::NothingRead);
            }
        }
    }

    fn write_frame(&mut self, _hdr_len: usize, buf: &mut [u8]) -> Result<(), WriteError> {
        match self.end.to_netd.push(TAG_FRAME, &[buf]) {
            Ok(()) => {
                if self.end.to_netd.publish() {
                    ring(self.end.sock.as_fd());
                }
                Ok(())
            }
            Err(PushError::Full) => {
                if self.gone() {
                    return Err(WriteError::ProcessNotRunning);
                }
                // netd is behind; it is running (it has frames), so no doorbell is owed.
                Err(WriteError::NothingWritten)
            }
            // Larger than any frame the device hands over; drop it like a wire would.
            Err(PushError::TooLarge) => Ok(()),
        }
    }

    fn has_unfinished_write(&self) -> bool {
        false
    }

    fn try_finish_write(&mut self, _hdr_len: usize, _buf: &[u8]) -> Result<(), WriteError> {
        Ok(())
    }

    fn raw_socket_fd(&self) -> RawFd {
        self.end.sock.as_raw_fd()
    }

    fn synthesizes_vnet_hdr(&self) -> bool {
        false
    }

    fn write_retry_delay_us(&self) -> u64 {
        50
    }

    /// netd died: reap it and start another, on a fresh channel. The guest keeps its address
    /// (netd leases the one address it has); its open connections are gone.
    fn reconnect(&mut self) -> Reconnect {
        kill(&mut self.pid);
        match Netd::start(&self.exe, &self.forwards) {
            Ok(fresh) => {
                *self = fresh;
                Reconnect::Done
            }
            Err(e) => {
                log::warn!("virtio-net: restarting limina-netd: {e}");
                Reconnect::NotYet(SysError::from_raw(e.raw_os_error().unwrap_or(libc::EIO)))
            }
        }
    }
}

impl Drop for Netd {
    fn drop(&mut self) {
        kill(&mut self.pid);
    }
}

/// Kills and reaps a netd, once. Reaping an already-dead one is what keeps it from lingering as a
/// zombie.
fn kill(pid: &mut Option<libc::pid_t>) {
    let Some(pid) = pid.take() else {
        return;
    };
    // SAFETY: our own child, not yet reaped, so the pid is still its.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        let mut status = 0;
        libc::waitpid(pid, &mut status, 0);
    }
}

unsafe extern "C" {
    /// Keeps `fd` open in the child of a `POSIX_SPAWN_CLOEXEC_DEFAULT` spawn (`<spawn.h>`).
    fn posix_spawn_file_actions_addinherit_np(
        actions: *mut libc::posix_spawn_file_actions_t,
        fd: libc::c_int,
    ) -> libc::c_int;
}

/// A close-on-exec copy of `fd` at 10 or above, clear of the descriptors netd is given.
fn above_targets(fd: &OwnedFd) -> io::Result<OwnedFd> {
    // SAFETY: duplicating a descriptor we hold; the copy is ours.
    let dup = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
    if dup < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just created.
    Ok(unsafe { OwnedFd::from_raw_fd(dup) })
}

/// Starts netd with `channel` as its descriptor 3 and `control` as 4, stdin on /dev/null, our
/// stdout and stderr (so its log lands in the worker's), and nothing else: every other descriptor
/// is closed in the child, whatever its close-on-exec flag says.
fn spawn(exe: &Path, channel: OwnedFd, control: OwnedFd) -> io::Result<libc::pid_t> {
    // Copies above both targets, so neither dup2 can land on the other's source.
    let channel = above_targets(&channel)?;
    let control = above_targets(&control)?;
    let path = CString::new(exe.as_os_str().as_bytes())
        .map_err(|_| io::Error::other("netd path has a NUL"))?;
    let argv = [path.as_ptr(), std::ptr::null()];
    unsafe extern "C" {
        static environ: *const *const libc::c_char;
    }
    let check = |rc: libc::c_int| {
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(rc))
        }
    };
    // SAFETY: the attribute and action objects are initialised before use and destroyed after;
    // argv and environ are NUL-terminated.
    unsafe {
        let mut attr: libc::posix_spawnattr_t = std::ptr::null_mut();
        let mut actions: libc::posix_spawn_file_actions_t = std::ptr::null_mut();
        check(libc::posix_spawnattr_init(&mut attr))?;
        check(libc::posix_spawn_file_actions_init(&mut actions))?;
        let result = (|| {
            check(libc::posix_spawnattr_setflags(
                &mut attr,
                libc::POSIX_SPAWN_CLOEXEC_DEFAULT as libc::c_short,
            ))?;
            check(libc::posix_spawn_file_actions_addopen(
                &mut actions,
                0,
                c"/dev/null".as_ptr(),
                libc::O_RDONLY,
                0,
            ))?;
            check(posix_spawn_file_actions_addinherit_np(&mut actions, 1))?;
            check(posix_spawn_file_actions_addinherit_np(&mut actions, 2))?;
            for (fd, target) in [(&channel, CHANNEL_FD), (&control, CONTROL_FD)] {
                check(libc::posix_spawn_file_actions_adddup2(
                    &mut actions,
                    fd.as_raw_fd(),
                    target,
                ))?;
            }
            let mut pid = 0;
            check(libc::posix_spawn(
                &mut pid,
                path.as_ptr(),
                &actions,
                &attr,
                argv.as_ptr() as *const *mut libc::c_char,
                environ as *const *mut libc::c_char,
            ))?;
            Ok(pid)
        })();
        libc::posix_spawn_file_actions_destroy(&mut actions);
        libc::posix_spawnattr_destroy(&mut attr);
        result
    }
}
