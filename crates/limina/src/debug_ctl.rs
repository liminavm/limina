// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Changing a running VM's log filters and diagnostic levers: the supervisor's half.
//!
//! The supervisor binds `$TMPDIR/limina-debug-<pid>.sock` and answers the `limina_debug::wire`
//! protocol there. `limina debug <vm> …` is the client; the Debug menu calls [`handle`] directly.
//! A `log` request for the worker is forwarded over the worker's `--debug-control-fd` link, and
//! the filter is remembered so the next worker (a guest reboot, a resume) starts from it.
//!
//! The socket is reachable by any process of the same user, like the control plane's. Most of
//! what it does changes nothing in the guest: it decides what gets logged, and where the frame
//! capture (`capture start <dir>`) writes what the windows showed. The two access levers are the
//! exception: `input-inject`, `debug-port` and `lifecycle-control` open harness features that are
//! off by default. They are a default-off posture, not a boundary against same-user code, which
//! can flip them here.
//!
//! All of it lasts for this process only. See `limina_debug` for why.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use limina_debug::lever::{self, Lever};
use limina_debug::wire::{self, Request, Scope};
use limina_launch::connect::Endpoint;

pub static EDGE_TRACE: Lever = Lever::new(
    "edge-trace",
    "LIMINA_EDGE_TRACE",
    "fullscreen grab: every free/captured pointer event, click, hit test, edge press and release",
);
pub static INPUT_TRACE: Lever = Lever::new(
    "input-trace",
    "LIMINA_INPUT_TRACE",
    "keyboard: every key and modifier decision, host bitmask against our pressed set",
);
pub static POINTER_WIRE_TRACE: Lever = Lever::new(
    "pointer-wire-trace",
    "LIMINA_POINTER_WIRE_TRACE",
    "every pointer event written to the guest's devices, wallclock-stamped",
);
pub static DISPLAY_TRACE: Lever = Lever::new(
    "display-trace",
    "LIMINA_DISPLAY_TRACE",
    "display sizing: fullscreen insets, host-mode pushes, window reshapes",
);
pub static OVERLAY_TRACE: Lever = Lever::new(
    "overlay-trace",
    "LIMINA_OVERLAY_TRACE",
    "notch overlay: window level and Space transitions",
);
pub static QGA_TRACE: Lever = Lever::new(
    "qga-trace",
    "LIMINA_QGA_TRACE",
    "qemu-guest-agent: every request and reply",
);
pub static PRESENT_COPY_TRACE: Lever = Lever::new(
    "present-copy-trace",
    "LIMINA_PRESENT_COPY_TRACE",
    "frame copies: first pixel as a frame arrives and as its copy goes up (logged at info)",
);
/// Harness access: `input …` requests on the runtime socket (`limina input`). Off, every one is
/// refused, and turning it off releases what injection holds pressed (`inject::lever_off`).
pub static INPUT_INJECT: Lever = Lever::new(
    "input-inject",
    "LIMINA_INPUT_INJECT",
    "harness: let `limina input` type and point into the guest (no Accessibility grant needed)",
);
/// Harness access: the debug port's answers (`debug_port`). Off, the port stays on the bus and
/// answers every request with `error=disabled`.
pub static DEBUG_PORT: Lever = Lever::new(
    "debug-port",
    "LIMINA_DEBUG_PORT",
    "harness: let the guest read the host build, host OS/model and pids from its debug port",
);
/// Harness access: `reset` requests on the runtime socket (`limina reset`), which kill the VM's
/// worker and cold-boot a fresh one. Off, every one is refused.
pub static LIFECYCLE_CONTROL: Lever = Lever::new(
    "lifecycle-control",
    "LIMINA_LIFECYCLE_CONTROL",
    "harness: let `limina reset` power-cycle the VM (kill the worker, cold-boot a fresh one)",
);

/// Every lever this process has, in the order the menu and `status` list them.
pub static LEVERS: &[&Lever] = &[
    &EDGE_TRACE,
    &INPUT_TRACE,
    &POINTER_WIRE_TRACE,
    &DISPLAY_TRACE,
    &OVERLAY_TRACE,
    &QGA_TRACE,
    &PRESENT_COPY_TRACE,
    &INPUT_INJECT,
    &DEBUG_PORT,
    &LIFECYCLE_CONTROL,
];

/// The levers that grant a harness access rather than print a trace. The menu lists them under
/// their own header.
pub fn is_access(l: &Lever) -> bool {
    std::ptr::eq(l, &INPUT_INJECT)
        || std::ptr::eq(l, &DEBUG_PORT)
        || std::ptr::eq(l, &LIFECYCLE_CONTROL)
}

