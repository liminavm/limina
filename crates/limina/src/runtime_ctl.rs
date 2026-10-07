// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! What a running VM is like right now, and the knobs that change it: the supervisor's half.
//!
//! The supervisor binds `$TMPDIR/limina-vm-<pid>.sock` and answers a line protocol there, framed
//! like the debug socket's (`limina_debug::wire`): report lines, then `ok` or `err <why>`.
//!
//! ```text
//! > info
//! < ssh-port 2223
//! < parked no
//! < ok
//! > ssh-port 2300
//! < ssh-port 2300
//! < ok
//! > watch
//! < ssh-port 2300
//! < ok
//! < ssh-port 2301        (pushed whenever it changes, for as long as the client stays)
//! ```
//!
//! `ssh-port none` means the VM has no NAT network, or its gateway is not up yet. `parked yes`
//! means the guest is suspended and its window is waiting for a play click: the supervisor is
//! still up, but there is no worker behind it. After `watch`
//! the connection carries only those pushed lines, so a watcher that also wants to change
//! something opens a second connection. The control center keeps one watch open per running VM
//! (`center::live`); `limina ssh-port` is the command-line client.
//!
//! Like the debug socket it is reachable by any process of the same user. Moving the SSH forward
//! binds a different loopback port on the host; the guest and its network are untouched.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use limina_debug::wire;

use crate::gateway::SshForward;

/// How long a client waits for an answer. A move asks gvproxy, which answers in milliseconds.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a push may block on a watcher that stopped reading before it is dropped.
const PUSH_TIMEOUT: Duration = Duration::from_secs(1);

/// The NAT gateway's SSH forward, once `run_vm` has started one.
static FORWARD: Mutex<Option<SshForward>> = Mutex::new(None);

/// The window is parked on a suspended guest (no worker until the play click).
static PARKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Connections that sent `watch`, each owed every change.
static WATCHERS: Mutex<Vec<UnixStream>> = Mutex::new(Vec::new());

/// Socket path to remove on exit (see `control::cleanup` for why this is a static).
static CLEANUP_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// One request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Report the VM's runtime state.
    Info,
    /// Report it, then push every change until the client leaves.
    Watch,
    /// Move the SSH forward to this host port.
    SshPort(u16),
}

impl Request {
    pub fn parse(line: &str) -> Result<Self, String> {
        let mut words = line.split_whitespace();
        let req = match words.next() {
            Some("info") => Request::Info,
            Some("watch") => Request::Watch,
            Some("ssh-port") => {
                let port = words.next().ok_or("ssh-port needs a port")?;
                Request::SshPort(
                    port.parse()
                        .map_err(|_| format!("{port:?} is not a port number"))?,
                )
            }
            Some(other) => return Err(format!("unknown request {other}")),
            None => return Err("empty request".into()),
        };
        if let Some(extra) = words.next() {
            return Err(format!("unexpected {extra:?} after the request"));
        }
        Ok(req)
    }

    pub fn to_line(self) -> String {
        match self {
            Request::Info => "info".into(),
            Request::Watch => "watch".into(),
            Request::SshPort(p) => format!("ssh-port {p}"),
        }
    }
}

/// A running VM as its supervisor reports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Info {
    /// The host port of the guest's SSH forward; `None` without a NAT network (or before its
    /// gateway is up).
    pub ssh_port: Option<u16>,
    /// The guest is suspended and the supervisor's window is parked on it. A supervisor too old
    /// to say reads as not parked.
    pub parked: bool,
}

impl Info {
    fn current() -> Self {
        Info {
            ssh_port: lock(&FORWARD).as_ref().map(SshForward::port),
            parked: PARKED.load(std::sync::atomic::Ordering::SeqCst),
        }
    }

    fn to_lines(self) -> Vec<String> {
        vec![
            match self.ssh_port {
                Some(p) => format!("ssh-port {p}"),
                None => "ssh-port none".into(),
            },
            format!("parked {}", if self.parked { "yes" } else { "no" }),
        ]
    }

