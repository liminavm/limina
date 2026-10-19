//! netd's sandbox: once it holds its descriptors, netd can make sockets and use them, ask the host
//! resolver, and nothing else — no files, no other processes, no binding a port.
//!
//! netd parses untrusted traffic from the guest and from the network, so a bug in it should reach
//! no further than its sockets. What it needs from then on:
//!
//! - **Sockets:** new TCP and UDP sockets, connecting and sending out on them (`network-outbound`),
//!   and binding an ephemeral port and receiving (`network-inbound`, without which a UDP flow's
//!   socket cannot even bind), here and on the forwards' listening sockets the worker hands over.
//! - **The host resolver:** `DNSServiceQueryRecord` reaches mDNSResponder at its Unix socket,
//!   `/var/run/mDNSResponder`: an outbound connection like any other, but reaching it resolves
//!   the `/var` symlink, whose metadata is the one piece of the file system netd may see. Without
//!   that it fails as `kDNSServiceErr_ServiceNotRunning`.
//!
//! Everything else it holds already: the channel, the shared memory, its stderr.

use std::ffi::{CStr, c_char};
use std::io;

const PROFILE: &CStr = c"(version 1)
(deny default)
(allow system-socket)
(allow network-outbound)
(allow network-inbound)
(allow file-read-metadata (literal \"/var\"))
";

unsafe extern "C" {
    fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> i32;
    fn sandbox_free_error(errorbuf: *mut c_char);
}

/// Enters the sandbox, for good.
pub fn enter() -> io::Result<()> {
    let mut err: *mut c_char = std::ptr::null_mut();
    // SAFETY: a NUL-terminated profile; the error buffer is freed below.
    let rc = unsafe { sandbox_init(PROFILE.as_ptr(), 0, &mut err) };
    if rc == 0 {
        return Ok(());
    }
    let msg = if err.is_null() {
        "no reason given".to_string()
    } else {
        // SAFETY: a NUL-terminated message from sandbox_init, freed once.
        let m = unsafe { CStr::from_ptr(err) }
            .to_string_lossy()
            .into_owned();
        unsafe { sandbox_free_error(err) };
        m
    };
    Err(io::Error::other(format!("entering the sandbox: {msg}")))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream, UdpSocket};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::process::Command;
    use std::time::{Duration, Instant};

    use crate::dns;
    use crate::poller::Poller;

    const CHILD: &str = "LIMINA_NETD_SANDBOX_CHILD";

    /// Runs `sandboxed_child` in a child of this test binary, against a TCP and a UDP echo the
    /// parent serves (the child may not bind them itself) and a listener it inherits.
    #[test]
    fn the_sandbox_holds_netd_to_its_sockets() {
        let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let tcp_port = tcp.local_addr().unwrap().port();
        let udp_port = udp.local_addr().unwrap().port();
        // A forward's listener, as the worker hands it over: inherited, already bound.
        let forward = TcpListener::bind("127.0.0.1:0").unwrap();
        let forward_port = forward.local_addr().unwrap().port();
        let forward_fd = forward.as_raw_fd();
        // SAFETY: clearing close-on-exec on our own descriptor, so the child inherits it.
        unsafe { libc::fcntl(forward_fd, libc::F_SETFD, 0) };

        std::thread::spawn(move || {
            for s in tcp.incoming() {
                let mut s = s.unwrap();
                let mut b = [0u8; 4];
                s.read_exact(&mut b).unwrap();
                s.write_all(&b).unwrap();
            }
        });
        std::thread::spawn(move || {
            let mut b = [0u8; 16];
            loop {
                let (n, from) = udp.recv_from(&mut b).unwrap();
                udp.send_to(&b[..n], from).unwrap();
            }
        });
        let connector = std::thread::spawn(move || {
            // The child accepts this on the inherited listener.
            let mut s = TcpStream::connect(("127.0.0.1", forward_port)).unwrap();
            let mut b = [0u8; 2];
            s.read_exact(&mut b).unwrap();
            b
        });

        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "sandbox::tests::sandboxed_child",
                "--include-ignored",
                "--nocapture",
            ])
            .env(CHILD, format!("{tcp_port} {udp_port} {forward_fd}"))
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "the sandboxed child failed:\n{text}");
        assert!(
            text.contains("1 passed"),
            "the child test did not run:\n{text}"
        );
        assert_eq!(&connector.join().unwrap(), b"ok");
        drop(forward);
    }

    #[test]
    #[ignore = "run in a child by the_sandbox_holds_netd_to_its_sockets"]
    fn sandboxed_child() {
        let Ok(spec) = std::env::var(CHILD) else {
            return;
        };
        let v: Vec<i32> = spec.split(' ').map(|s| s.parse().unwrap()).collect();
        let (tcp_port, udp_port, forward_fd) = (v[0] as u16, v[1] as u16, v[2]);

        super::enter().unwrap();

        let err = std::fs::File::open("/etc/hosts").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");
        assert!(
            UdpSocket::bind("127.0.0.1:0")
                .and_then(|s| s.local_addr())
                .is_ok(),
            "an ephemeral UDP port is how every flow starts"
        );

        let mut s = TcpStream::connect(("127.0.0.1", tcp_port)).unwrap();
        s.write_all(b"ping").unwrap();
        let mut b = [0u8; 4];
        s.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"ping");

        let u = UdpSocket::bind("0.0.0.0:0").unwrap();
        u.connect(("127.0.0.1", udp_port)).unwrap();
        u.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        u.send(b"pong").unwrap();
        let n = u.recv(&mut b).unwrap();
        assert_eq!(&b[..n], b"pong");

        // SAFETY: the listener the parent left open for us.
        let forward = TcpListener::from(unsafe { OwnedFd::from_raw_fd(forward_fd) });
        let (mut c, _) = forward.accept().unwrap();
        c.write_all(b"ok").unwrap();

        let poller = Poller::new().unwrap();
        let mut resolver = dns::Resolver::default();
        let q = dns::parse_query(&dns::tests::query_msg(&[b"localhost"], 1, None)).unwrap();
        assert_eq!(
            resolver.ask(5353, q, &poller),
            None,
            "mDNSResponder refused"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut events = Vec::new();
        loop {
            assert!(Instant::now() < deadline, "no answer from mDNSResponder");
            poller
                .wait(Some(Duration::from_millis(500)), &mut events)
                .unwrap();
            if let Some((_, msg)) = events.iter().find_map(|e| resolver.process(e.fd)) {
                assert!(msg.ends_with(&[0, 4, 127, 0, 0, 1]), "{msg:?}");
                break;
            }
        }
    }
}