/// How to turn a lever on, for a refusal to quote: its variable, the Debug menu, the CLI.
pub fn how_to_enable(l: &Lever) -> String {
    format!(
        "start the VM with {}=1, tick {} in the window's Debug menu, or run \
         `limina debug <vm> lever {} on`",
        l.env(),
        l.name(),
        l.name()
    )
}

/// The tests, in any module, that read or flip `INPUT_INJECT`, `DEBUG_PORT` or
/// `LIFECYCLE_CONTROL` (process-global). Each leaves them off.
#[cfg(test)]
pub(crate) static ACCESS_LEVER_TESTS: Mutex<()> = Mutex::new(());

/// How long a forwarded request waits for the worker's answer.
const WORKER_TIMEOUT: Duration = Duration::from_secs(2);

/// The worker's debug link, once `run_vm` has made one.
static WORKER: Mutex<Option<Endpoint>> = Mutex::new(None);

/// The worker filter set at runtime, which every later spawn starts from. `None` until one is
/// set, or after `default`: the worker then starts from the supervisor's own `RUST_LOG`.
static WORKER_FILTER: Mutex<Option<String>> = Mutex::new(None);

/// Socket path to remove on exit (see `control::cleanup` for why this is a static).
static CLEANUP_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Where the supervisor with this pid answers.
pub fn socket_path(pid: u32) -> PathBuf {
    std::env::temp_dir().join(format!("limina-debug-{pid}.sock"))
}

/// Remember how to reach the worker's debug link.
pub fn set_worker_link(endpoint: Endpoint) {
    *lock(&WORKER) = Some(endpoint);
}

/// The `RUST_LOG` a newly spawned worker gets, when the filter was changed at runtime.
pub fn worker_rust_log() -> Option<String> {
    lock(&WORKER_FILTER).clone()
}

/// Answer one request: the report lines, or why it was refused.
pub fn handle(req: &Request) -> Result<Vec<String>, String> {
    match req {
        Request::Status => Ok(status()),
        Request::Log { scope, spec } => set_log(*scope, spec),
        Request::Lever { name, on } => {
            let l = lever::find(LEVERS, name).ok_or_else(|| {
                format!(
                    "no lever named {name} (have: {})",
                    LEVERS
                        .iter()
                        .map(|l| l.name())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
            l.set(*on);
            if !*on && std::ptr::eq(l, &INPUT_INJECT) {
                crate::inject::lever_off();
            }
            log::warn!(
                "debug: lever {} is now {}",
                l.name(),
                if *on { "on" } else { "off" }
            );
            Ok(Vec::new())
        }
        Request::Capture(wire::Capture::Start { dir }) => {
            crate::window::frame_capture::start(Path::new(dir)).map(|msg| vec![msg])
        }
        Request::Capture(wire::Capture::Stop) => {
            crate::window::frame_capture::stop().map(|s| vec![s.describe()])
        }
        Request::Capture(wire::Capture::Still { slot, path }) => {
            crate::window::still::take(Path::new(path), *slot).map(|line| vec![line])
        }
    }
}

fn status() -> Vec<String> {
    let mut lines = vec![format!(
        "log supervisor {}",
        limina_debug::logger::filter_spec()
    )];
    lines.push(match ask_worker(&Request::Status) {
        Ok(reply) => reply
            .into_iter()
            .next()
            .unwrap_or_else(|| "log worker ?".into()),
        Err(why) => format!("log worker ? ({why})"),
    });
    for l in LEVERS {
        lines.push(format!(
            "lever {} {} {} {}",
            l.name(),
            if l.on() { "on" } else { "off" },
            l.env(),
            l.about()
        ));
    }
    lines.push(match crate::window::frame_capture::status() {
        Some(dir) => format!("capture on {}", dir.display()),
        None => "capture off".into(),
    });
    lines
}

fn set_log(scope: Scope, spec: &str) -> Result<Vec<String>, String> {
    // Validate once, before touching either process, so a typo cannot leave the two halves
    // disagreeing.
    if spec != "default" {
        limina_debug::logger::parse(spec)?;
    }
    let mut report = Vec::new();
    if scope.includes_supervisor() {
        limina_debug::logger::set_filter(spec)?;
        log::warn!(
            "debug: supervisor log filter is now {}",
            limina_debug::logger::filter_spec()
        );
    }
    if scope.includes_worker() {
        *lock(&WORKER_FILTER) = (spec != "default").then(|| spec.to_string());
        // `default` is spelled out for the worker. A worker spawned after a runtime change
        // started from that change, so its own idea of `default` is the change, not the filter
        // the VM was started with; both processes started from this one.
        let spec = if spec == "default" {
            limina_debug::logger::startup_spec()
        } else {
            spec.to_string()
        };
        let req = Request::Log {
            scope: Scope::Worker,
            spec,
        };
        if let Err(why) = ask_worker(&req) {
            // Not a failure of the request: the filter is kept for the next worker.
            report.push(format!(
                "worker not reached ({why}); its next start uses this filter"
            ));
        }
    }
    Ok(report)
}

/// Send one request to the worker and read its answer.
fn ask_worker(req: &Request) -> Result<Vec<String>, String> {
    let endpoint = lock(&WORKER)
        .clone()
        .ok_or("this VM's worker has no debug link")?;
    let stream = endpoint.connect().map_err(|e| e.to_string())?;
    let _ = stream.set_read_timeout(Some(WORKER_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WORKER_TIMEOUT));
    (&stream)
        .write_all(format!("{}\n", req.to_line()).as_bytes())
        .map_err(|e| e.to_string())?;
    wire::read_answer(&mut BufReader::new(&stream))
}

/// Bind the debug socket and answer it on a thread for the process's lifetime.
pub fn serve() -> Result<()> {
    let path = socket_path(std::process::id());
    // A SIGKILLed run with a recycled pid leaves its socket behind; bind over it.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("binding the debug socket at {}", path.display()))?;
    restrict_to_owner(&path);
    *lock(&CLEANUP_PATH) = Some(path.clone());
    log::info!("debug: answering at {}", path.display());
    std::thread::Builder::new()
        .name("debug-socket".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    // One thread per client: a request for the worker can wait on it, and that
                    // must not hold up the next client.
                    Ok(s) => {
                        let _ = std::thread::Builder::new()
                            .name("debug-client".into())
                            .spawn(move || serve_client(s));
                    }
                    Err(e) => log::warn!("debug: accept failed: {e}"),
                }
            }
        })
        .context("spawning the debug socket thread")?;
    Ok(())
}

