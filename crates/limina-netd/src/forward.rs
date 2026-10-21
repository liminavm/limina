//! Port forwards: listening sockets the worker binds and hands over the control pair
//! (`limina_netd_wire::Control`), each carried to a port on the guest. netd never binds one: the
//! port is the worker's, and outlives a netd restart.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};

use limina_netd_wire::{Control, recv_control, set_nonblocking};

use crate::poller::Poller;

struct Forward {
    id: u32,
    guest_port: u16,
    listener: OwnedFd,
}

pub struct Forwards {
    ctl: Option<OwnedFd>,
    by_fd: HashMap<RawFd, Forward>,
}

impl Forwards {
    /// Takes forwards from `ctl`, if the worker gave netd a control pair.
    pub fn new(ctl: Option<OwnedFd>, poller: &Poller) -> io::Result<Self> {
        if let Some(c) = &ctl {
            set_nonblocking(c.as_fd())?;
            poller.add(c.as_raw_fd())?;
        }
        Ok(Self {
            ctl,
            by_fd: HashMap::new(),
        })
    }

    pub fn is_control(&self, fd: RawFd) -> bool {
        self.ctl.as_ref().is_some_and(|c| c.as_raw_fd() == fd)
    }

    pub fn is_listener(&self, fd: RawFd) -> bool {
        self.by_fd.contains_key(&fd)
    }

    /// Applies what the worker sent on the control pair; where the guest is goes to `guest`.
    pub fn control(
        &mut self,
        poller: &Poller,
        guest: &mut dyn FnMut([u8; 6], [u8; 4]),
    ) -> io::Result<()> {
        let Some(ctl) = &self.ctl else { return Ok(()) };
        while let Some((msg, fd)) = recv_control(ctl.as_fd())? {
            match (msg, fd) {
                (Control::Listen { id, guest_port }, Some(listener)) => {
                    set_nonblocking(listener.as_fd())?;
                    poller.add(listener.as_raw_fd())?;
                    log::info!("forward {id}: carrying connections to guest port {guest_port}");
                    self.by_fd.insert(
                        listener.as_raw_fd(),
                        Forward {
                            id,
                            guest_port,
                            listener,
                        },
                    );
                }
                (Control::Close { id }, _) => {
                    self.by_fd.retain(|_, f| f.id != id);
                    log::info!("forward {id}: closed");
                }
                (Control::Guest { mac, ip }, _) => guest(mac, ip),
                (Control::Listen { .. }, None) => unreachable!("refused by recv_control"),
            }
        }
        Ok(())
    }

    /// Accepts every connection waiting on listener `fd`, handing each to `open` with the guest
    /// port it is for.
    pub fn accept(&mut self, fd: RawFd, open: &mut dyn FnMut(u16, OwnedFd)) {
        let Some(f) = self.by_fd.get(&fd) else { return };
        loop {
            // SAFETY: accepting on a listening socket we own; the peer address is not wanted.
            let s = unsafe {
                libc::accept(
                    f.listener.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if s < 0 {
                let e = io::Error::last_os_error();
                match e.kind() {
                    io::ErrorKind::WouldBlock => return,
                    io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted => continue,
                    _ => {
                        log::warn!("forward {}: accept: {e}", f.id);
                        return;
                    }
                }
            }
            // SAFETY: the connection accept just made is ours.
            open(f.guest_port, unsafe { OwnedFd::from_raw_fd(s) });
        }
    }
}
