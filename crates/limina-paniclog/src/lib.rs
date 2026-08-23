// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Where a panic goes when nobody is looking at stderr.
//!
//! Our loud assertions ([`crate::window::warp`] and friends) are deliberate: a warp-class fault
//! crashes with a message naming the target, the slot and the whole display arrangement, because
//! the alternative is a silently wrong pointer. That design assumes somebody *reads* the message
//! — and in the one place it matters most it is thrown away. A Dock-launched `Limina.app` has no
//! stderr: launchd gives it `/dev/null`, the unified log does not capture it, and the `.ips` crash
//! report carries only the backtrace. A dogfood crash on 2026-08-23 (a stale pointer park after an
//! external display was unplugged) therefore cost a disassembly of the shipped binary to recover
//! one `file:line` that the process had already formatted and dropped.
//!
//! So every panic is also appended to a file, before any abort hook takes the process down.
//! Both halves of a running VM write to it — the `limina` supervisor and its entitled
//! `limina-vmm` worker, which inherits the supervisor's stderr and so inherits the same silence.
//! Each record names its own `component` and pid, and `O_APPEND` plus a single `write_all` is
//! what keeps two processes from interleaving mid-record.
//!
//! Install it first in `main`, before anything else takes a hook: `limina`'s own
//! `install_panic_kill_hook` chains on top and takes this sink as the `default` it calls before
//! `abort()`.
//!
//! Nothing here may panic. A hook that panics aborts with no message at all, which is exactly the
//! failure this module exists to end — so every step degrades to `eprintln!` and returns.

use std::backtrace::Backtrace;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Write;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};

/// The log every limina process appends to. `$LIMINA_PANIC_LOG` overrides it (tests, and a
/// dogfood run that wants the record somewhere specific); otherwise the macOS convention,
/// `~/Library/Logs/Limina/panic.log` — the same folder the user already opens in Console.app
/// next to `DiagnosticReports`.
pub fn log_path() -> Option<PathBuf> {
    path_from(
        std::env::var_os("LIMINA_PANIC_LOG"),
        std::env::var_os("HOME"),
    )
}

/// The path decision, pure. An override wins even if `HOME` is set; with neither there is
/// nowhere to write and the sink stays silent rather than inventing a relative path in whatever
/// directory the process happens to be in.
fn path_from(override_var: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    if let Some(p) = override_var {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let home = home.filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join("Library/Logs/Limina/panic.log"))
}

/// One panic as it lands in the file. Several limina processes share it — the control center,
/// one supervisor per running VM, and each supervisor's worker — so every record names the
/// binary it came from as well as its pid and thread; the `====` banner is the record separator
/// a reader greps for.
struct Record<'a> {
    stamp: &'a str,
    /// The binary that panicked — the caller's `CARGO_PKG_NAME`, not this crate's.
    component: &'a str,
    pid: u32,
    version: &'a str,
    thread: &'a str,
    location: Option<&'a str>,
    payload: &'a str,
    backtrace: &'a str,
}

fn format_record(r: &Record<'_>) -> String {
    let Record {
        stamp,
        component,
        pid,
        version,
        thread,
        location,
        payload,
        backtrace,
    } = r;
    let mut s = String::with_capacity(payload.len() + backtrace.len() + 256);
    s.push_str(&format!(
        "==== {component} panic {stamp} pid={pid} v{version} thread={thread} ====\n"
    ));
    s.push_str(&format!(
        "at {}\n",
        location.unwrap_or("<unknown location>")
    ));
    s.push_str(payload);
    if !payload.ends_with('\n') {
        s.push('\n');
    }
    s.push_str("--- backtrace ---\n");
    s.push_str(backtrace);
    if !backtrace.ends_with('\n') {
        s.push('\n');
    }
    s
}

/// Append one finished record. `O_APPEND` on a single `write_all` is what keeps concurrent
/// processes from interleaving mid-record.
fn append_record(path: &Path, record: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(record.as_bytes())
}

/// The wall-clock stamp, local time, via libc — the crate carries no date dependency and a
/// panic record only needs to be correlatable with a `.ips` capture time.
fn now_stamp() -> String {
    // SAFETY: plain libc time formatting into our own buffers.
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return format!("epoch+{t}");
        }
        let mut buf = [0i8; 64];
        let n = libc::strftime(
            buf.as_mut_ptr(),
            buf.len(),
            c"%Y-%m-%d %H:%M:%S %z".as_ptr(),
            &tm,
        );
        if n == 0 {
            return format!("epoch+{t}");
        }
        String::from_utf8_lossy(&buf[..n].iter().map(|&c| c as u8).collect::<Vec<u8>>())
            .into_owned()
    }
}

