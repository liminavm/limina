// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Driving a flat run's lifecycle from the command line, unattended: `limina --detach`, and
//! `limina stop` for a run that has no bundle to find it by.
//!
//! **Detach.** A run started in the foreground keeps whatever it was started with: its stdout and
//! stderr, and its session. Every process of the run inherits those descriptors — the
//! supervisor, the gvproxy gateway and its reaper, and the worker, whose stdio launchd is handed
//! by `supervisor::launch_worker` — so an `ssh host 'limina --disk …'` keeps the ssh session open
//! for the VM's whole life: sshd ends a session only once every holder of its output pipes has
//! closed them. `--detach` re-executes the same command line minus the flag as a child that
//! leads its own session ([`launch`]): no controlling terminal, nothing but the log file on
//! stdio, and every other inherited descriptor closed at exec. The launcher waits until the
//! child's runtime socket answers, so `limina ssh-port <pid>` works the moment it returns,
//! prints one line for a script to parse and exits:
//!
//! ```text
//! limina: detached pid=<supervisor pid> log=<path>
//! ```
//!
//! **Stop.** Stopping a VM is a SIGTERM to its supervisor, which any process of the same user can
//! already send; `limina stop` for a flat run is a front-end to that and nothing more (one signal
//! is the orderly ladder, a repeated one is the kill, `vmlib::runtime::signal_stop`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// How long the launcher waits for the detached supervisor's runtime socket. The supervisor binds
/// it before anything slow (the gateway, the worker's spawn), so this is margin for a loaded host.
const SOCKET_WAIT: Duration = Duration::from_secs(30);

/// Where a detached run logs when `--log` is not given: beside its boot disk, the way its suspend
/// snapshot (`<disk>.limina-suspend.bin`) already is. The disk is a flat run's identity — `limina
/// stop`, `suspend`, `debug` and `ssh-port` all find the run by it — so this is the one place a
/// harness can find the log again without having kept the launcher's output.
pub fn default_log(disk: Option<&Path>) -> Result<PathBuf> {
    let disk = disk
        .context("--detach needs --log <file> when the run has no --disk to keep its log beside")?;
    Ok(PathBuf::from(format!("{}.limina.log", disk.display())))
}

/// The detached child's arguments: the launcher's own, without `--detach` and `--log` (the child
/// is already writing to the log on its stdio, and `--log` is only valid with `--detach`).
pub fn child_args(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut out = Vec::new();
    let mut args = args.into_iter();
    while let Some(a) = args.next() {
        match a.to_str() {
            Some("--detach") => {}
            Some("--log") => {
                args.next();
            }
            Some(s) if s.starts_with("--log=") => {}
            _ => out.push(a),
        }
    }
    out
}

/// Open the run's log for a fresh run, keeping the previous runs' (`vmlib::logrot`). Append mode
/// is what lets the supervisor keep it bounded while it runs (`logrot::bound_stderr_for_this_run`).
fn open_log(path: &Path) -> Result<std::fs::File> {
    crate::vmlib::logrot::rotate(path, crate::vmlib::logrot::GENERATIONS);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening the log {}", path.display()))?;
    file.set_len(0)
        .with_context(|| format!("emptying {}", path.display()))?;
    Ok(file)
}

