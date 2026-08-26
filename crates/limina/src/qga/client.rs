// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The I/O half: one port, one outstanding call.
//!
//! `qemu-ga` never speaks unless spoken to, so there is no reader thread here — the whole
//! conversation happens under one mutex, which is also what guarantees a reply can only
//! ever belong to the question directly above it. Everything that can go wrong (no agent
//! installed, an agent that stopped answering, a stale stream from a previous supervisor)
//! collapses into the same two moves: mark the port unsynced, and resynchronize with
//! `guest-sync-delimited` before the next question.
//!
//! Lifetime matches the vdagent broker's: **one client per worker spawn**. A reboot or a
//! resume makes a new port and a new client, so no state survives a boundary it should not.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use super::codec::{self, Lines};
use super::policy::TimeSample;

/// Budget for the commands that answer immediately (everything QGA-1 sends).
pub const FAST: Duration = Duration::from_secs(2);

/// Budget for commands that legitimately take their time — the freeze family, `guest-fstrim`.
/// Nothing sends one yet, but the shape has to be here from the start: a single constant
/// would have made the first slow command look like a dead agent.
#[allow(dead_code)]
pub const SLOW: Duration = Duration::from_secs(30);

/// How long a write may block before we treat the agent as wedged. Same bound, and the same
/// reason, as the vdagent broker: this runs on the shared `limina-timesync` thread, and a
/// guest that stopped draining its port must not take the host's clock sync down with it.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long to leave a silent port alone before probing it again. A stock Debian guest has
/// no `qemu-guest-agent` package at all, so "no answer" is a permanent state there and must
/// cost one bounded probe per interval, not one per tick.
const PROBE_RETRY: Duration = Duration::from_secs(30);

/// Log every request and reply (`LIMINA_QGA_TRACE=1`).
fn trace_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LIMINA_QGA_TRACE").is_some_and(|v| v != "0"))
}

/// What the agent said it can do, from `guest-info`. The gate for every command we send:
/// the guest's own config decides what is enabled, and partial availability is normal.
#[derive(Debug, Clone, Default)]
pub struct Caps {
    pub version: String,
    pub commands: BTreeSet<String>,
}

impl Caps {
    pub fn has(&self, cmd: &str) -> bool {
        self.commands.contains(cmd)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// No probe has succeeded or failed yet.
    Unknown,
    /// `guest-info` answered; `caps` is populated.
    Ready,
    /// Nothing answered. Try again after the instant.
    Silent,
}

struct Conn {
    sock: UnixStream,
    lines: Lines,
    /// Complete replies read but not yet handed out. At most one call is ever outstanding,
    /// so this only ever holds a stale answer — but dropping those silently is how a client
    /// hands the *next* caller someone else's reply.
    pending: std::collections::VecDeque<Vec<u8>>,
    next_id: i64,
    synced: bool,
    state: State,
    caps: Caps,
    retry_at: Instant,
}

/// A live `org.qemu.guest_agent.0` conversation.
pub struct Qga {
    conn: Mutex<Conn>,
}

impl Qga {
    /// Adopt the host end of the port. Does **not** probe: at spawn time the guest has not
    /// booted, so a probe here would only burn its timeout and mark a healthy agent silent.
    /// The first call does the probe, and every caller is already prepared for it to fail.
    pub fn start(host_fd: OwnedFd) -> Result<Qga> {
        let sock = UnixStream::from(host_fd);
        sock.set_write_timeout(Some(WRITE_TIMEOUT))
            .context("bounding writes to the qemu-guest-agent port")?;
        Ok(Qga {
            conn: Mutex::new(Conn {
                sock,
                lines: Lines::new(),
                pending: std::collections::VecDeque::new(),
                next_id: 1,
                synced: false,
                state: State::Unknown,
                caps: Caps::default(),
                retry_at: Instant::now(),
            }),
        })
    }

    /// Is there an agent answering on this port right now (as of the last probe)?
    #[allow(dead_code)]
    pub fn ready(&self) -> bool {
        self.conn.lock().unwrap().state == State::Ready
    }

    /// What the agent reported it supports, empty until a probe succeeds.
    #[allow(dead_code)]
    pub fn caps(&self) -> Caps {
        self.conn.lock().unwrap().caps.clone()
    }

    /// Run one command and return its `return` value.
    ///
    /// Fails when there is no agent, when the port misbehaves, **and** when the agent
    /// answers with an error — a blocked or unimplemented RPC is a failed call, not a
    /// broken port, and the message says which.
    pub fn call(&self, cmd: &str, args: Option<Value>, timeout: Duration) -> Result<Value> {
        let mut conn = self.conn.lock().unwrap();
        ensure_ready(&mut conn)?;
        if !conn.caps.commands.is_empty() && !conn.caps.has(cmd) {
            bail!("the guest agent does not offer {cmd}");
        }
        exec(&mut conn, cmd, args, timeout)
    }

