//! The NIC backend that moves between vmnet and limina-netd under one virtio device and one MAC
//! (`docs/design/multi-vm-networking.md` §3.5), handed to libkrun as an external backend.
//!
//! [`Switch`] is the backend the net worker drives. It holds a running netd always and a vmnet
//! interface while the NIC is on vmnet; the guest's frames go to whichever side the NIC is on,
//! except frames for netd's own subnet, which go to netd from either side: on vmnet, netd still
//! carries the port forwards in (from its gateway's address, which the guest answers through its
//! default route), so the `127.0.0.1` SSH forward works the same on both. The worker watches the
//! command socket as the backend's descriptor and both sides' descriptors as extras.
//!
//! [`SwitchControl`] moves the NIC, on a thread of its own: vmnet's start and stop each take up
//! to seconds, which the net worker's thread must never wait on. A move drops the guest's carrier,
//! changes side, and raises it again after [`flap`], so the guest's network manager takes the
//! link down and asks the new side for a lease. Each move is logged with its reason.

use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use devices::virtio::net::NetLink;
use devices::virtio::net::backend::{
    ConnectError, ExternalBackend, NetBackend, ReadError, Reconnect, WriteError,
};
use devices::virtio::net::vmnet::{Vmnet, VmnetConfig};

use super::netd::{Forwards, Netd};
use super::uplink::{INTERNET, RouteWatch, route_to, wanted};

/// netd's subnet, 192.168.127.0/24: frames for it go to netd from either side.
const NETD_NET: [u8; 3] = [192, 168, 127];

/// How long the carrier stays down across a move; `LIMINA_NET_FLAP_MS` overrides it. Fedora 44's
/// NetworkManager asks for a fresh lease when the carrier returns after a drop of 100 ms as after
/// one of 5 s; a second leaves room for DHCP clients that wait out a short flicker.
const FLAP: Duration = Duration::from_secs(1);

/// How long the routes must stay clear before a NIC on netd goes back to vmnet, so a VPN that
/// flaps does not flap the guest; doubled after each start of vmnet that fails, up to
/// [`HOLD_MAX`].
const HOLD: Duration = Duration::from_secs(10);
const HOLD_MAX: Duration = Duration::from_secs(300);

/// The side a NIC is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Vmnet,
    Netd,
}

impl std::str::FromStr for Side {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "vmnet" => Ok(Side::Vmnet),
            "netd" => Ok(Side::Netd),
            _ => Err(format!("{s:?} is not a side (vmnet or netd)")),
        }
    }
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Side::Vmnet => "vmnet",
            Side::Netd => "limina-netd",
        })
    }
}

/// What the controller asks of the backend.
enum Command {
    /// Serve the guest from this interface.
    UseVmnet(Box<Vmnet>),
    /// Serve the guest from netd; the interface goes back to be stopped.
    UseNetd,
}

