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
//! `input <verb…>` lines inject keyboard and pointer events into the guest's virtio-input
//! devices (`crate::inject`, whose `HELP` lists the verbs); `limina input` is their client.
//! They are refused unless the VM's `input-inject` lever is on (`debug_ctl::INPUT_INJECT`, off
//! by default, read per request). With it on, any process of the same user can type into the
//! guest through this socket, with none of the Accessibility (TCC) grant that osascript-driven
//! input needs.
//!
//! `reset` kills the VM's worker and cold-boots a fresh one behind the same supervisor, window,
//! gateway and SSH port (`supervisor::request_reset`); `limina reset` is its client. The answer
//! comes once the fresh worker is running (`worker-pid <pid>`). It is refused unless the VM's
//! `lifecycle-control` lever is on (`debug_ctl::LIFECYCLE_CONTROL`, off by default, read per
//! request). Stopping a VM needs no lever and no socket: it is a signal to the supervisor, which
//! any process of the same user can already send (`limina stop`).
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

/// The run's boot disk, canonicalized: what `limina <verb> <disk>` matches a run by.
static DISK: Mutex<Option<PathBuf>> = Mutex::new(None);

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
    /// Kill the worker and cold-boot a fresh one (`lifecycle-control` lever).
    Reset,
}

impl Request {
    pub fn parse(line: &str) -> Result<Self, String> {
        let mut words = line.split_whitespace();
        let req = match words.next() {
            Some("info") => Request::Info,
            Some("watch") => Request::Watch,
            Some("reset") => Request::Reset,
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
            Request::Reset => "reset".into(),
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

/// Publish the run's boot disk (canonical), reported as `disk <path>` after the state lines. It
/// never changes during a run, so it is set once, before the socket is bound.
pub fn set_disk(disk: Option<PathBuf>) {
    *lock(&DISK) = disk;
}

/// Every line an `info` answers with: the state, then the run's identity.
fn report() -> Vec<String> {
    let mut lines = Info::current().to_lines();
    if let Some(disk) = lock(&DISK).as_ref() {
        lines.push(format!("disk {}", disk.display()));
    }
    lines
}

/// Publish whether the window is parked on a suspended guest (and tell every watcher).
pub fn set_parked(parked: bool) {
    PARKED.store(parked, std::sync::atomic::Ordering::SeqCst);
    push();
}

/// How long a `reset` answer waits for the fresh worker. Its spawn goes through launchd and
/// recycles gvproxy, which takes about a second.
const RESET_TIMEOUT: Duration = Duration::from_secs(60);

/// The answer to `reset` while the `lifecycle-control` lever is off.
pub fn reset_refusal() -> String {
    format!(
        "lifecycle control is off for this VM; to allow `limina reset`, {}",
        crate::debug_ctl::how_to_enable(&crate::debug_ctl::LIFECYCLE_CONTROL)
    )
}

/// Reset the VM and wait for the fresh worker: the request is answered once it can be relied on.
fn reset() -> Result<Vec<String>, String> {
    if !crate::debug_ctl::LIFECYCLE_CONTROL.on() {
        return Err(reset_refusal());
    }
    if PARKED.load(std::sync::atomic::Ordering::SeqCst) {
        return Err("the guest is suspended: there is no worker to reset".into());
    }
    let old = crate::supervisor::request_reset()?;
    log::warn!("runtime: reset requested (worker {old})");
    let deadline = std::time::Instant::now() + RESET_TIMEOUT;
    loop {
        let now = crate::supervisor::worker_pid();
        if now > 0 && now != old {
            return Ok(vec![format!("worker-pid {now}")]);
        }
        if crate::supervisor::stop_requested() {
            return Err("the VM stopped instead of relaunching".into());
        }
        if crate::supervisor::reset_skipped() {
            return Err("not reset: a stop or suspend of this VM got there first".into());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "no fresh worker within {RESET_TIMEOUT:?} of the reset; the VM log says why"
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Answer one request (`watch` is answered in `serve_client`, which also keeps the client).
fn handle(req: Request) -> Result<Vec<String>, String> {
    match req {
        Request::Reset => reset(),
        Request::Info | Request::Watch => Ok(report()),
        Request::SshPort(port) => {
            let forward = lock(&FORWARD)
                .clone()
                .ok_or("this VM has no NAT network to forward SSH through")?;
            forward.set(port).map_err(|e| format!("{e:#}"))?;
            // The same line the boot printed: scripts that read the log take the last one.
            println!("guest SSH forward ready: ssh -p {port} <user>@127.0.0.1");
            push();
            Ok(report())
        }
    }
}

/// Send the current state to every watcher, dropping the ones that are gone.
fn push() {
    let mut text = report().join("\n");
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

/// The longest request line a client may send. Generous for a `type` line, and what keeps a
/// client that never sends a newline from growing the supervisor's buffer without bound.
const MAX_LINE: u64 = 64 * 1024;

/// A reader's lines, ending (with an error) at the first one longer than [`MAX_LINE`].
fn capped_lines<R: BufRead>(mut reader: R) -> impl Iterator<Item = std::io::Result<String>> {
    use std::io::Read;
    std::iter::from_fn(move || {
        let mut buf = String::new();
        match reader.by_ref().take(MAX_LINE + 1).read_line(&mut buf) {
            Ok(0) => None,
            Ok(_) if !buf.ends_with('\n') && buf.len() as u64 > MAX_LINE => Some(Err(
                std::io::Error::new(std::io::ErrorKind::InvalidData, "request line too long"),
            )),
            Ok(_) => {
                let line = buf.strip_suffix('\n').unwrap_or(&buf);
                Some(Ok(line.strip_suffix('\r').unwrap_or(line).to_string()))
            }
            Err(e) => Some(Err(e)),
        }
    })
}

pub(crate) fn serve_client(stream: UnixStream) {
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    let mut lines = capped_lines(BufReader::new(stream));
    // This connection's injected input: what it holds is released when it drops, on every way
    // out of this function (`inject::Session`).
    let mut input = crate::inject::Session::default();
    while let Some(Ok(line)) = lines.next() {
        if let Some(verb) = line
            .strip_prefix(crate::inject::PREFIX)
            .filter(|rest| rest.is_empty() || rest.starts_with(' '))
        {
            // The lever's refusal comes first: `run` answers it, parked or not.
            let result = if crate::debug_ctl::INPUT_INJECT.on()
                && PARKED.load(std::sync::atomic::Ordering::SeqCst)
            {
                Err("the guest is suspended: there is no worker to take input".to_string())
            } else {
                input.run(verb)
            };
            if out.write_all(answer(result).as_bytes()).is_err() {
                return;
            }
            continue;
        }
        let req = Request::parse(&line);
        if req == Ok(Request::Watch) {
            // Answer and register under the lock the pushes take, so no change slips between
            // the state this client is told and the first push it gets.
            let mut watchers = lock(&WATCHERS);
            if out.write_all(answer(Ok(report())).as_bytes()).is_err() {
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

/// Who a supervisor says it is: its run's boot disk (canonical; `None` from a run without one, or
/// a build too old to say) and whether it is parked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub disk: Option<PathBuf>,
    pub parked: bool,
}

impl Identity {
    fn from_lines<'a>(lines: impl IntoIterator<Item = &'a str> + Clone) -> Self {
        Identity {
            disk: lines
                .clone()
                .into_iter()
                .find_map(|l| l.strip_prefix("disk "))
                .map(PathBuf::from),
            parked: Info::from_lines(lines).parked,
        }
    }
}

/// Ask the supervisor with this pid who it is. An error means nothing answers there: no
/// supervisor, whatever the pid is now.
pub fn identity(pid: u32) -> Result<Identity> {
    let stream = connect(pid)?;
    (&stream)
        .write_all(format!("{}\n", Request::Info.to_line()).as_bytes())
        .context("sending the request")?;
    let lines = wire::read_answer(&mut BufReader::new(&stream))
        .map_err(|why| anyhow::anyhow!("info: {why}"))?;
    Ok(Identity::from_lines(lines.iter().map(String::as_str)))
}

/// The pids whose runtime socket exists in this user's temp dir: every supervisor that might be
/// running. A socket left by a SIGKILLed run is among them, which is why a caller asks each
/// ([`identity`]) before believing it.
pub fn socket_pids() -> Vec<u32> {
    let Ok(dir) = std::fs::read_dir(std::env::temp_dir()) else {
        return Vec::new();
    };
    dir.filter_map(|e| {
        let name = e.ok()?.file_name();
        name.to_str()?
            .strip_prefix("limina-vm-")?
            .strip_suffix(".sock")?
            .parse()
            .ok()
    })
    .collect()
}

/// `limina reset`: ask the supervisor with this pid to power-cycle its VM, and return the fresh
/// worker's pid once it is running.
pub fn request_reset(pid: u32) -> Result<u32> {
    let stream = connect(pid)?;
    stream.set_read_timeout(Some(RESET_TIMEOUT + ANSWER_TIMEOUT))?;
    (&stream)
        .write_all(format!("{}\n", Request::Reset.to_line()).as_bytes())
        .context("sending the request")?;
    let lines = wire::read_answer(&mut BufReader::new(&stream))
        .map_err(|why| anyhow::anyhow!("reset: {why}"))?;
    lines
        .iter()
        .find_map(|l| l.strip_prefix("worker-pid ")?.trim().parse().ok())
        .with_context(|| format!("reset: no worker pid in the answer {lines:?}"))
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
        for r in [
            Request::Info,
            Request::Watch,
            Request::SshPort(2300),
            Request::Reset,
        ] {
            assert_eq!(Request::parse(&r.to_line()), Ok(r));
        }
    }

    #[test]
    fn an_endless_request_line_ends_the_connection() {
        let long = "x".repeat(MAX_LINE as usize + 10);
        let input = format!("info\r\nwatch\n{long}\ninfo\n");
        let got: Vec<_> = capped_lines(input.as_bytes()).collect();
        assert_eq!(got[0].as_deref().ok(), Some("info"));
        assert_eq!(got[1].as_deref().ok(), Some("watch"));
        assert!(got[2].is_err(), "the oversized line is refused");
        let ok = format!("{}\n", "y".repeat(MAX_LINE as usize - 1));
        assert_eq!(capped_lines(ok.as_bytes()).count(), 1);
    }

    #[test]
    fn malformed_requests_say_what_is_wrong() {
        assert!(Request::parse("").is_err());
        assert!(Request::parse("ssh-port").is_err());
        assert!(Request::parse("ssh-port 70000").is_err());
        assert!(Request::parse("ssh-port 2300 2301").is_err());
        assert!(Request::parse("info please").is_err());
        assert!(Request::parse("reboot").is_err());
        assert!(Request::parse("reset now").is_err());
    }

    /// `reset` is refused while the lever is off, with the way to turn it on, and only `reset`
    /// is; with the lever on it gets as far as asking for a worker, which a unit test has none
    /// of. The lever is read per request, so a toggle applies to the next line.
    #[test]
    fn reset_follows_the_lifecycle_lever_and_nothing_else_does() {
        let _levers = lock(&crate::debug_ctl::ACCESS_LEVER_TESTS);
        let _forward = lock(&FORWARD_TESTS);
        let dir = std::env::temp_dir().join(format!("limina-rt-reset-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("r.sock");
        set_forward(None);
        serve_at(&path).unwrap();
        let stream = connect_at(&path).unwrap();
        let mut reader = BufReader::new(&stream);
        let mut say = |line: &str| {
            (&stream).write_all(format!("{line}\n").as_bytes()).unwrap();
            wire::read_answer(&mut reader)
        };
        let lever = |on| {
            crate::debug_ctl::handle(&limina_debug::wire::Request::Lever {
                name: "lifecycle-control".into(),
                on,
            })
            .unwrap()
        };

        let refused = say("reset").unwrap_err();
        assert_eq!(refused, reset_refusal());
        assert!(
            refused.contains("LIMINA_LIFECYCLE_CONTROL=1")
                && refused.contains("limina debug <vm> lever lifecycle-control on"),
            "{refused}"
        );
        assert!(say("info").is_ok(), "info is not gated");
        lever(true);
        let past_the_gate = say("reset").unwrap_err();
        assert!(past_the_gate.contains("no worker"), "{past_the_gate}");
        lever(false);
        assert_eq!(say("reset"), Err(reset_refusal()));
        let _ = std::fs::remove_dir_all(&dir);
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
        let id = Identity::from_lines(["ssh-port none", "parked yes", "disk /v/a b.raw"]);
        assert_eq!(id.disk, Some(PathBuf::from("/v/a b.raw")));
        assert!(id.parked);
        assert_eq!(Identity::from_lines(["parked no"]).disk, None);
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

    /// `input …` is refused while the lever is off, and only `input …`; the lever is read per
    /// request, so a toggle applies to the next line on the same connection.
    #[test]
    fn input_requests_follow_the_lever_and_nothing_else_does() {
        let _levers = lock(&crate::debug_ctl::ACCESS_LEVER_TESTS);
        let _forward = lock(&FORWARD_TESTS);
        let dir = std::env::temp_dir().join(format!("limina-rt-gate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("g.sock");
        set_forward(None);
        serve_at(&path).unwrap();
        let stream = connect_at(&path).unwrap();
        let mut reader = BufReader::new(&stream);
        let mut say = |line: &str| {
            (&stream).write_all(format!("{line}\n").as_bytes()).unwrap();
            wire::read_answer(&mut reader)
        };
        let lever = |on| {
            crate::debug_ctl::handle(&limina_debug::wire::Request::Lever {
                name: "input-inject".into(),
                on,
            })
            .unwrap()
        };

        assert_eq!(say("input info"), Err(crate::inject::refusal()));
        assert_eq!(say("input"), Err(crate::inject::refusal()));
        assert!(say("info").is_ok(), "info is not gated");
        lever(true);
        let report = say("input info").expect("allowed once the lever is on");
        assert!(
            report.iter().any(|l| l.starts_with("abs-max ")),
            "{report:?}"
        );
        lever(false);
        assert_eq!(say("input info"), Err(crate::inject::refusal()));
        let _ = std::fs::remove_dir_all(&dir);
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
