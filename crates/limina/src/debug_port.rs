// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The debug port: a guest asks which host build it runs on, with stock tools.
//!
//! Every worker spawn puts a named virtio-serial port, `org.limina.debug.0`, on the guest's
//! virtio-console device (`crates/limina-vmm/src/krun/console.rs`); the supervisor holds the
//! other end of its socketpair and answers here. A stock guest sees
//! `/dev/virtio-ports/org.limina.debug.0` with nothing of ours installed, so a test harness
//! running inside any guest can attribute a result to the host build that produced it. The
//! interface — requests, keys, the stock one-liner — is specified in `docs/design/debug-port.md`.
//!
//! **Request/response, never push.** The host cannot see the guest open or close the port:
//! libkrun turns the guest's `VIRTIO_CONSOLE_PORT_OPEN` into "start moving bytes" and nothing
//! reaches this socket (`virtio/console/device.rs`, the `PORT_OPEN` arm), and it marks every
//! port host-connected at `PORT_READY`, so a guest read blocks rather than seeing EOF. A blob
//! pushed at spawn would sit in the socket until the *first* opener drained it, and every later
//! reader would block forever. So the guest writes a request line and the host answers every
//! line, unknown ones included, with a framed answer.
//!
//! **Framing.** An answer is `key=value` lines, the first always `format=<n>`, terminated by a
//! line holding a lone `.` (the only line without an `=`). A reader skips anything before
//! `format=`: the port is a byte stream that outlives its readers, and one that closed halfway
//! through an answer leaves the tail for the next opener.
//!
//! **Why `key=value` and not JSON.** The reader is a stock guest — maybe a minimal one with no
//! `jq` or Python — and bash's `read` and `case` parse this with nothing else. Values are the
//! rest of the line (spaces allowed, newlines never); keys are fixed here. Readers must ignore
//! keys they do not know, which is what lets later facts be appended without a format bump;
//! `format` changes only for a change an old reader would misread.
//!
//! **Off by default.** The port is on the bus for every launch whatever the setting — the
//! device set must not depend on it, or a snapshot taken with it one way would not restore with
//! it the other. What the setting decides is the answer: unless the `debug-port` lever is on
//! (`crate::debug_ctl::DEBUG_PORT`), every request, `help` included, gets [`disabled`] — the
//! format, `error=disabled`, how to enable it, and nothing about the build or the host. The lever
//! is read per request, so a change applies to the next one.

use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::OnceLock;
use std::time::Duration;

/// The answer format. Bumped only for a change an existing reader would misread; new keys do
/// not bump it.
pub const FORMAT: u32 = 1;

/// The line that ends every answer.
pub const END: &str = ".";

/// The requests this port answers.
const REQUESTS: &str = "identity help";

/// A request longer than this is not a request; it is discarded and answered with an error.
const MAX_REQUEST: usize = 256;

/// How long an answer may wait on a guest that stopped reading. The responder has a thread of
/// its own, so this only bounds how long one abandoned answer holds it.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// What this supervisor run is: fixed for every worker it spawns, reboots and resumes included.
#[derive(Clone, Debug, Default)]
pub struct RunFacts {
    /// `managed` (`limina start <vm>`) or `flat` (`limina --disk …`).
    pub vm_kind: &'static str,
    /// The managed VM's name, or the first disk's file name for a flat run.
    pub vm: String,
    /// `efi` (firmware → the guest's own bootloader) or `kernel` (`--kernel` direct boot).
    pub boot: &'static str,
    /// The virtio-gpu the guest gets: `coexist` (software-2D + venus/vrend), `software-2d`, or
    /// `none` (a headless run with no display device).
    pub gpu: &'static str,
    /// Where the scanout goes: `window`, `capture` (`--display-capture`), or `none`.
    pub display: &'static str,
    /// Scanouts on the virtio-gpu (`--display-pool`); 0 without a display device.
    pub display_pool: u32,
    pub cpus: u8,
    /// Guest RAM as allocated (the maximum of a dynamic-memory range).
    pub ram_mib: usize,
}

