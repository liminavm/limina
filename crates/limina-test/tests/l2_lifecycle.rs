// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Unattended lifecycle of a FLAT run, driven only through the command line the way a remote
//! harness drives it: `limina --detach`, `limina reset`, `limina stop [--force]`.
//!
//! Phase 1 boots the stock image headless with `--detach --net` and checks that the launcher
//! hands its stdout and stderr back at once. Those are pipes here, read to EOF, which is exactly
//! what sshd waits for before it ends a session: a VM process that inherited either pipe keeps
//! the EOF from coming. The VM must be up behind it (ssh works, `limina ssh-port` answers). Then
//! `limina reset` is refused while the `lifecycle-control` lever is off, and once it is on it
//! cold-boots the guest (a new `boot_id`) behind the same supervisor and the same SSH port.
//! `limina stop` then powers the guest off in order.
//!
//! Phase 2 boots it again, wedges the guest with a kernel panic, and checks that an ordinary
//! `limina stop` does not kill it and that `limina stop --force` ends every process of the run —
//! supervisor, worker and its launcher, gvproxy and its reaper — without writing a suspend
//! snapshot.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use limina_test::{Boot, GuestConfig};

/// How long the launcher may keep its stdout and stderr open. It waits for the supervisor's
/// runtime socket before it returns, which takes a second or two, and gives up after 30 s
/// (`lifecycle::SOCKET_WAIT`); this bound is past that, so a slow host reads as a launcher that
/// gave up (exit 3), never as a pipe a VM process inherited.
const LAUNCHER_RETURNS_WITHIN: Duration = Duration::from_secs(45);

/// Boot to a usable sshd, generous for a loaded host.
const SSH_UP: Duration = Duration::from_secs(240);

/// A run's scratch directory (its disk clone and logs), removed when dropped — including when a
/// launch fails before there is a run to stop. `LIMINA_TEST_KEEP_SCRATCH=1` keeps it.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::env::var_os("LIMINA_TEST_KEEP_SCRATCH").is_none() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// One detached run of a private clone of the stock image. Its fields drop after its own `Drop`
/// has stopped the VM, so the scratch directory goes last.
struct Run {
    limina: PathBuf,
    vmm: PathBuf,
    scratch: Scratch,
    disk: PathBuf,
    port: u16,
    pid: libc::pid_t,
}

/// Run `limina <args>` to completion, killed at `timeout`.
fn limina(bin: &Path, args: &[&str], timeout: Duration) -> Output {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning limina");
    let (out, err) = drain(&mut child);
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(s) = child.try_wait().expect("polling limina") {
            break s;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let s = child.wait().expect("reaping limina");
            eprintln!("`limina {}` killed after {timeout:?}", args.join(" "));
            break s;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    Output {
        status,
        stdout: out.recv_timeout(Duration::from_secs(5)).unwrap_or_default(),
        stderr: err.recv_timeout(Duration::from_secs(5)).unwrap_or_default(),
    }
}

/// Read a child's stdout and stderr to EOF on threads; each channel yields once its pipe closes.
fn drain(
    child: &mut std::process::Child,
) -> (
    std::sync::mpsc::Receiver<Vec<u8>>,
    std::sync::mpsc::Receiver<Vec<u8>>,
) {
    fn reader(pipe: Option<impl Read + Send + 'static>) -> std::sync::mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            let _ = tx.send(buf);
        });
        rx
    }
    (reader(child.stdout.take()), reader(child.stderr.take()))
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Gone, or a zombie nobody has reaped yet.
fn gone(pid: libc::pid_t) -> bool {
    if unsafe { libc::kill(pid, 0) } != 0 {
        return true;
    }
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map(|o| text(&o.stdout))
        .unwrap_or_default();
    out.trim().is_empty() || out.trim().starts_with('Z')
}