    /// Fold one report line in. Lines this build does not know are skipped, so a newer
    /// supervisor can report more without breaking an older client.
    fn apply(&mut self, line: &str) {
        if let Some(v) = line.strip_prefix("ssh-port ") {
            self.ssh_port = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("parked ") {
            self.parked = v.trim() == "yes";
        }
    }

    fn from_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> Self {
        let mut info = Info::default();
        for l in lines {
            info.apply(l);
        }
        info
    }
}

/// Where the supervisor with this pid answers.
pub fn socket_path(pid: u32) -> PathBuf {
    std::env::temp_dir().join(format!("limina-vm-{pid}.sock"))
}

/// Publish the NAT gateway's SSH forward (and tell every watcher).
pub fn set_forward(forward: Option<SshForward>) {
    *lock(&FORWARD) = forward;
    push();
}

/// Publish whether the window is parked on a suspended guest (and tell every watcher).
pub fn set_parked(parked: bool) {
    PARKED.store(parked, std::sync::atomic::Ordering::SeqCst);
    push();
}

/// Answer one request (`watch` is answered in `serve_client`, which also keeps the client).
fn handle(req: Request) -> Result<Vec<String>, String> {
    match req {
        Request::Info | Request::Watch => Ok(Info::current().to_lines()),
        Request::SshPort(port) => {
            let forward = lock(&FORWARD)
                .clone()
                .ok_or("this VM has no NAT network to forward SSH through")?;
            forward.set(port).map_err(|e| format!("{e:#}"))?;
            // The same line the boot printed: scripts that read the log take the last one.
            println!("guest SSH forward ready: ssh -p {port} <user>@127.0.0.1");
            push();
            Ok(Info::current().to_lines())
        }
    }
}

/// Send the current state to every watcher, dropping the ones that are gone.
fn push() {
    let mut text = Info::current().to_lines().join("\n");
    text.push('\n');
    lock(&WATCHERS).retain_mut(|w| w.write_all(text.as_bytes()).is_ok());
}

/// Bind the socket and answer it on a thread for the process's lifetime.
pub fn serve() -> Result<()> {
    let path = socket_path(std::process::id());
    serve_at(&path)?;
    *lock(&CLEANUP_PATH) = Some(path);
    Ok(())
}

fn serve_at(path: &Path) -> Result<()> {
    // A SIGKILLed run with a recycled pid leaves its socket behind; bind over it.
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)
        .with_context(|| format!("binding the runtime socket at {}", path.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    log::info!("runtime: answering at {}", path.display());
    std::thread::Builder::new()
        .name("runtime-socket".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    // One thread per client: a watcher holds its connection for the VM's life.
                    Ok(s) => {
                        let _ = std::thread::Builder::new()
                            .name("runtime-client".into())
                            .spawn(move || serve_client(s));
                    }
                    Err(e) => log::warn!("runtime: accept failed: {e}"),
                }
            }
        })
        .context("spawning the runtime socket thread")?;
    Ok(())
}

fn serve_client(stream: UnixStream) {
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    let mut lines = BufReader::new(stream).lines();
    while let Some(Ok(line)) = lines.next() {
        let req = Request::parse(&line);
        if req == Ok(Request::Watch) {
            // Answer and register under the lock the pushes take, so no change slips between
            // the state this client is told and the first push it gets.
            let mut watchers = lock(&WATCHERS);
            if out
                .write_all(answer(Ok(Info::current().to_lines())).as_bytes())
                .is_err()
            {
                return;
            }
            let _ = out.set_write_timeout(Some(PUSH_TIMEOUT));
            watchers.push(out);
            drop(watchers);
            // Nothing more is read; the connection ends when the client closes it, and the next
            // push drops it then.
            for _ in lines {}
            return;
        }
        if out
            .write_all(answer(req.and_then(handle)).as_bytes())
            .is_err()
        {
            return;
        }
    }
}

/// One answer on the wire: the report lines and `ok`, or the one `err` line.
fn answer(result: Result<Vec<String>, String>) -> String {
    match result {
        Ok(report) => {
            let mut s = String::new();
            for l in report {
                s.push_str(&l);
                s.push('\n');
            }
            s.push_str(wire::OK);
            s.push('\n');
            s
        }
        Err(why) => format!("{}\n", wire::err_line(&why)),
    }
}

/// Remove the socket (idempotent; safe from any exit path).
pub fn cleanup() {
    if let Some(path) = lock(&CLEANUP_PATH).take() {
        let _ = std::fs::remove_file(path);
    }
}

// ---------------------------------------------------------------------------
// Clients: the control center and `limina ssh-port`.
// ---------------------------------------------------------------------------

fn connect(pid: u32) -> Result<UnixStream> {
    connect_at(&socket_path(pid))
        .with_context(|| format!("reaching supervisor {pid} (a build with the runtime socket?)"))
}

fn connect_at(path: &Path) -> Result<UnixStream> {
    let stream =
        UnixStream::connect(path).with_context(|| format!("connecting to {}", path.display()))?;
    stream.set_read_timeout(Some(ANSWER_TIMEOUT))?;
    stream.set_write_timeout(Some(ANSWER_TIMEOUT))?;
    Ok(stream)
}