static RUN: OnceLock<RunFacts> = OnceLock::new();

/// Record this run's facts. Called once, before the first spawn; every later spawn reads them.
pub fn set_run_facts(facts: RunFacts) {
    let _ = RUN.set(facts);
}

/// What changes per worker spawn.
#[derive(Clone, Debug)]
pub struct Launch {
    /// Fresh for every worker launch — cold boot, reboot relaunch and resume alike.
    pub launch_id: String,
    /// This launch restored a suspended VM rather than booting it.
    pub resumed: bool,
    pub supervisor_pid: u32,
    pub worker_pid: u32,
}

impl Launch {
    pub fn new(resumed: bool, worker_pid: u32) -> Launch {
        Launch {
            launch_id: crate::vmlib::schema::uuid_v4(),
            resumed,
            supervisor_pid: std::process::id(),
            worker_pid,
        }
    }
}

/// The host this runs on.
#[derive(Clone, Debug)]
pub struct Host {
    /// `macOS <product version>`.
    pub os: String,
    /// The hardware model identifier (`hw.model`, e.g. `Mac14,6`).
    pub model: String,
}

impl Host {
    fn current() -> Host {
        let os = sysctl_string(c"kern.osproductversion")
            .map(|v| format!("macOS {v}"))
            .unwrap_or_else(|| "unknown".into());
        Host {
            os,
            model: sysctl_string(c"hw.model").unwrap_or_else(|| "unknown".into()),
        }
    }
}

/// Everything one launch's identity answer carries.
pub struct Identity {
    pub build: crate::about::BuildInfo,
    pub host: Host,
    pub run: RunFacts,
    pub launch: Launch,
}

impl Identity {
    /// This process's identity for a worker just spawned.
    pub fn for_launch(launch: Launch) -> Identity {
        Identity {
            build: crate::about::build_info(),
            host: Host::current(),
            run: RUN.get().cloned().unwrap_or_default(),
            launch,
        }
    }

    /// The identity as ordered `(key, value)` pairs, `format` first. New keys go at the end.
    pub fn fields(&self) -> Vec<(String, String)> {
        let mut f: Vec<(String, String)> = Vec::new();
        let mut put = |k: &str, v: String| f.push((k.to_string(), v));
        put("format", FORMAT.to_string());
        put("limina_version", self.build.version.to_string());
        put("limina_git_rev", self.build.git_rev.to_string());
        put("limina_build_date", self.build.built.to_string());
        for dep in &self.build.deps {
            put(&format!("dep.{}", dep.name), dep.rev.clone());
        }
        put("launch_id", self.launch.launch_id.clone());
        put("resumed", yes_no(self.launch.resumed).into());
        put("vm_kind", or_unknown(self.run.vm_kind));
        put("vm", self.run.vm.clone());
        put("boot", or_unknown(self.run.boot));
        put("gpu", or_unknown(self.run.gpu));
        put("display", or_unknown(self.run.display));
        put("display_pool", self.run.display_pool.to_string());
        put("cpus", self.run.cpus.to_string());
        put("ram_mib", self.run.ram_mib.to_string());
        put("host_os", self.host.os.clone());
        put("host_model", self.host.model.clone());
        put("supervisor_pid", self.launch.supervisor_pid.to_string());
        put("worker_pid", self.launch.worker_pid.to_string());
        f
    }

    /// The identity as the supervisor prints it at every spawn: one `limina: identity k=v` line
    /// per field, so `grep '^limina: identity '` lifts it out of a log.
    pub fn log_text(&self) -> String {
        self.fields()
            .iter()
            .map(|(k, v)| format!("limina: identity {k}={}\n", clean(v)))
            .collect()
    }
}

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