    /// Send a command that answers nothing on success (`guest-shutdown`, `guest-suspend-*`).
    /// There is no reply to wait for, so a "success" here means only that the bytes left.
    #[allow(dead_code)]
    pub fn fire(&self, cmd: &str, args: Option<Value>) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        ensure_ready(&mut conn)?;
        write_all(&mut conn, &codec::request(cmd, args))
    }

    /// Measure the guest's clock against the host's, bracketing the round trip so the
    /// estimate carries its own error bar (see [`super::policy`]).
    pub fn time_sample(&self) -> Result<TimeSample> {
        let mut conn = self.conn.lock().unwrap();
        ensure_ready(&mut conn)?;
        let t0 = SystemTime::now();
        let ret = exec(&mut conn, "guest-get-time", None, FAST)?;
        let t1 = SystemTime::now();
        let guest_ns = ret
            .as_i64()
            .ok_or_else(|| anyhow!("guest-get-time returned {ret}, not a number"))?;
        let rtt = t1.duration_since(t0).unwrap_or_default();
        Ok(TimeSample {
            host_ns: unix_ns(t0) + rtt.as_nanos() as i64 / 2,
            guest_ns,
            rtt,
        })
    }

    /// Set the guest's clock to `unix_ns`. The agent sets `CLOCK_REALTIME` and then pushes
    /// the value at the RTC, so a guest that only consults its RTC on resume lands there too.
    pub fn set_time(&self, unix_ns: i64) -> Result<()> {
        self.call("guest-set-time", Some(json!({ "time": unix_ns })), FAST)
            .map(|_| ())
    }
}