/// The panic payload as text. `PanicHookInfo::payload` is `&dyn Any`; the two shapes std ever
/// puts there are `&str` (a bare `panic!("literal")`) and `String` (anything formatted, which is
/// every assertion in this codebase).
fn payload_of(info: &PanicHookInfo<'_>) -> String {
    let p = info.payload();
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Install the file sink. Chains the hook already in place (so the default stderr print still
/// happens) and must be called before any hook that aborts, which then wraps this one.
///
/// `component` and `version` are the caller's own `CARGO_PKG_NAME` / `CARGO_PKG_VERSION` — this
/// crate's would name the sink, not the binary that crashed.
pub fn install(component: &'static str, version: &'static str) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        let Some(path) = log_path() else {
            return;
        };
        let thread = std::thread::current();
        let record = format_record(&Record {
            stamp: &now_stamp(),
            component,
            pid: std::process::id(),
            version,
            thread: thread.name().unwrap_or("<unnamed>"),
            location: info.location().map(|l| l.to_string()).as_deref(),
            payload: &payload_of(info),
            backtrace: &Backtrace::force_capture().to_string(),
        });
        if let Err(e) = append_record(&path, &record) {
            // The sink failing must never be the thing that hides the panic.
            eprintln!(
                "limina: could not write the panic log {}: {e}",
                path.display()
            );
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_override_wins_and_an_empty_one_does_not() {
        assert_eq!(
            path_from(Some("/tmp/p.log".into()), Some("/Users/x".into())),
            Some(PathBuf::from("/tmp/p.log")),
        );
        assert_eq!(
            path_from(Some("".into()), Some("/Users/x".into())),
            Some(PathBuf::from("/Users/x/Library/Logs/Limina/panic.log")),
            "an empty override is not a path — fall through to HOME",
        );
    }

    #[test]
    fn with_no_home_and_no_override_there_is_nowhere_to_write() {
        assert_eq!(path_from(None, None), None);
        assert_eq!(path_from(None, Some("".into())), None);
    }

    #[test]
    fn a_record_carries_the_location_and_the_message() {
        let r = format_record(&Record {
            stamp: "2026-08-23 17:03:22 -0300",
            component: "limina",
            pid: 49683,
            version: "0.1.0",
            thread: "main",
            location: Some("crates/limina/src/window/warp.rs:106:5"),
            payload: "pointer capture [repin]: warp target (1746.0,259.0) is on NO display",
            backtrace: "0: limina::window::warp::warp_checked",
        });
        assert!(r.starts_with(
            "==== limina panic 2026-08-23 17:03:22 -0300 pid=49683 v0.1.0 thread=main ====\n"
        ));
        assert!(r.contains("at crates/limina/src/window/warp.rs:106:5\n"));
        assert!(
            r.contains("is on NO display\n"),
            "the message is terminated"
        );
        assert!(r.contains("--- backtrace ---\n0: limina::window::warp::warp_checked\n"));
    }

    #[test]
    fn a_record_with_no_location_still_says_so() {
        let r = format_record(&Record {
            stamp: "t",
            component: "limina-vmm",
            pid: 1,
            version: "0.1.0",
            thread: "worker",
            location: None,
            payload: "boom",
            backtrace: "bt",
        });
        assert!(r.contains("at <unknown location>\n"));
        assert!(
            r.starts_with("==== limina-vmm panic "),
            "a shared log is only readable if each record names its binary; got:\n{r}"
        );
    }

    /// The hook itself, end to end. A panic hook is process-global and this test suite shares
    /// one process, so the only honest way to exercise `install` is a child: the test re-execs
    /// its own binary, and the child installs the sink and panics for real.
    #[test]
    fn the_installed_hook_writes_the_panic_to_the_log() {
        if std::env::var_os("LIMINA_PANIC_LOG_CHILD").is_some() {
            // The child writes to whatever `LIMINA_PANIC_LOG` the parent handed it — it must
            // not re-derive the path, whose name carries the *parent's* pid.
            install("limina-test-child", "0.0.0");
            panic!("a deliberate panic, from {}", "the child");
        }
        let path =
            std::env::temp_dir().join(format!("limina-panic-hook-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let status = std::process::Command::new(std::env::current_exe().expect("test binary path"))
            .args([
                "tests::the_installed_hook_writes_the_panic_to_the_log",
                "--exact",
                "--nocapture",
            ])
            .env("LIMINA_PANIC_LOG_CHILD", "1")
            .env("LIMINA_PANIC_LOG", &path)
            .output()
            .expect("re-exec the test binary");
        let got = std::fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "the child left no panic log at {} ({e}); child stderr:\n{}",
                path.display(),
                String::from_utf8_lossy(&status.stderr),
            )
        });
        assert!(
            got.contains("a deliberate panic, from the child"),
            "the formatted message is what a reader needs; got:\n{got}"
        );
        assert!(
            got.contains("lib.rs:"),
            "the record names the panicking location; got:\n{got}"
        );
        assert!(
            got.contains("==== limina-test-child panic "),
            "the record names the component the caller passed; got:\n{got}"
        );
        assert!(
            got.contains("--- backtrace ---"),
            "the record carries a backtrace; got:\n{got}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn records_append_and_never_truncate() {
        let dir =
            std::env::temp_dir().join(format!("limina-panic-log-test-{}", std::process::id()));
        let path = dir.join("nested/panic.log");
        let _ = std::fs::remove_dir_all(&dir);
        append_record(&path, "first\n").expect("the sink creates its directory");
        append_record(&path, "second\n").expect("a second record appends");
        let got = std::fs::read_to_string(&path).expect("the log is readable");
        assert_eq!(got, "first\nsecond\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