fn or_unknown(s: &str) -> String {
    if s.is_empty() { "unknown" } else { s }.to_string()
}

/// A value must stay on its line: a newline in it would end the field and could forge the
/// terminator.
fn clean(v: &str) -> String {
    v.replace(['\n', '\r'], " ")
}

/// Frame `fields` as an answer: `format=` first, then the rest, then [`END`].
fn frame(fields: &[(String, String)]) -> String {
    let mut out = String::new();
    for (k, v) in fields {
        out.push_str(&format!("{k}={}\n", clean(v)));
    }
    out.push_str(END);
    out.push('\n');
    out
}

fn error(msg: &str) -> String {
    frame(&[
        ("format".into(), FORMAT.to_string()),
        ("error".into(), msg.into()),
    ])
}

/// The answer to every request while the `debug-port` lever is off. It says how to turn it on and
/// nothing else: no build, host or launch fact.
pub fn disabled() -> String {
    frame(&[
        ("format".into(), FORMAT.to_string()),
        ("error".into(), "disabled".into()),
        (
            "enable".into(),
            crate::debug_ctl::how_to_enable(&crate::debug_ctl::DEBUG_PORT),
        ),
    ])
}

/// What the responder writes for one unit of input; `enabled` is the lever, read per request.
fn reply(input: Input, identity: &Identity, enabled: bool) -> Option<String> {
    match input {
        Input::Line(line) if line.trim().is_empty() => None,
        _ if !enabled => Some(disabled()),
        Input::Line(line) => answer(&line, identity),
        Input::TooLong => Some(error("request too long")),
    }
}

/// The answer to one request line, or `None` for a blank line (a stray newline is not a
/// request, and answering it would leave an extra answer for the next reader).
pub fn answer(request: &str, identity: &Identity) -> Option<String> {
    let request = request.trim();
    match request {
        "" => None,
        "identity" => Some(frame(&identity.fields())),
        "help" => Some(frame(&[
            ("format".into(), FORMAT.to_string()),
            ("requests".into(), REQUESTS.into()),
        ])),
        other => Some(error(&format!(
            "unknown request {other:?}; requests: {REQUESTS}"
        ))),
    }
}

/// Splits the guest's byte stream into request lines, refusing to buffer an unbounded one.
#[derive(Default)]
struct Lines {
    buf: Vec<u8>,
    /// Discarding the rest of an over-long line until its newline.
    skipping: bool,
}

/// One unit of input: a complete line, or a line that was too long to be a request.
#[derive(Debug, PartialEq)]
enum Input {
    Line(String),
    TooLong,
}

impl Lines {
    fn push(&mut self, bytes: &[u8]) -> Vec<Input> {
        let mut out = Vec::new();
        for &b in bytes {
            if b == b'\n' {
                if !self.skipping {
                    out.push(Input::Line(String::from_utf8_lossy(&self.buf).into_owned()));
                }
                self.buf.clear();
                self.skipping = false;
            } else if !self.skipping {
                self.buf.push(b);
                if self.buf.len() > MAX_REQUEST {
                    self.buf.clear();
                    self.skipping = true;
                    out.push(Input::TooLong);
                }
            }
        }
        out
    }
}

/// Print this launch's identity and answer the guest on `host_fd` until the worker exits (with
/// [`disabled`] while the `debug-port` lever is off). The printed copy goes to the worker log on
/// the host whatever the lever says.
///
/// Printed rather than logged: the worker log runs at `warn` by default, and a build stamp
/// that only shows up when someone remembered to raise the filter is the provenance gap this
/// exists to close (the same reason the SSH forward line is printed).
pub fn serve(host_fd: OwnedFd, identity: Identity) {
    print!("{}", identity.log_text());
    let _ = std::io::stdout().flush();
    let spawned = std::thread::Builder::new()
        .name("limina-debug-port".into())
        .spawn(move || {
            if let Err(e) = respond(UnixStream::from(host_fd), &identity) {
                log::debug!("debug port: {e}");
            }
        });
    if let Err(e) = spawned {
        log::warn!("debug port: no responder for this launch: {e}");
    }
}