/// Host wallclock as nanoseconds since the epoch.
pub fn unix_ns(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Bring the port to a state where a question can be asked, probing at most once per
/// [`PROBE_RETRY`] while nothing answers.
fn ensure_ready(conn: &mut Conn) -> Result<()> {
    if conn.state == State::Ready && conn.synced {
        return Ok(());
    }
    if conn.state == State::Silent && Instant::now() < conn.retry_at {
        bail!("no guest agent on org.qemu.guest_agent.0 (waiting to re-probe)");
    }

    match probe(conn) {
        Ok(caps) => {
            if conn.state != State::Ready {
                log::info!(
                    "qga: guest agent {} answered on org.qemu.guest_agent.0 ({} commands)",
                    if caps.version.is_empty() {
                        "(unknown version)"
                    } else {
                        &caps.version
                    },
                    caps.commands.len()
                );
            }
            conn.caps = caps;
            conn.state = State::Ready;
            Ok(())
        }
        Err(e) => {
            // Said once per transition, at info: on a guest with no qemu-guest-agent
            // installed (Debian) this is the permanent, correct state, and repeating it
            // every tick would bury the log that explains a real incident.
            if conn.state != State::Silent {
                log::info!(
                    "qga: no guest agent answered on org.qemu.guest_agent.0 ({e:#}); \
                     re-probing every {}s",
                    PROBE_RETRY.as_secs()
                );
            } else {
                log::debug!("qga: still silent ({e:#})");
            }
            conn.state = State::Silent;
            conn.caps = Caps::default();
            conn.retry_at = Instant::now() + PROBE_RETRY;
            Err(e)
        }
    }
}

/// Resynchronize the stream, then ask what the agent supports.
fn probe(conn: &mut Conn) -> Result<Caps> {
    sync(conn)?;
    let info = exec(conn, "guest-info", None, FAST)?;
    let version = info
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let commands = info
        .get("supported_commands")
        .and_then(Value::as_array)
        .map(|cmds| {
            cmds.iter()
                .filter(|c| c.get("enabled").and_then(Value::as_bool).unwrap_or(false))
                .filter_map(|c| c.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Ok(Caps { version, commands })
}

/// `guest-sync-delimited`: the only way to know the stream is ours again. Everything the
/// agent wrote before the sentinel belongs to a question we are no longer waiting for.
fn sync(conn: &mut Conn) -> Result<()> {
    let id = conn.next_id;
    conn.next_id += 1;
    conn.lines.reset();
    conn.pending.clear();
    write_all(conn, &codec::sync_request(id))?;

    let deadline = Instant::now() + FAST;
    loop {
        let line = read_line(conn, deadline)?;
        match codec::parse_reply(&line)? {
            Ok(v) if v.as_i64() == Some(id) => {
                conn.synced = true;
                return Ok(());
            }
            // A reply from before the sentinel cannot reach us (the codec drops it), so
            // anything else here is an out-of-order answer — keep reading for ours.
            other => log::debug!("qga: discarding a reply while syncing: {other:?}"),
        }
    }
}

/// One request, one reply, on an already-synced port.
fn exec(conn: &mut Conn, cmd: &str, args: Option<Value>, timeout: Duration) -> Result<Value> {
    if !conn.synced {
        sync(conn)?;
    }
    if trace_on() {
        eprintln!("[QGA] -> {cmd} {}", args.clone().unwrap_or(json!(null)));
    }
    write_all(conn, &codec::request(cmd, args))?;

    let deadline = Instant::now() + timeout;
    let line = read_line(conn, deadline).inspect_err(|_| {
        // The answer may still be in flight; it must not be mistaken for the next
        // command's. Force a resync before anything else is asked.
        conn.synced = false;
        conn.lines.reset();
        conn.pending.clear();
    })?;
    if trace_on() {
        eprintln!("[QGA] <- {}", String::from_utf8_lossy(&line));
    }
    match codec::parse_reply(&line)? {
        Ok(v) => Ok(v),
        Err(e) => Err(e.into()),
    }
}

fn write_all(conn: &mut Conn, bytes: &[u8]) -> Result<()> {
    if let Err(e) = conn.sock.write_all(bytes) {
        // A partial write leaves the agent's parser mid-message; the next sentinel is what
        // repairs that, so make sure one is sent.
        conn.synced = false;
        conn.state = State::Silent;
        conn.retry_at = Instant::now() + PROBE_RETRY;
        return Err(anyhow!("writing to the guest agent port: {e}"));
    }
    Ok(())
}

/// Read until one complete reply line is available, or the deadline passes.
fn read_line(conn: &mut Conn, deadline: Instant) -> Result<Vec<u8>> {
    if let Some(line) = conn.pending.pop_front() {
        return Ok(line);
    }
    let mut buf = [0u8; 4096];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!("the guest agent did not answer in time");
        }
        // A zero timeout means "block forever" to the kernel, so never let it round to that.
        conn.sock
            .set_read_timeout(Some(left.max(Duration::from_millis(1))))
            .context("bounding a read from the guest agent port")?;
        let n = match conn.sock.read(&mut buf) {
            Ok(0) => bail!("the guest agent port closed"),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                bail!("the guest agent did not answer in time")
            }
            Err(e) => bail!("reading from the guest agent port: {e}"),
        };
        let fresh = conn.lines.push(&buf[..n])?;
        conn.pending.extend(fresh);
        if let Some(line) = conn.pending.pop_front() {
            return Ok(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;

    /// A hand-driven `qemu-ga`: real JSON on a real socket, so these tests exercise the
    /// framing, the sentinel, the timeouts and the caching together rather than a mock of
    /// our own protocol understanding.
    fn spawn_agent<F>(stream: UnixStream, mut answer: F) -> std::thread::JoinHandle<()>
    where
        F: FnMut(&str, &Value) -> Option<Value> + Send + 'static,
    {
        std::thread::spawn(move || {
            let mut out = stream.try_clone().unwrap();
            let mut reader = std::io::BufReader::new(stream);
            loop {
                let mut line = Vec::new();
                match reader.read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                // The agent feeds bytes to a JSON parser; our leading sentinel is what
                // breaks it out of a partial message. Strip it the way that parser would.
                let cleaned: Vec<u8> = line
                    .iter()
                    .copied()
                    .filter(|&b| b != codec::SENTINEL)
                    .collect();
                let Ok(req) = serde_json::from_slice::<Value>(&cleaned) else {
                    continue;
                };
                let cmd = req["execute"].as_str().unwrap_or_default().to_string();
                let args = req.get("arguments").cloned().unwrap_or(json!({}));
                if cmd == "guest-sync-delimited" {
                    let mut bytes = vec![codec::SENTINEL];
                    bytes.extend_from_slice(json!({ "return": args["id"] }).to_string().as_bytes());
                    bytes.push(b'\n');
                    if out.write_all(&bytes).is_err() {
                        return;
                    }
                    continue;
                }
                if let Some(reply) = answer(&cmd, &args) {
                    let mut bytes = reply.to_string().into_bytes();
                    bytes.push(b'\n');
                    if out.write_all(&bytes).is_err() {
                        return;
                    }
                }
            }
        })
    }

    fn info_reply(commands: &[&str]) -> Value {
        json!({
            "return": {
                "version": "10.1.0",
                "supported_commands": commands.iter().map(|c| json!({
                    "name": c, "enabled": true, "success-response": true
                })).collect::<Vec<_>>(),
            }
        })
    }

    fn client_with<F>(answer: F) -> (Qga, std::thread::JoinHandle<()>)
    where
        F: FnMut(&str, &Value) -> Option<Value> + Send + 'static,
    {
        let (host, guest) = UnixStream::pair().unwrap();
        let handle = spawn_agent(guest, answer);
        (Qga::start(host.into()).unwrap(), handle)
    }

    #[test]
    fn a_probe_learns_the_agents_commands() {
        let (qga, _agent) = client_with(|cmd, _| match cmd {
            "guest-info" => Some(info_reply(&["guest-ping", "guest-get-time"])),
            "guest-ping" => Some(json!({ "return": {} })),
            _ => None,
        });
        assert!(!qga.ready(), "no probe has run yet");
        qga.call("guest-ping", None, FAST).unwrap();
        assert!(qga.ready());
        let caps = qga.caps();
        assert_eq!(caps.version, "10.1.0");
        assert!(caps.has("guest-get-time"));
        assert!(!caps.has("guest-exec"));
    }

    #[test]
    fn a_command_the_agent_blocked_is_refused_without_asking() {
        // Fedora ships every RPC enabled, but a hardened guest blocks some; `guest-info`
        // reports that, and a milestone that assumes otherwise must fail loudly here rather
        // than hang waiting for a reply that never comes.
        let (qga, _agent) = client_with(|cmd, _| match cmd {
            "guest-info" => Some(info_reply(&["guest-ping"])),
            "guest-ping" => Some(json!({ "return": {} })),
            _ => None,
        });
        qga.call("guest-ping", None, FAST).unwrap();
        let err = qga.call("guest-exec", None, FAST).unwrap_err();
        assert!(
            err.to_string().contains("does not offer guest-exec"),
            "{err}"
        );
    }

    #[test]
    fn an_agent_error_reply_is_an_error_not_a_dead_port() {
        let (qga, _agent) = client_with(|cmd, _| match cmd {
            "guest-info" => Some(info_reply(&["guest-set-time"])),
            "guest-set-time" => Some(json!({
                "error": { "class": "GenericError", "desc": "hwclock failed" }
            })),
            _ => None,
        });
        let err = qga.set_time(1).unwrap_err();
        assert!(err.to_string().contains("hwclock failed"), "{err}");
        // The port itself is fine, so the next call still works.
        assert!(qga.ready());
    }

    #[test]
    fn the_time_sample_carries_the_guests_answer_and_its_error_bar() {
        let guest_ns = 1_700_000_000_000_000_000i64;
        let (qga, _agent) = client_with(move |cmd, _| match cmd {
            "guest-info" => Some(info_reply(&["guest-get-time"])),
            "guest-get-time" => Some(json!({ "return": guest_ns })),
            _ => None,
        });
        let s = qga.time_sample().unwrap();
        assert_eq!(s.guest_ns, guest_ns);
        assert!(s.host_ns > guest_ns, "the host is not in 2023");
        assert!(s.rtt < FAST);
    }

    #[test]
    fn a_silent_port_fails_fast_after_the_first_probe() {
        // Nothing answers — a Debian guest with no qemu-guest-agent package. The first
        // attempt pays the probe timeout; the next must not.
        let (host, _guest) = UnixStream::pair().unwrap();
        let qga = Qga::start(host.into()).unwrap();
        let t0 = Instant::now();
        assert!(qga.call("guest-ping", None, FAST).is_err());
        let probed = t0.elapsed();
        assert!(probed >= FAST, "the probe should have waited: {probed:?}");

        let t1 = Instant::now();
        assert!(qga.call("guest-ping", None, FAST).is_err());
        assert!(
            t1.elapsed() < Duration::from_millis(500),
            "a silent port must not re-probe on every call: {:?}",
            t1.elapsed()
        );
        assert!(!qga.ready());
    }

    #[test]
    fn a_late_reply_cannot_be_mistaken_for_the_next_answer() {
        // The trap this protocol has no ids for: a command times out, its answer arrives
        // afterwards, and a naive client hands it to the *next* caller. The resync is what
        // makes that impossible — after a timeout the stale reply is dropped at the
        // sentinel.
        let (qga, _agent) = client_with(|cmd, _| match cmd {
            "guest-info" => Some(info_reply(&["guest-ping", "guest-get-time"])),
            // Answer `guest-ping` far too late, with the *wrong* shape for a time reply.
            "guest-ping" => {
                std::thread::sleep(Duration::from_millis(400));
                Some(json!({ "return": {"stale": true} }))
            }
            "guest-get-time" => Some(json!({ "return": 1_700_000_000_000_000_000i64 })),
            _ => None,
        });
        qga.time_sample().unwrap();
        assert!(qga
            .call("guest-ping", None, Duration::from_millis(50))
            .is_err());
        let s = qga
            .time_sample()
            .expect("the stale ping reply must not land here");
        assert_eq!(s.guest_ns, 1_700_000_000_000_000_000i64);
    }
}
