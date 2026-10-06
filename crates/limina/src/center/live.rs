// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! What the control center knows about running VMs, from their supervisors.
//!
//! One `watch` connection per running supervisor (`runtime_ctl`), held on its own thread for the
//! supervisor's life: the supervisor pushes every change, and the 1 s refresh only reads the
//! latest report — it never waits on a socket, so a wedged supervisor cannot freeze the window.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::runtime_ctl::{self, Info};

/// How long to keep trying a supervisor that has not answered yet. A fresh one binds its socket
/// within its first moments; one that never does is a build from before the socket.
const CONNECT_PATIENCE: Duration = Duration::from_secs(30);
const CONNECT_RETRY: Duration = Duration::from_millis(250);

enum Link {
    /// A thread is trying to reach it.
    Connecting,
    /// Its latest report.
    Up(Info),
    /// Unreachable, or its connection ended. Not retried: the row falls back to what the VM's
    /// definition says.
    Gone,
}

static LINKS: Mutex<Option<HashMap<i32, Link>>> = Mutex::new(None);

fn with_links<T>(f: impl FnOnce(&mut HashMap<i32, Link>) -> T) -> T {
    let mut guard = LINKS.lock().unwrap_or_else(|p| p.into_inner());
    f(guard.get_or_insert_with(HashMap::new))
}

/// The supervisor's latest report, following it if nobody does yet. `None` until its first
/// report arrives, and for a supervisor that cannot be reached.
pub fn info(pid: i32) -> Option<Info> {
    if pid <= 0 {
        return None;
    }
    let start = with_links(|links| match links.get(&pid) {
        Some(Link::Up(info)) => Some(Some(*info)),
        Some(_) => Some(None),
        None => {
            links.insert(pid, Link::Connecting);
            None
        }
    });
    match start {
        Some(known) => known,
        None => {
            let spawned = std::thread::Builder::new()
                .name(format!("center-live-{pid}"))
                .spawn(move || follow(pid));
            if spawned.is_err() {
                update(pid, Link::Gone);
            }
            None
        }
    }
}

/// Forget every supervisor not in `running`. Their threads end on their own when the
/// connection closes, and find nothing left to update.
pub fn retain(running: &[i32]) {
    with_links(|links| links.retain(|pid, _| running.contains(pid)));
}

/// Replace what is known about `pid`, unless it was forgotten meanwhile.
fn update(pid: i32, link: Link) {
    with_links(|links| {
        if let Some(l) = links.get_mut(&pid) {
            *l = link;
        }
    });
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn follow(pid: i32) {
    let deadline = Instant::now() + CONNECT_PATIENCE;
    loop {
        let mut heard = false;
        let result = runtime_ctl::watch(pid as u32, |info| {
            heard = true;
            update(pid, Link::Up(info));
        });
        if heard {
            // It answered and then went away: the VM stopped (or the link broke, which the
            // next refresh cannot tell apart from that and need not).
            if let Err(e) = result {
                log::info!("control center: lost supervisor {pid}: {e:#}");
            }
            break;
        }
        if !alive(pid) || Instant::now() >= deadline {
            if let Err(e) = result {
                log::info!("control center: supervisor {pid} not reachable: {e:#}");
            }
            break;
        }
        std::thread::sleep(CONNECT_RETRY);
    }
    update(pid, Link::Gone);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End to end against this process's own runtime socket: the center hears the supervisor's
    /// state without asking again, and hears it change.
    #[test]
    fn the_center_follows_a_supervisor_through_a_change() {
        let _serial = runtime_ctl::tests::FORWARD_TESTS
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let me = std::process::id() as i32;
        runtime_ctl::set_forward(None);
        runtime_ctl::serve().unwrap();

        let until = |want: Option<u16>| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if info(me).map(|i| i.ssh_port) == Some(want) {
                    return;
                }
                assert!(Instant::now() < deadline, "never saw ssh-port {want:?}");
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        until(None);
        runtime_ctl::set_forward(Some(crate::gateway::SshForward::for_tests(2299)));
        until(Some(2299));

        retain(&[]);
        runtime_ctl::set_forward(None);
        runtime_ctl::cleanup();
    }
}