fn respond(mut stream: UnixStream, identity: &Identity) -> std::io::Result<()> {
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let mut lines = Lines::default();
    let mut buf = [0u8; 512];
    loop {
        let n = match stream.read(&mut buf) {
            // The worker exited: its end of the socketpair is gone.
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        for input in lines.push(&buf[..n]) {
            if let Some(reply) = reply(input, identity, crate::debug_ctl::DEBUG_PORT.on())
                && let Err(e) = stream.write_all(reply.as_bytes())
            {
                // A guest that asked and stopped reading. Drop the answer, keep serving: the
                // next reader skips the partial one by its framing.
                log::debug!("debug port: answer dropped: {e}");
            }
        }
    }
}

fn sysctl_string(name: &std::ffi::CStr) -> Option<String> {
    let mut len: libc::size_t = 0;
    // SAFETY: a size query (null buffer) on a NUL-terminated name.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len];
    // SAFETY: `buf` holds `len` bytes, the size the kernel just reported.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    buf.truncate(len);
    let text = String::from_utf8_lossy(&buf);
    let text = text.trim_end_matches('\0').trim();
    (!text.is_empty()).then(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::about::{BuildInfo, Dep};

    fn identity() -> Identity {
        Identity {
            build: BuildInfo {
                version: "0.1.0",
                git_rev: "0123456789ab",
                built: "2026-10-10",
                deps: vec![
                    Dep {
                        name: "libkrun".into(),
                        rev: "aaaa".into(),
                    },
                    Dep {
                        name: "virglrs".into(),
                        rev: "bbbb".into(),
                    },
                ],
            },
            host: Host {
                os: "macOS 26.6.2".into(),
                model: "Mac14,6".into(),
            },
            run: RunFacts {
                vm_kind: "flat",
                vm: "my disk.raw".into(),
                boot: "efi",
                gpu: "coexist",
                display: "window",
                display_pool: 4,
                cpus: 4,
                ram_mib: 4096,
            },
            launch: Launch {
                launch_id: "11111111-2222-4333-8444-555555555555".into(),
                resumed: false,
                supervisor_pid: 100,
                worker_pid: 200,
            },
        }
    }

    /// Parse an answer the way the documented bash reader does: skip to `format=`, stop at the
    /// terminator.
    fn read_answer(stream: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut on = false;
        for line in stream.lines() {
            if line.starts_with("format=") {
                on = true;
            }
            if !on {
                continue;
            }
            if line == END {
                return out;
            }
            let (k, v) = line
                .split_once('=')
                .expect("every answer line is key=value");
            out.push((k.to_string(), v.to_string()));
        }
        panic!("no terminator in {stream:?}");
    }

    #[test]
    fn an_identity_answer_starts_with_the_format_and_ends_with_the_terminator() {
        let text = answer("identity\n", &identity()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.first(), Some(&"format=1"));
        assert_eq!(lines.last(), Some(&END));
        assert!(
            lines[..lines.len() - 1].iter().all(|l| l.contains('=')),
            "only the terminator may lack an '=': {text}"
        );
    }

    #[test]
    fn the_identity_carries_the_build_the_deps_and_the_launch() {
        let fields = read_answer(&answer("identity", &identity()).unwrap());
        let get = |k: &str| {
            fields
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("limina_git_rev"), Some("0123456789ab"));
        assert_eq!(get("limina_version"), Some("0.1.0"));
        assert_eq!(get("limina_build_date"), Some("2026-10-10"));
        assert_eq!(get("dep.libkrun"), Some("aaaa"));
        assert_eq!(get("dep.virglrs"), Some("bbbb"));
        assert_eq!(
            get("launch_id"),
            Some("11111111-2222-4333-8444-555555555555")
        );
        assert_eq!(get("vm"), Some("my disk.raw"), "a value keeps its spaces");
        assert_eq!(get("host_os"), Some("macOS 26.6.2"));
        assert_eq!(get("display_pool"), Some("4"));
        assert_eq!(get("resumed"), Some("no"));
        assert_eq!(get("worker_pid"), Some("200"));
        assert_eq!(
            get("dirty"),
            None,
            "no dirty flag: build.rs explains why it would lie"
        );
    }

    #[test]
    fn a_newline_in_a_value_cannot_forge_a_field_or_the_terminator() {
        let mut id = identity();
        id.run.vm = "evil\n.\nlimina_git_rev=forged".into();
        let fields = read_answer(&answer("identity", &id).unwrap());
        let revs: Vec<_> = fields
            .iter()
            .filter(|(k, _)| k == "limina_git_rev")
            .collect();
        assert_eq!(revs.len(), 1);
        assert_eq!(revs[0].1, "0123456789ab");
        assert!(
            fields.iter().any(|(k, _)| k == "worker_pid"),
            "answer cut short"
        );
    }

    #[test]
    fn every_request_gets_an_answer_but_a_blank_line_gets_none() {
        let id = identity();
        assert_eq!(answer("", &id), None);
        assert_eq!(answer("  \r", &id), None);
        // CRLF from a guest-side tool is still the request.
        assert!(
            answer("identity\r", &id)
                .unwrap()
                .contains("limina_git_rev=")
        );
        let help = read_answer(&answer("help", &id).unwrap());
        assert!(help.contains(&("requests".into(), REQUESTS.into())));
        let err = read_answer(&answer("bogus", &id).unwrap());
        assert_eq!(err[0], ("format".into(), "1".into()));
        assert!(err[1].0 == "error" && err[1].1.contains("bogus"));
    }

    #[test]
    fn a_reader_skips_the_tail_of_an_abandoned_answer() {
        let id = identity();
        let full = answer("identity", &id).unwrap();
        // The previous reader left after the first three lines.
        let tail: String = full.lines().skip(3).map(|l| format!("{l}\n")).collect();
        let stream = format!("{tail}{full}");
        assert_eq!(read_answer(&stream), id.fields());
    }

    #[test]
    fn requests_split_on_newlines_across_reads_and_long_lines_are_refused() {
        let mut lines = Lines::default();
        assert_eq!(lines.push(b"iden"), vec![]);
        assert_eq!(
            lines.push(b"tity\nhelp\n"),
            vec![Input::Line("identity".into()), Input::Line("help".into())]
        );
        let long = vec![b'x'; MAX_REQUEST * 3];
        assert_eq!(lines.push(&long), vec![Input::TooLong]);
        // The rest of the over-long line is dropped, and the stream recovers at its newline.
        assert_eq!(
            lines.push(b"xxx\nidentity\n"),
            vec![Input::Line("identity".into())]
        );
    }

    #[test]
    fn the_log_text_is_one_greppable_line_per_field() {
        let id = identity();
        let text = id.log_text();
        assert_eq!(text.lines().count(), id.fields().len());
        assert!(text.lines().all(|l| l.starts_with("limina: identity ")));
        assert!(text.contains("limina: identity limina_git_rev=0123456789ab\n"));
    }

    #[test]
    fn the_host_answers_with_real_values() {
        let host = Host::current();
        assert!(host.os.starts_with("macOS "), "{host:?}");
        assert_ne!(host.model, "unknown");
    }

    #[test]
    fn disabled_every_request_gets_the_same_answer_and_it_carries_no_fact() {
        let id = identity();
        let off = read_answer(&disabled());
        assert_eq!(off[0], ("format".into(), "1".into()));
        assert_eq!(off[1], ("error".into(), "disabled".into()));
        assert_eq!(off.len(), 3, "format, error, enable: {off:?}");
        let (k, how) = &off[2];
        assert_eq!(k, "enable");
        assert!(how.contains("LIMINA_DEBUG_PORT=1"), "{how}");
        assert!(how.contains("Debug menu"), "{how}");
        assert!(
            how.contains("limina debug <vm> lever debug-port on"),
            "{how}"
        );
        for input in [
            Input::Line("identity".into()),
            Input::Line("help".into()),
            Input::Line("bogus".into()),
            Input::TooLong,
        ] {
            let text = reply(input, &id, false).unwrap();
            assert_eq!(text, disabled());
            // Nothing the identity knows leaks into it, value or key.
            for (key, value) in id.fields().iter().skip(1) {
                assert!(!text.contains(&format!("{key}=")), "{key} in {text}");
                let fact = key.starts_with("limina_")
                    || key.starts_with("dep.")
                    || key.starts_with("host_")
                    || key.ends_with("_pid")
                    || key == "launch_id";
                assert!(!fact || !text.contains(value.as_str()), "{value} in {text}");
            }
        }
        // A blank line is still not a request, on or off.
        assert_eq!(reply(Input::Line(" \r".into()), &id, false), None);
        // On, the same inputs get their real answers.
        assert!(
            reply(Input::Line("identity".into()), &id, true)
                .unwrap()
                .contains("limina_git_rev=0123456789ab")
        );
        assert!(
            reply(Input::TooLong, &id, true)
                .unwrap()
                .contains("error=request too long")
        );
    }

    #[test]
    fn the_responder_reads_the_lever_per_request() {
        let _serial = crate::debug_ctl::ACCESS_LEVER_TESTS
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let lever = &crate::debug_ctl::DEBUG_PORT;
        assert!(!lever.on(), "off by default");
        let (host, mut guest) = UnixStream::pair().unwrap();
        let t = std::thread::spawn(move || respond(host, &identity()));
        let mut got = String::new();
        let mut buf = [0u8; 4096];
        let mut ask = |guest: &mut UnixStream, req: &[u8]| {
            guest.write_all(req).unwrap();
            let start = got.len();
            while !got[start..].ends_with("\n.\n") {
                let n = guest.read(&mut buf).unwrap();
                assert!(n > 0, "responder closed early: {got:?}");
                got.push_str(std::str::from_utf8(&buf[..n]).unwrap());
            }
            got[start..].to_string()
        };
        assert_eq!(ask(&mut guest, b"identity\n"), disabled());
        lever.set(true);
        let on = ask(&mut guest, b"identity\n");
        lever.set(false);
        assert!(on.contains("limina_git_rev=0123456789ab"), "{on}");
        assert_eq!(ask(&mut guest, b"help\n"), disabled());
        drop(guest);
        t.join().unwrap().unwrap();
    }

    #[test]
    fn the_responder_answers_over_a_socket_and_stops_when_the_worker_end_closes() {
        let _serial = crate::debug_ctl::ACCESS_LEVER_TESTS
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        crate::debug_ctl::DEBUG_PORT.set(true);
        let (host, guest) = UnixStream::pair().unwrap();
        let t = std::thread::spawn(move || respond(host, &identity()));
        let mut guest = guest;
        guest.write_all(b"bogus\nidentity\n").unwrap();
        let mut got = String::new();
        let mut buf = [0u8; 4096];
        while got.matches("\n.\n").count() < 2 {
            let n = guest.read(&mut buf).unwrap();
            assert!(n > 0, "responder closed early: {got:?}");
            got.push_str(std::str::from_utf8(&buf[..n]).unwrap());
        }
        assert!(got.starts_with("format=1\nerror="));
        assert!(got.contains("limina_git_rev=0123456789ab"));
        crate::debug_ctl::DEBUG_PORT.set(false);
        drop(guest);
        t.join().unwrap().unwrap();
    }
}