/// `limina --detach …`: start the run as a session leader writing to `log`, wait for it to
/// answer, print where it is and return. Never returns `Ok` without having printed the line.
pub fn launch(log: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let log = std::path::absolute(log).with_context(|| format!("resolving {}", log.display()))?;
    let file = open_log(&log)?;
    let exe = std::env::current_exe().context("locating the limina binary")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(child_args(std::env::args_os().skip(1)))
        .stdin(std::process::Stdio::null())
        .stdout(file.try_clone().context("duplicating the log descriptor")?)
        .stderr(file);
    // SAFETY: only async-signal-safe calls between fork and exec. setsid makes the child a session
    // leader with no controlling terminal, so a hangup of the launching terminal or ssh session
    // never reaches it. Every descriptor above stdio is marked close-on-exec rather than closed:
    // std's own exec-error pipe is among them and must stay open until the exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            for fd in 3..libc::getdtablesize() {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags >= 0 {
                    libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                }
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("starting the detached supervisor")?;
    let pid = child.id();
    let socket = crate::runtime_ctl::socket_path(pid);
    let deadline = Instant::now() + SOCKET_WAIT;
    loop {
        if let Some(status) = child
            .try_wait()
            .context("polling the detached supervisor")?
        {
            anyhow::bail!(
                "the VM exited while starting ({status}); its log {} says why:\n{}",
                log.display(),
                tail(&log, 12)
            );
        }
        if crate::runtime_ctl::request(pid, crate::runtime_ctl::Request::Info).is_ok() {
            break;
        }
        if Instant::now() >= deadline {
            eprintln!(
                "limina: supervisor {pid} has not bound {} after {SOCKET_WAIT:?}; it is still \
                 starting, see {}",
                socket.display(),
                log.display()
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("limina: detached pid={pid} log={}", log.display());
    // The child is not ours to wait for: once we exit, launchd adopts and reaps it.
    Ok(())
}

/// The last `n` non-empty lines of `path`.
fn tail(path: &Path, n: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// The name of `pid`'s executable, or `None` when it is gone.
fn command_name(pid: i32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let comm = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if comm.is_empty() {
        return None;
    }
    Some(
        Path::new(&comm)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or(comm),
    )
}

/// Is `pid` a VM's supervisor? A bare pid on the command line is only ever signalled if it is, so
/// a typo cannot SIGTERM something else of the user's — another `limina` process included: the
/// control center and a gateway's reaper are `limina` too, but only a supervisor answers on a
/// runtime socket.
pub fn is_supervisor(pid: i32) -> bool {
    pid > 0
        && crate::runtime_ctl::socket_path(pid as u32).exists()
        && command_name(pid).as_deref() == Some("limina")
}

/// Has `pid` exited? A zombie counts: its parent may not have reaped it yet (a detached run's
/// parent is launchd, which does at once; a harness that spawned the run may not), and
/// `kill(pid, 0)` still succeeds on one.
pub fn exited(pid: i32) -> bool {
    if unsafe { libc::kill(pid, 0) } != 0 {
        return true;
    }
    let state = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    state_exited(&state)
}

/// [`exited`]'s reading of `ps -o stat=`: no line (gone), or a zombie.
fn state_exited(stat: &str) -> bool {
    stat.is_empty() || stat.starts_with('Z')
}

/// `limina stop` for the supervisor `pid` of a run without a bundle: signal it, wait up to
/// `timeout` for it to exit, and say how it went. `what` is how the user named the run.
pub fn stop_pid(pid: i32, what: &str, force: bool, timeout: Duration) -> Result<()> {
    crate::vmlib::runtime::signal_stop(pid, force)?;
    let deadline = Instant::now() + timeout;
    while !exited(pid) {
        if Instant::now() >= deadline {
            if force {
                anyhow::bail!("{what} did not stop within {timeout:?} (supervisor pid {pid})");
            }
            anyhow::bail!(
                "{what} is still running after {timeout:?}: the guest has not powered off \
                 (supervisor pid {pid}). A stop never kills a VM on its own — `limina stop \
                 --force {what}` ends it."
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    println!("{what} stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_child_runs_the_same_command_line_without_the_detach_flags() {
        let got = child_args(os(&[
            "--disk",
            "/vm/a.raw",
            "--detach",
            "--net",
            "--log",
            "/vm/a.log",
            "--cpus",
            "4",
        ]));
        assert_eq!(got, os(&["--disk", "/vm/a.raw", "--net", "--cpus", "4"]));
        let got = child_args(os(&["--log=/x.log", "--detach", "--disk", "d"]));
        assert_eq!(got, os(&["--disk", "d"]));
        // A value that merely looks like the flag is someone else's argument only after its
        // own flag, which the parse already consumed: the flags are dropped wherever they are.
        assert_eq!(child_args(os(&["--net"])), os(&["--net"]));
    }

    #[test]
    fn the_default_log_lives_beside_the_boot_disk() {
        assert_eq!(
            default_log(Some(Path::new("/vms/f44.raw"))).unwrap(),
            PathBuf::from("/vms/f44.raw.limina.log")
        );
        let err = default_log(None).unwrap_err().to_string();
        assert!(err.contains("--log"), "{err}");
    }

    #[test]
    fn a_zombie_has_exited_and_a_live_process_has_not() {
        assert!(state_exited(""));
        assert!(state_exited("Z"));
        assert!(state_exited("Z+"));
        assert!(!state_exited("S"));
        assert!(!state_exited("Ss"));
        assert!(!exited(std::process::id() as i32));
        // An exited, unreaped child is a zombie; `kill(pid, 0)` alone would call it running.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !exited(pid) {
            assert!(Instant::now() < deadline, "true never exited");
            std::thread::sleep(Duration::from_millis(20));
        }
        child.wait().unwrap();
    }

    #[test]
    fn only_a_supervisor_counts_as_one() {
        assert!(!is_supervisor(0));
        assert!(!is_supervisor(-5));
        assert!(!is_supervisor(1), "launchd is not a limina supervisor");
        assert!(
            !is_supervisor(std::process::id() as i32),
            "a process with no runtime socket is not a supervisor"
        );
    }

    #[test]
    fn the_detach_log_is_rotated_and_opened_for_append() {
        let dir = std::env::temp_dir().join(format!("limina-detach-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("a.raw.limina.log");
        std::fs::write(&log, "the previous run\n").unwrap();
        let f = open_log(&log).unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "");
        assert_eq!(
            std::fs::read_to_string(dir.join("a.raw.limina.1.log")).unwrap(),
            "the previous run\n"
        );
        use std::os::fd::AsRawFd;
        let flags = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags & libc::O_APPEND, 0, "the log must be append-mode");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