/// The carrier's down time across a move.
fn flap() -> Duration {
    std::env::var("LIMINA_NET_FLAP_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(FLAP)
}

/// Where an IPv4 frame from the guest is going, and where it comes from (MAC and address).
fn ipv4_ends(frame: &[u8]) -> Option<([u8; 4], [u8; 6], [u8; 4])> {
    if frame.get(12..14)? != [0x08, 0x00] {
        return None;
    }
    let ip = frame.get(14..34)?;
    Some((
        ip[16..20].try_into().ok()?,
        frame[6..12].try_into().ok()?,
        ip[12..16].try_into().ok()?,
    ))
}

/// The backend the net worker drives.
pub struct Switch {
    vmnet: Option<Box<Vmnet>>,
    netd: Netd,
    forwards: Arc<Forwards>,
    commands: Receiver<Command>,
    /// Rung by the controller after each command; the worker watches it.
    wake: UnixDatagram,
    /// Interfaces taken off the NIC, for the controller to stop.
    retired: Sender<Box<Vmnet>>,
    /// Which side a read tries first, alternating so neither starves the other.
    netd_first: bool,
}

impl Switch {
    fn apply_commands(&mut self) {
        let mut sink = [0u8; 64];
        while self.wake.recv(&mut sink).is_ok() {}
        while let Ok(cmd) = self.commands.try_recv() {
            match cmd {
                Command::UseVmnet(v) => {
                    if let Some(old) = self.vmnet.replace(v) {
                        let _ = self.retired.send(old);
                    }
                }
                Command::UseNetd => {
                    if let Some(old) = self.vmnet.take() {
                        let _ = self.retired.send(old);
                    }
                }
            }
        }
    }
}

impl NetBackend for Switch {
    fn read_frame(&mut self, buf: &mut [u8]) -> Result<usize, ReadError> {
        self.apply_commands();
        self.netd_first = !self.netd_first;
        if !self.netd_first
            && let Some(v) = &mut self.vmnet
        {
            match v.read_frame(buf) {
                Err(ReadError::NothingRead) => {}
                other => return other,
            }
        }
        match self.netd.read_frame(buf) {
            Err(ReadError::NothingRead) => {}
            other => return other,
        }
        match &mut self.vmnet {
            Some(v) if self.netd_first => v.read_frame(buf),
            _ => Err(ReadError::NothingRead),
        }
    }

    fn write_frame(&mut self, hdr_len: usize, buf: &mut [u8]) -> Result<(), WriteError> {
        self.apply_commands();
        let ends = buf.get(hdr_len..).and_then(ipv4_ends);
        if let Some((_, mac, src)) = ends
            && src != [0; 4]
        {
            self.forwards.tell_guest(mac, src);
        }
        let to_netd = ends.is_some_and(|(dst, _, _)| dst[..3] == NETD_NET);
        match &mut self.vmnet {
            Some(v) if !to_netd => v.write_frame(hdr_len, buf),
            _ => self.netd.write_frame(hdr_len, buf),
        }
    }

    fn has_unfinished_write(&self) -> bool {
        false
    }

    fn try_finish_write(&mut self, _hdr_len: usize, _buf: &[u8]) -> Result<(), WriteError> {
        Ok(())
    }

    fn raw_socket_fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }

    fn extra_fds(&self) -> Vec<RawFd> {
        let mut fds = vec![self.netd.raw_socket_fd()];
        fds.extend(self.vmnet.as_ref().map(|v| v.raw_socket_fd()));
        fds
    }

    fn synthesizes_vnet_hdr(&self) -> bool {
        false
    }

    fn write_retry_delay_us(&self) -> u64 {
        50
    }

    /// netd died: start another. vmnet's side carries on meanwhile.
    fn reconnect(&mut self) -> Reconnect {
        self.netd.reconnect()
    }
}

/// The backend's ends of the controller's channels, taken by the one backend built.
struct BackendEnds {
    commands: Receiver<Command>,
    wake: UnixDatagram,
    retired: Sender<Box<Vmnet>>,
}

/// A requested move.
struct Move {
    to: Side,
    reason: String,
    /// A NIC on vmnet starts a fresh interface: the old one NATs out of an uplink that is gone.
    rebuild: bool,
}

/// Moves the NIC between its sides.
pub struct SwitchControl {
    vmnet: VmnetConfig,
    /// The features the guest acked, which a new interface is started for.
    features: OnceLock<u64>,
    link: OnceLock<Arc<NetLink>>,
    side: Mutex<Side>,
    commands: Sender<Command>,
    wake: UnixDatagram,
    retired: Mutex<Receiver<Box<Vmnet>>>,
    backend_ends: Mutex<Option<BackendEnds>>,
    moves: Mutex<Option<Sender<Move>>>,
    /// Moves asked for and not yet carried out.
    pending: AtomicUsize,
    /// Asked onto netd: the routes do not move it back until asked otherwise.
    pinned: AtomicBool,
    /// An address on the last vmnet interface's subnet the host does not hold itself, whose route
    /// says whether vmnet's subnet is still the bridge's.
    subnet: Mutex<Option<Ipv4Addr>>,
}

impl SwitchControl {
    pub fn new(vmnet: VmnetConfig) -> io::Result<Arc<Self>> {
        let (commands, commands_rx) = mpsc::channel();
        let (retired_tx, retired) = mpsc::channel();
        let (wake_rx, wake) = UnixDatagram::pair()?;
        wake_rx.set_nonblocking(true)?;
        wake.set_nonblocking(true)?;
        let control = Arc::new(Self {
            vmnet,
            features: OnceLock::new(),
            link: OnceLock::new(),
            side: Mutex::new(Side::Netd),
            commands,
            wake,
            retired: Mutex::new(retired),
            backend_ends: Mutex::new(Some(BackendEnds {
                commands: commands_rx,
                wake: wake_rx,
                retired: retired_tx,
            })),
            moves: Mutex::new(None),
            pending: AtomicUsize::new(0),
            pinned: AtomicBool::new(false),
            subnet: Mutex::new(None),
        });
        let (moves, moves_rx) = mpsc::channel::<Move>();
        *lock(&control.moves) = Some(moves);
        let c = control.clone();
        std::thread::Builder::new()
            .name("net-switch".into())
            .spawn(move || {
                for m in moves_rx {
                    c.carry_out(m);
                    c.pending.fetch_sub(1, Ordering::SeqCst);
                }
            })?;
        let c = control.clone();
        std::thread::Builder::new()
            .name("net-routes".into())
            .spawn(move || c.follow_routes())?;
        Ok(control)
    }

