// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! End-to-end test of the death-pact reaper subcommand (`limina __reap-gateway`).
//!
//! Proves the *shipped binary*, when the death-pact pipe it inherited closes (i.e. its
//! supervisor died by any means), reaps gvproxy — exercising the real re-exec + fd-inheritance
//! wiring that the in-crate unit tests can't reach. gvproxy is launched *orphaned* (reparented
//! to init) so it mimics a real leftover and avoids zombie-of-the-test artifacts.
//!
//! Gated on `LIMINA_HVF_TESTS` (set by `scripts/test-boot.sh`) so a plain sandboxed `cargo test`
//! — which can't spawn gvproxy or write `$TMPDIR` — skips it.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn gated() -> bool {
    std::env::var_os("LIMINA_HVF_TESTS").is_some()
}

fn pid_is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn gvproxy_bin() -> String {
    std::env::var("LIMINA_GVPROXY_BIN").unwrap_or_else(|_| "/opt/homebrew/bin/gvproxy".into())
}

/// Launch gvproxy ORPHANED (reparented to init): a child `sh` backgrounds gvproxy and exits, so
/// gvproxy's parent becomes init. Returns gvproxy's pid (read from its own `-pid-file`).
fn launch_orphan_gvproxy(dir: &Path, socket: &Path) -> i32 {
    let pidfile = dir.join("gv.pid");
    let _ = std::fs::remove_file(&pidfile);
    let mut launcher = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!(
            "{} -listen-vfkit unixgram://{} -ssh-port 2222 -pid-file {} &",
            gvproxy_bin(),
            socket.display(),
            pidfile.display()
        ))
        .spawn()
        .expect("launch orphan gvproxy");
    let _ = launcher.wait();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(s) = std::fs::read_to_string(&pidfile) {
            if let Ok(pid) = s.trim().parse::<i32>() {
                if pid > 0 && pid_is_alive(pid) {
                    return pid;
                }
            }
        }
        assert!(Instant::now() < deadline, "orphan gvproxy never came up");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn reaper_reaps_gvproxy_when_supervisor_pipe_closes() {
    if !gated() {
        eprintln!(
            "skipping reaper_reaps_gvproxy_when_supervisor_pipe_closes (set LIMINA_HVF_TESTS=1)"
        );
        return;
    }
    let dir: PathBuf =
        std::env::temp_dir().join(format!("limina-deathpact-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mktmp");
    let sock = dir.join("gw.sock");
    let gpid = launch_orphan_gvproxy(&dir, &sock);
    assert!(pid_is_alive(gpid), "orphan gvproxy should be up");

    // The death-pact pipe: the test stands in for the supervisor and keeps the write end.
    let mut fds = [0 as libc::c_int; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
    let (rfd, wfd) = (fds[0], fds[1]);
    // CLOEXEC on BOTH ends (mirroring the production `spawn_death_pact_watcher`): the spawned
    // reaper must inherit ONLY the read end. If wfd were left inheritable, the reaper would hold
    // its own copy of the write end, so closing OUR wfd below could never deliver EOF — the
    // reaper would block on read() forever, never reap gvproxy, and (holding our inherited
    // stdout) wedge cargo. pre_exec clears CLOEXEC on rfd alone so the reaper still gets it.
    for fd in [rfd, wfd] {
        let f = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(
            f >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFD, f | libc::FD_CLOEXEC) } >= 0,
            "set FD_CLOEXEC"
        );
    }

    // Spawn the REAL shipped reaper subcommand, handing it the read end (clear CLOEXEC on rfd so
    // it inherits; wfd stays CLOEXEC so ONLY the test/"supervisor" holds the write end).
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_limina"));
    cmd.arg("__reap-gateway")
        .arg(rfd.to_string())
        .arg(gpid.to_string());
    unsafe {
        cmd.pre_exec(move || {
            let f = libc::fcntl(rfd, libc::F_GETFD);
            if f < 0 || libc::fcntl(rfd, libc::F_SETFD, f & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut watcher = cmd.spawn().expect("spawn reaper");
    // Only the watcher should hold the read end now.
    unsafe { libc::close(rfd) };

    // While the write end is open, gvproxy must stay up (the watcher is blocked on read).
    std::thread::sleep(Duration::from_millis(250));
    assert!(
        pid_is_alive(gpid),
        "gvproxy must stay up while the death-pact pipe is open"
    );

    // "Supervisor dies": close the write end → watcher reads EOF → reaps gvproxy.
    unsafe { libc::close(wfd) };

    let deadline = Instant::now() + Duration::from_secs(5);
    while pid_is_alive(gpid) {
        assert!(
            Instant::now() < deadline,
            "the reaper did not kill gvproxy after the death-pact pipe closed"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = watcher.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