fn ask(stream: &UnixStream, req: Request) -> Result<Info> {
    (&*stream)
        .write_all(format!("{}\n", req.to_line()).as_bytes())
        .context("sending the request")?;
    let lines = wire::read_answer(&mut BufReader::new(stream))
        .map_err(|why| anyhow::anyhow!("{}: {why}", req.to_line()))?;
    Ok(Info::from_lines(lines.iter().map(String::as_str)))
}

/// Ask the supervisor with this pid one question (`info` or `ssh-port N`).
pub fn request(pid: u32, req: Request) -> Result<Info> {
    ask(&connect(pid)?, req)
}

/// Follow the supervisor with this pid: `on_change` gets its state now and after every change,
/// until the supervisor exits (`Ok`) or the link fails.
pub fn watch(pid: u32, on_change: impl FnMut(Info)) -> Result<()> {
    watch_stream(connect(pid)?, on_change)
}

fn watch_stream(stream: UnixStream, mut on_change: impl FnMut(Info)) -> Result<()> {
    let mut reader = BufReader::new(&stream);
    (&stream)
        .write_all(b"watch\n")
        .context("sending the watch")?;
    let lines = wire::read_answer(&mut reader).map_err(|why| anyhow::anyhow!("watch: {why}"))?;
    let mut info = Info::from_lines(lines.iter().map(String::as_str));
    on_change(info);
    // Pushes come only on change, which may be never.
    stream.set_read_timeout(None)?;
    // A push repeats every line of the report; only the lines that differ are news.
    for line in reader.lines() {
        let before = info;
        info.apply(&line.context("reading the watch")?);
        if info != before {
            on_change(info);
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The tests that publish a forward, which is process-global.
    pub(crate) static FORWARD_TESTS: Mutex<()> = Mutex::new(());

    #[test]
    fn requests_round_trip() {
        for r in [Request::Info, Request::Watch, Request::SshPort(2300)] {
            assert_eq!(Request::parse(&r.to_line()), Ok(r));
        }
    }

    #[test]
    fn malformed_requests_say_what_is_wrong() {
        assert!(Request::parse("").is_err());
        assert!(Request::parse("ssh-port").is_err());
        assert!(Request::parse("ssh-port 70000").is_err());
        assert!(Request::parse("ssh-port 2300 2301").is_err());
        assert!(Request::parse("info please").is_err());
        assert!(Request::parse("reboot").is_err());
    }

    #[test]
    fn info_survives_the_wire_and_ignores_what_it_does_not_know() {
        for info in [
            Info {
                ssh_port: Some(2223),
                parked: false,
            },
            Info {
                ssh_port: None,
                parked: true,
            },
        ] {
            let lines = info.to_lines();
            assert_eq!(Info::from_lines(lines.iter().map(String::as_str)), info);
        }
        let newer = ["vcpus 4", "ssh-port 2400"];
        assert_eq!(Info::from_lines(newer).ssh_port, Some(2400));
    }

    #[test]
    fn moving_ssh_without_a_network_is_refused() {
        let _serial = lock(&FORWARD_TESTS);
        set_forward(None);
        let err = handle(Request::SshPort(2300)).unwrap_err();
        assert!(err.contains("no NAT network"), "{err}");
    }

    #[test]
    fn a_watcher_hears_the_current_state_and_every_change() {
        let _serial = lock(&FORWARD_TESTS);
        let dir = std::env::temp_dir().join(format!("limina-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("w.sock");
        set_forward(None);
        serve_at(&path).unwrap();

        // A plain question first, on its own connection.
        let info = ask(&connect_at(&path).unwrap(), Request::Info).unwrap();
        assert_eq!(info.ssh_port, None);

        let (tx, rx) = std::sync::mpsc::channel();
        let stream = connect_at(&path).unwrap();
        std::thread::spawn(move || {
            let _ = watch_stream(stream, |i| {
                let _ = tx.send(i);
            });
        });
        let wait = |want: Option<u16>| {
            let got = rx.recv_timeout(Duration::from_secs(5)).expect("a report");
            assert_eq!(got.ssh_port, want);
        };
        wait(None);

        // The gateway coming up is a change: a forward with no gvproxy behind it is enough,
        // since publishing it asks gvproxy nothing.
        let forward = crate::gateway::SshForward::for_tests(2299);
        set_forward(Some(forward));
        wait(Some(2299));

        set_forward(None);
        wait(None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