fn restrict_to_owner(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

fn serve_client(stream: UnixStream) {
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        let answer = match Request::parse(&line).and_then(|req| handle(&req)) {
            Ok(lines) => {
                let mut s = String::new();
                for l in lines {
                    s.push_str(&l);
                    s.push('\n');
                }
                s.push_str(wire::OK);
                s.push('\n');
                s
            }
            Err(why) => format!("{}\n", wire::err_line(&why)),
        };
        if out.write_all(answer.as_bytes()).is_err() {
            break;
        }
    }
}

/// Remove the socket (idempotent; safe from any exit path).
pub fn cleanup() {
    if let Some(path) = lock(&CLEANUP_PATH).take() {
        let _ = std::fs::remove_file(path);
    }
}

/// `limina debug`: send `requests` to the supervisor with this pid and print what it says.
pub fn client(pid: u32, requests: &[Request]) -> Result<()> {
    let path = socket_path(pid);
    let stream = UnixStream::connect(&path).with_context(|| {
        format!(
            "connecting to {} (is supervisor {pid} a build with the debug socket?)",
            path.display()
        )
    })?;
    // Status asks the worker too, which can take up to its own timeout; a capture stop waits for
    // the frames already copied to be written.
    let timeout = if requests.iter().any(|r| matches!(r, Request::Capture(_))) {
        Duration::from_secs(60)
    } else {
        WORKER_TIMEOUT * 3
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let mut reader = BufReader::new(&stream);
    for req in requests {
        (&stream)
            .write_all(format!("{}\n", req.to_line()).as_bytes())
            .context("sending the request")?;
        match wire::read_answer(&mut reader) {
            Ok(lines) => {
                for l in lines {
                    println!("{l}");
                }
            }
            Err(why) => anyhow::bail!("{}: {why}", req.to_line()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests that read or write the worker filter, which is process-global.
    static WORKER_FILTER_TESTS: Mutex<()> = Mutex::new(());

    #[test]
    fn every_lever_has_a_distinct_name_and_variable() {
        let mut names: Vec<_> = LEVERS.iter().map(|l| l.name()).collect();
        let mut envs: Vec<_> = LEVERS.iter().map(|l| l.env()).collect();
        names.sort_unstable();
        envs.sort_unstable();
        names.dedup();
        envs.dedup();
        assert_eq!(names.len(), LEVERS.len());
        assert_eq!(envs.len(), LEVERS.len());
        for l in LEVERS {
            assert!(
                !l.name().contains(char::is_whitespace),
                "{} would not survive the wire",
                l.name()
            );
        }
    }

    #[test]
    fn the_access_levers_are_off_by_default_and_say_how_to_turn_them_on() {
        let _serial = lock(&ACCESS_LEVER_TESTS);
        for l in [&INPUT_INJECT, &DEBUG_PORT, &LIFECYCLE_CONTROL] {
            assert!(
                std::env::var_os(l.env()).is_none(),
                "unset {} to run the unit tests",
                l.env()
            );
            assert!(!l.on(), "{} is on with its variable unset", l.name());
            assert!(is_access(l) && LEVERS.iter().any(|x| std::ptr::eq(*x, l)));
            let how = how_to_enable(l);
            assert!(how.contains(&format!("{}=1", l.env())), "{how}");
            assert!(how.contains("Debug menu"), "{how}");
            assert!(
                how.contains(&format!("limina debug <vm> lever {} on", l.name())),
                "{how}"
            );
        }
        assert!(!is_access(&EDGE_TRACE));
        assert_eq!(
            (INPUT_INJECT.name(), INPUT_INJECT.env()),
            ("input-inject", "LIMINA_INPUT_INJECT")
        );
        assert_eq!(
            (DEBUG_PORT.name(), DEBUG_PORT.env()),
            ("debug-port", "LIMINA_DEBUG_PORT")
        );
        assert_eq!(
            (LIFECYCLE_CONTROL.name(), LIFECYCLE_CONTROL.env()),
            ("lifecycle-control", "LIMINA_LIFECYCLE_CONTROL")
        );
        let lines = status();
        for want in [
            "lever input-inject off LIMINA_INPUT_INJECT ",
            "lever debug-port off LIMINA_DEBUG_PORT ",
            "lever lifecycle-control off LIMINA_LIFECYCLE_CONTROL ",
        ] {
            assert!(lines.iter().any(|l| l.starts_with(want)), "{lines:?}");
        }
    }

    #[test]
    fn the_access_levers_follow_the_lever_request() {
        let _serial = lock(&ACCESS_LEVER_TESTS);
        for (l, name) in [
            (&INPUT_INJECT, "input-inject"),
            (&DEBUG_PORT, "debug-port"),
            (&LIFECYCLE_CONTROL, "lifecycle-control"),
        ] {
            for on in [true, false] {
                let req = Request::Lever {
                    name: name.into(),
                    on,
                };
                assert_eq!(handle(&req), Ok(vec![]));
                assert_eq!(l.on(), on, "{name}");
            }
        }
    }

    #[test]
    fn an_unknown_lever_names_the_ones_there_are() {
        let err = handle(&Request::Lever {
            name: "nope".into(),
            on: true,
        })
        .unwrap_err();
        assert!(err.contains("edge-trace"), "{err}");
    }

    #[test]
    fn a_bad_filter_changes_nothing_anywhere() {
        let _serial = lock(&WORKER_FILTER_TESTS);
        let before = worker_rust_log();
        let err = handle(&Request::Log {
            scope: Scope::All,
            spec: "limina=loud".into(),
        });
        assert!(err.is_err());
        assert_eq!(worker_rust_log(), before);
    }

    #[test]
    fn a_worker_filter_without_a_worker_is_kept_for_the_next_one() {
        let _serial = lock(&WORKER_FILTER_TESTS);
        // No link in a unit test: the request is still accepted, reported, and remembered.
        let lines = handle(&Request::Log {
            scope: Scope::Worker,
            spec: "info,krun_devices=debug".into(),
        })
        .unwrap();
        assert!(lines.iter().any(|l| l.contains("next start")), "{lines:?}");
        assert_eq!(
            worker_rust_log().as_deref(),
            Some("info,krun_devices=debug")
        );
        handle(&Request::Log {
            scope: Scope::Worker,
            spec: "default".into(),
        })
        .unwrap();
        assert_eq!(worker_rust_log(), None);
    }

    #[test]
    fn the_socket_answers_the_protocol() {
        let dir = std::env::temp_dir().join(format!("limina-debug-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            if let Ok((s, _)) = listener.accept() {
                serve_client(s);
            }
        });
        let stream = UnixStream::connect(&path).unwrap();
        let mut reader = BufReader::new(&stream);
        (&stream).write_all(b"lever edge-trace on\n").unwrap();
        assert_eq!(wire::read_answer(&mut reader), Ok(vec![]));
        assert!(EDGE_TRACE.on());
        (&stream).write_all(b"lever edge-trace off\n").unwrap();
        assert_eq!(wire::read_answer(&mut reader), Ok(vec![]));
        assert!(!EDGE_TRACE.on());
        (&stream).write_all(b"bogus\n").unwrap();
        assert!(wire::read_answer(&mut reader).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
