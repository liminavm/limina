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

use std::ffi::CString;
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use devices::virtio::net::backend::{
    ConnectError, ExternalBackend, NetBackend, ReadError, Reconnect, SysError, WriteError,
};
use limina_netd_wire::{TAG_FRAME, WorkerEnd, drain, ring, socketpair};
use limina_ring::PushError;

/// The descriptor netd finds its end of the channel on.
const NETD_CHANNEL_FD: RawFd = 3;

/// netd's binary: next to this one, as cargo and the app bundle both lay them out.
pub fn netd_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("limina-netd"))
}

/// The libkrun backend builder for a NIC on netd.
pub fn backend(exe: PathBuf) -> ExternalBackend {
    Arc::new(move |_features| {
        Netd::start(&exe)
            .map(|n| Box::new(n) as Box<dyn NetBackend + Send>)
            .map_err(|e| ConnectError::External(format!("starting {}: {e}", exe.display())))
    })
}

/// One running netd and this worker's end of its channel.
struct Netd {
    exe: PathBuf,
    /// `None` once it has been killed and reaped: a reaped pid may already name someone else.
    pid: Option<libc::pid_t>,
    end: WorkerEnd,
}

impl Netd {
    fn start(exe: &Path) -> io::Result<Self> {
        let (ours, theirs) = socketpair()?;
        // The region goes out before netd exists; the socket holds the setup message for it.
        let end = WorkerEnd::new(ours)?;
        let pid = spawn(exe, theirs)?;
        log::info!("virtio-net: started limina-netd (pid {pid})");
        Ok(Self {
            exe: exe.to_path_buf(),
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
        match Netd::start(&self.exe) {
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

/// Starts netd with `channel` as its descriptor 3, stdin on /dev/null, our stdout and stderr (so
/// its log lands in the worker's), and nothing else: every other descriptor is closed in the
/// child, whatever its close-on-exec flag says.
fn spawn(exe: &Path, channel: OwnedFd) -> io::Result<libc::pid_t> {
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
            let fd = channel.as_raw_fd();
            if fd == NETD_CHANNEL_FD {
                check(posix_spawn_file_actions_addinherit_np(&mut actions, fd))?;
            } else {
                check(libc::posix_spawn_file_actions_adddup2(
                    &mut actions,
                    fd,
                    NETD_CHANNEL_FD,
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