    /// The device's carrier, once the device exists.
    pub fn set_link(&self, link: Arc<NetLink>) {
        let _ = self.link.set(link);
    }

    pub fn side(&self) -> Side {
        *lock(&self.side)
    }

    /// Moves the NIC to `to` because someone asked, on the controller's thread; the log says when
    /// it is done. On netd it stays until asked back, as it would under a VPN that stays up.
    pub fn request(&self, to: Side, reason: &str) {
        self.pinned.store(to == Side::Netd, Ordering::SeqCst);
        self.enqueue(to, reason.to_string(), false);
    }

    fn enqueue(&self, to: Side, reason: String, rebuild: bool) {
        if let Some(m) = lock(&self.moves).as_ref() {
            self.pending.fetch_add(1, Ordering::SeqCst);
            if m.send(Move {
                to,
                reason,
                rebuild,
            })
            .is_err()
            {
                self.pending.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    /// Follows the host's routes for the VM's life, moving the NIC as [`wanted`] says: to netd at
    /// once, back to vmnet once the routes have stayed clear for the hold-off, and onto a fresh
    /// interface when vmnet's way out changes.
    fn follow_routes(&self) {
        let watch = match RouteWatch::new() {
            Ok(w) => w,
            Err(e) => {
                log::warn!("network: not following the host's routes: {e}");
                return;
            }
        };
        let mut way_out: Option<String> = None;
        let mut clear_since: Option<Instant> = None;
        let mut hold = HOLD;
        loop {
            if watch.wait(Duration::from_secs(1)) {
                // Changes come in bursts; read the routes once they have settled.
                while watch.wait(Duration::from_millis(300)) {}
            }
            if self.features.get().is_none()
                || self.pending.load(Ordering::SeqCst) > 0
                || self.pinned.load(Ordering::SeqCst)
            {
                clear_since = None;
                continue;
            }
            let uplink = route_to(INTERNET).ok().flatten();
            let subnet = lock(&self.subnet).and_then(|a| route_to(a).ok().flatten());
            let (want, why) = wanted(uplink.as_ref(), subnet.as_ref());
            if way_out.is_none() {
                log::info!(
                    "network: following the host's routes: out through {:?}, vmnet's subnet \
                     through {:?}",
                    uplink.as_ref().map(|r| &r.ifname),
                    subnet.as_ref().map(|r| (&r.ifname, r.dst, r.prefix))
                );
            }
            let out = uplink.map(|r| r.ifname);
            match (self.side(), want) {
                (Side::Vmnet, Side::Netd) => self.enqueue(Side::Netd, why, false),
                (Side::Vmnet, Side::Vmnet) => {
                    hold = HOLD;
                    if let (Some(was), Some(now)) = (&way_out, &out)
                        && was != now
                    {
                        self.enqueue(
                            Side::Vmnet,
                            format!("the way out moved from {was} to {now}"),
                            true,
                        );
                    }
                }
                (Side::Netd, Side::Vmnet) => {
                    let since = *clear_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= hold {
                        self.enqueue(Side::Vmnet, why, false);
                        clear_since = None;
                        hold = (hold * 2).min(HOLD_MAX);
                    }
                }
                (Side::Netd, Side::Netd) => clear_since = None,
            }
            way_out = out;
        }
    }

    fn send(&self, cmd: Command) {
        let _ = self.commands.send(cmd);
        let _ = self.wake.send(&[1]);
    }

    fn carry_out(&self, m: Move) {
        if self.side() == m.to && !m.rebuild {
            log::info!("network: already on {} ({})", m.to, m.reason);
            return;
        }
        let Some(&features) = self.features.get() else {
            log::warn!(
                "network: not moving to {}: the guest has not started its NIC",
                m.to
            );
            return;
        };
        let vmnet = match m.to {
            Side::Vmnet => match start_vmnet(&self.vmnet, features) {
                Ok(v) => {
                    *lock(&self.subnet) = v.subnet().map(|(host, _)| guest_side(host));
                    Some(Box::new(v))
                }
                Err(e) => {
                    log::warn!(
                        "network: staying on limina-netd: vmnet would not start ({e:?}); \
                         wanted for: {}",
                        m.reason
                    );
                    return;
                }
            },
            Side::Netd => None,
        };
        log::info!("network: moving to {} ({})", m.to, m.reason);
        let link = self.link.get();
        if let Some(l) = link {
            l.set_up(false);
        }
        let down = std::time::Instant::now();
        self.send(match vmnet {
            Some(v) => Command::UseVmnet(v),
            None => Command::UseNetd,
        });
        *lock(&self.side) = m.to;
        // The interface the backend gives back is stopped here, off the worker's thread.
        let retired = lock(&self.retired);
        if m.to == Side::Netd {
            match retired.recv_timeout(Duration::from_secs(10)) {
                Ok(v) => drop(v),
                Err(_) => log::warn!("network: the NIC kept its vmnet interface past 10 s"),
            }
        }
        while let Ok(v) = retired.try_recv() {
            drop(v);
        }
        drop(retired);
        std::thread::sleep(flap().saturating_sub(down.elapsed()));
        if let Some(l) = link {
            l.set_up(true);
        }
        log::info!("network: on {}", m.to);
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Whether a test asked for vmnet's start to fail, as it does without the entitlement or with
/// vmnet busy: `LIMINA_NET_VMNET_FAIL=1`.
fn vmnet_refused() -> bool {
    std::env::var("LIMINA_NET_VMNET_FAIL").is_ok_and(|v| v == "1")
}

/// An address on the guests' side of a vmnet subnet: the one after the host's own, whose route is
/// a local one on lo0 that says nothing about where the guests' traffic goes.
fn guest_side(host: Ipv4Addr) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(host).wrapping_add(1))
}

fn start_vmnet(config: &VmnetConfig, features: u64) -> Result<Vmnet, ConnectError> {
    if vmnet_refused() {
        return Err(ConnectError::Vmnet(
            "refused by LIMINA_NET_VMNET_FAIL".into(),
        ));
    }
    Vmnet::start(config.clone(), features)
}

/// The libkrun backend builder for a NIC that switches: on vmnet when it starts and the host's
/// routes let it serve, on netd otherwise.
pub fn backend(
    exe: PathBuf,
    forwards: Arc<Forwards>,
    control: Arc<SwitchControl>,
) -> ExternalBackend {
    Arc::new(move |features| {
        let ends = lock(&control.backend_ends)
            .take()
            .ok_or_else(|| ConnectError::External("the switching NIC is already built".into()))?;
        let _ = control.features.set(features);
        let netd = Netd::start(&exe, &forwards)
            .map_err(|e| ConnectError::External(format!("starting {}: {e}", exe.display())))?;
        let (want, why) = wanted(route_to(INTERNET).ok().flatten().as_ref(), None);
        let started = match want {
            Side::Vmnet => start_vmnet(&control.vmnet, features)
                .map_err(|e| format!("vmnet would not start ({e:?})")),
            Side::Netd => Err(why),
        };
        let vmnet = match started {
            Ok(v) => {
                log::info!("network: on vmnet");
                *lock(&control.subnet) = v.subnet().map(|(host, _)| guest_side(host));
                *lock(&control.side) = Side::Vmnet;
                Some(Box::new(v))
            }
            Err(why) => {
                log::warn!("network: on limina-netd: {why}");
                *lock(&control.side) = Side::Netd;
                None
            }
        };
        Ok(Box::new(Switch {
            vmnet,
            netd,
            forwards: forwards.clone(),
            commands: ends.commands,
            wake: ends.wake,
            retired: ends.retired,
            netd_first: false,
        }) as Box<dyn NetBackend + Send>)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(src: [u8; 4], dst: [u8; 4]) -> Vec<u8> {
        let mut f = vec![0u8; 60];
        f[6..12].copy_from_slice(&[0x5a, 1, 2, 3, 4, 5]);
        f[12..14].copy_from_slice(&[0x08, 0x00]);
        f[14] = 0x45;
        f[26..30].copy_from_slice(&src);
        f[30..34].copy_from_slice(&dst);
        f
    }

    #[test]
    fn an_ipv4_frame_names_its_ends() {
        let f = frame([192, 168, 64, 7], [192, 168, 127, 1]);
        assert_eq!(
            ipv4_ends(&f),
            Some(([192, 168, 127, 1], [0x5a, 1, 2, 3, 4, 5], [192, 168, 64, 7]))
        );
    }

    #[test]
    fn other_frames_name_none() {
        let mut arp = frame([0; 4], [0; 4]);
        arp[12..14].copy_from_slice(&[0x08, 0x06]);
        assert_eq!(ipv4_ends(&arp), None);
        assert_eq!(ipv4_ends(&[0u8; 20]), None);
    }
}