fn wait_gone(pids: &[libc::pid_t], timeout: Duration) -> Vec<libc::pid_t> {
    let deadline = Instant::now() + timeout;
    loop {
        let left: Vec<_> = pids.iter().copied().filter(|&p| !gone(p)).collect();
        if left.is_empty() || Instant::now() >= deadline {
            return left;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

impl Run {
    /// Clone the stock image and start it with `--detach`, asserting the launcher returns.
    fn launch(name: &str) -> Run {
        let cfg = GuestConfig::fedora_from_env().expect("stock config");
        let (firmware, golden) = match &cfg.boot {
            Boot::Firmware { firmware, disk, .. } => (firmware.clone(), disk.clone()),
            _ => unreachable!("fedora_from_env builds a firmware boot"),
        };
        let scratch = Scratch(
            std::env::temp_dir().join(format!("limina-lifecycle-{}-{name}", std::process::id())),
        );
        let _ = std::fs::remove_dir_all(&scratch.0);
        std::fs::create_dir_all(&scratch.0).expect("scratch dir");
        let disk = scratch.0.join("disk.raw");
        limina_test::cow_clone(&golden, &disk).expect("cloning the stock image");
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .and_then(|l| l.local_addr())
            .expect("probing a free port")
            .port();
        let log = scratch.0.join("supervisor.log");

        let started = Instant::now();
        let mut child = Command::new(&cfg.limina_bin)
            .args([
                "--cpus",
                "4",
                "--ram-mib",
                "4096",
                "--shutdown-grace-secs",
                "10",
            ])
            .arg("--vmm-bin")
            .arg(&cfg.vmm_bin)
            .arg("--firmware")
            .arg(&firmware)
            .arg("--disk")
            .arg(&disk)
            .arg("--console")
            .arg(scratch.0.join("console.log"))
            .args(["--net", "--ssh-port", &port.to_string()])
            .arg("--detach")
            .arg("--log")
            .arg(&log)
            .env(
                "RUST_LOG",
                "warn,limina=info,krun::vmm=info,krun_devices=info",
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawning the launcher");
        let (out, err) = drain(&mut child);
        let mut held = |what: &str| -> Vec<u8> {
            let rx = if what == "stdout" { &out } else { &err };
            match rx.recv_timeout(LAUNCHER_RETURNS_WITHIN.saturating_sub(started.elapsed())) {
                Ok(b) => b,
                Err(_) => {
                    // Something of the run still holds the pipe: name it before failing.
                    let _ = child.kill();
                    let holders = Command::new("lsof")
                        .args(["-a", "-d", "0,1,2", "-c", "limina", "-c", "gvproxy"])
                        .output()
                        .map(|o| text(&o.stdout))
                        .unwrap_or_default();
                    panic!(
                        "the launcher's {what} was still open {LAUNCHER_RETURNS_WITHIN:?} after \
                         `limina --detach` started: a VM process inherited it, and an ssh session \
                         would hang the same way.\n{holders}"
                    );
                }
            }
        };
        let stdout = text(&held("stdout"));
        let stderr = text(&held("stderr"));
        let status = child.wait().expect("reaping the launcher");
        eprintln!(
            "launcher returned in {:?} ({status}): {stdout}{stderr}",
            started.elapsed()
        );
        assert!(
            status.success(),
            "`limina --detach` failed ({status}):\nstdout: {stdout}\nstderr: {stderr}"
        );
        let line = stdout
            .lines()
            .find(|l| l.starts_with("limina: detached "))
            .unwrap_or_else(|| panic!("no `limina: detached` line in {stdout:?}"));
        let field = |k: &str| {
            line.split_whitespace()
                .find_map(|w| w.strip_prefix(&format!("{k}=")))
                .unwrap_or_else(|| panic!("no {k}= in {line:?}"))
                .to_string()
        };
        let pid: libc::pid_t = field("pid").parse().expect("a numeric pid");
        assert_eq!(PathBuf::from(field("log")), log, "{line}");

        let run = Run {
            limina: cfg.limina_bin.clone(),
            vmm: cfg.vmm_bin.clone(),
            scratch,
            disk,
            port,
            pid,
        };
        assert!(!gone(pid), "the detached supervisor {pid} is not running");
        // Its own session: no terminal, and no hangup when the launching shell or ssh goes.
        assert_eq!(
            unsafe { libc::getsid(pid) },
            pid,
            "the detached supervisor must lead its own session"
        );
        // The launcher waited for the runtime socket, so the run answers straight away.
        let sp = run.cli(&["ssh-port", &pid.to_string()], Duration::from_secs(10));
        assert!(sp.status.success(), "ssh-port: {}", text(&sp.stderr));
        assert_eq!(text(&sp.stdout).trim(), port.to_string());
        run
    }

    fn cli(&self, args: &[&str], timeout: Duration) -> Output {
        limina(&self.limina, args, timeout)
    }

    fn disk_arg(&self) -> &str {
        self.disk.to_str().expect("utf-8 scratch path")
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.scratch.0.join("supervisor.log")).unwrap_or_default()
    }

    fn wait_ssh(&self) {
        let pid = self.pid;
        if let Err(e) = limina_test::wait_for_ssh_on(self.port, SSH_UP, || !gone(pid)) {
            panic!("guest ssh never came up: {e:#}\n{}", self.log());
        }
    }

    fn ssh(&self, cmd: &str) -> String {
        limina_test::ssh_exec_on(self.port, cmd, Duration::from_secs(60))
            .unwrap_or_else(|e| panic!("{e:#}\n{}", self.log()))
    }

    fn boot_id(&self) -> String {
        self.ssh("cat /proc/sys/kernel/random/boot_id")
            .trim()
            .to_string()
    }

    fn worker(&self) -> libc::pid_t {
        limina_test::worker_pid_of(self.pid, &self.vmm).expect("the run's worker")
    }

    /// Every process of the run: supervisor, worker and its launchd launcher, gvproxy, and the
    /// gvproxy reaper.
    fn processes(&self) -> Vec<(&'static str, libc::pid_t)> {
        let mut all = vec![("supervisor", self.pid)];
        let worker = self.worker();
        all.push(("worker", worker));
        if let Some(launcher) = limina_test::parent_pid(worker).filter(|&p| p != self.pid) {
            all.push(("worker launcher", launcher));
        }
        for child in limina_test::child_pids(self.pid) {
            let argv = limina_test::proc_argv(child).unwrap_or_default();
            if argv.iter().any(|a| a == "__reap-gateway") {
                all.push(("gvproxy reaper", child));
            } else if argv
                .iter()
                .any(|a| a.contains(&format!("limina-gvproxy-{}.sock", self.pid)))
            {
                all.push(("gvproxy", child));
            }
        }
        assert!(
            all.iter().any(|(n, _)| *n == "gvproxy")
                && all.iter().any(|(n, _)| *n == "gvproxy reaper"),
            "a --net run has a gvproxy and its reaper: {all:?}"
        );
        all
    }

    fn runtime_socket(&self) -> PathBuf {
        std::env::temp_dir().join(format!("limina-vm-{}.sock", self.pid))
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        if !gone(self.pid) {
            let _ = self.cli(
                &["stop", "--force", "--timeout", "30", &self.pid.to_string()],
                Duration::from_secs(40),
            );
        }
        if !gone(self.pid) {
            // Only ever this run's own processes: the disk path is under a scratch directory
            // named for this test process's pid and the phase, so no other run on the host —
            // another session's included — has it in its argv, as a whole or as a substring.
            unsafe { libc::kill(self.pid, libc::SIGKILL) };
            let _ = Command::new("pkill")
                .args(["-9", "-f", self.disk_arg()])
                .status();
        }
    }
}

#[test]
fn a_detached_flat_run_resets_and_stops_without_a_terminal() {
    if !limina_test::require_hvf_or_skip("a_detached_flat_run_resets_and_stops_without_a_terminal")
    {
        return;
    }
    if let Err(e) = GuestConfig::fedora_from_env() {
        eprintln!("SKIPPED a_detached_flat_run_resets_and_stops_without_a_terminal: {e}");
        return;
    }

    // ---- Phase 1: detach, reset (gated), orderly stop ----
    let run = Run::launch("orderly");
    run.wait_ssh();
    let first_boot = run.boot_id();
    let first_worker = run.worker();

    // A second detach of the running disk is refused before it touches anything: the live run's
    // log stays where it is, and no new run starts.
    let log = run.scratch.0.join("supervisor.log");
    let again = run.cli(
        &[
            "--disk",
            run.disk_arg(),
            "--detach",
            "--log",
            log.to_str().expect("utf-8 scratch path"),
        ],
        Duration::from_secs(30),
    );
    assert!(
        !again.status.success() && text(&again.stderr).contains("already running"),
        "a second --detach of a running disk must be refused: {}{}",
        text(&again.stdout),
        text(&again.stderr)
    );
    assert!(
        !run.scratch.0.join("supervisor.1.log").exists()
            && run.log().contains("guest SSH forward ready"),
        "the refused detach rotated the live run's log"
    );

    // Reset is a control surface: refused, with the way to turn it on, while the lever is off.
    let refused = run.cli(&["reset", run.disk_arg()], Duration::from_secs(30));
    let why = text(&refused.stderr);
    assert!(
        !refused.status.success(),
        "reset must be refused while lifecycle-control is off: {}",
        text(&refused.stdout)
    );
    assert!(
        why.contains("LIMINA_LIFECYCLE_CONTROL=1")
            && why.contains("limina debug <vm> lever lifecycle-control on"),
        "the refusal must say how to turn the lever on: {why}"
    );
    assert_eq!(
        run.worker(),
        first_worker,
        "a refused reset touched the worker"
    );

    let lever = run.cli(
        &["debug", run.disk_arg(), "lever", "lifecycle-control", "on"],
        Duration::from_secs(30),
    );
    assert!(lever.status.success(), "lever: {}", text(&lever.stderr));

    let reset = run.cli(&["reset", run.disk_arg()], Duration::from_secs(120));
    assert!(
        reset.status.success(),
        "reset failed: {}\n{}",
        text(&reset.stderr),
        run.log()
    );
    assert!(!gone(run.pid), "reset must keep the supervisor");
    assert_ne!(run.worker(), first_worker, "reset must replace the worker");
    run.wait_ssh();
    let second_boot = run.boot_id();
    assert_ne!(
        first_boot, second_boot,
        "reset must cold-boot the guest (same boot_id)"
    );
    let sp = run.cli(&["ssh-port", run.disk_arg()], Duration::from_secs(10));
    assert_eq!(
        text(&sp.stdout).trim(),
        run.port.to_string(),
        "the SSH port moved across the reset"
    );

    let procs = run.processes();
    let stop = run.cli(
        &["stop", "--timeout", "90", run.disk_arg()],
        Duration::from_secs(100),
    );
    assert!(
        stop.status.success(),
        "orderly stop failed: {}\n{}",
        text(&stop.stderr),
        run.log()
    );
    let left = wait_gone(
        &procs.iter().map(|p| p.1).collect::<Vec<_>>(),
        Duration::from_secs(10),
    );
    assert!(
        left.is_empty(),
        "left running after stop: {left:?} of {procs:?}"
    );
    assert!(
        run.log().contains("VM powered off cleanly"),
        "the stop was not an orderly power-off:\n{}",
        run.log()
    );
    drop(run);

    // ---- Phase 2: a wedged guest, an orderly stop that cannot end it, and --force ----
    let run = Run::launch("forced");
    run.wait_ssh();
    let procs = run.processes();
    // A panic must leave the guest wedged, not reboot it: no panic timeout, and no crash kernel
    // loaded for kdump to boot into. The stock image has both that way; pin the first and check
    // the second, so a changed image fails here instead of passing a test of something else.
    run.ssh("sudo sysctl -q -w kernel.panic=0");
    assert_eq!(
        run.ssh("cat /sys/kernel/kexec_crash_loaded").trim(),
        "0",
        "a crash kernel is loaded: the panic below would reboot the guest into kdump"
    );
    // The ssh that panics the kernel never gets an answer; its timeout is the expected outcome.
    let _ = limina_test::ssh_exec_on(
        run.port,
        "echo c | sudo tee /proc/sysrq-trigger",
        Duration::from_secs(15),
    );
    assert!(
        limina_test::ssh_exec_on(run.port, "true", Duration::from_secs(20)).is_err(),
        "the guest still answers ssh after a kernel panic"
    );

    let polite = run.cli(
        &["stop", "--timeout", "10", run.disk_arg()],
        Duration::from_secs(30),
    );
    assert!(
        !polite.status.success(),
        "an ordinary stop reported success against a wedged guest"
    );
    assert!(
        text(&polite.stderr).contains("--force"),
        "the stop must say what ends it: {}",
        text(&polite.stderr)
    );
    assert!(!gone(run.pid), "an ordinary stop must never kill the VM");

    let forced = run.cli(
        &["stop", "--force", "--timeout", "30", run.disk_arg()],
        Duration::from_secs(40),
    );
    assert!(
        forced.status.success(),
        "stop --force failed: {}\n{}",
        text(&forced.stderr),
        run.log()
    );
    let left = wait_gone(
        &procs.iter().map(|p| p.1).collect::<Vec<_>>(),
        Duration::from_secs(10),
    );
    assert!(
        left.is_empty(),
        "left running after stop --force: {left:?} of {procs:?}"
    );
    assert!(
        !run.runtime_socket().exists(),
        "the runtime socket outlived the run"
    );
    let snapshot = PathBuf::from(format!("{}.limina-suspend.bin", run.disk.display()));
    assert!(
        !snapshot.exists(),
        "a forced stop must not write a suspend snapshot"
    );
}
