// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The host end of a VM's SSH forward (`127.0.0.1:<port>` → the guest's port 22).
//!
//! The worker binds the forward and limina-netd carries its connections into the guest, on
//! either side of the NIC (`docs/design/multi-vm-networking.md` §3.5). The supervisor picks the
//! port, distinct per VM, explicitly (`--ssh-port`) or as the first free one from
//! [`DEFAULT_SSH_PORT`] up ([`allocate_ssh_port`]), so two VMs that both leave it unset still
//! don't fight over 2222. It moves while the VM runs ([`SshForward::set`]) over the worker's
//! network control link, and every worker spawned after a move binds the moved port.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use limina_launch::connect::Endpoint;

/// The host port the first VM's SSH forward takes, and where the scan for a free one starts.
pub const DEFAULT_SSH_PORT: u16 = 2222;

/// The lowest host port a forward may take: below it are the privileged ports.
pub const SSH_PORT_MIN: u16 = 1024;

/// The host end of the guest's SSH forward, shared between the run that started the worker and
/// whoever moves it at runtime (the supervisor's runtime socket). Cloning shares the same forward.
#[derive(Clone)]
pub struct SshForward {
    port: Arc<Mutex<u16>>,
    /// The worker's network control link; `None` only in tests that never move the forward.
    link: Option<Endpoint>,
}

impl SshForward {
    /// The forward a worker holds, moved over its control `link`.
    pub fn on_worker(port: u16, link: Endpoint) -> Self {
        SshForward {
            port: Arc::new(Mutex::new(port)),
            link: Some(link),
        }
    }

    /// A forward with no worker behind it, for tests that never move it.
    #[cfg(test)]
    pub(crate) fn for_tests(port: u16) -> Self {
        SshForward {
            port: Arc::new(Mutex::new(port)),
            link: None,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, u16> {
        self.port.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The host port the forward listens on now.
    pub fn port(&self) -> u16 {
        *self.lock()
    }

    /// Move the forward to `port` without disturbing the guest. The new listener opens before the
    /// old one closes, so a refusal (the port is taken, the worker said no) leaves the old forward
    /// working. Moving to the current port is a no-op.
    pub fn set(&self, port: u16) -> Result<()> {
        let mut current = self.lock();
        if *current == port {
            return Ok(());
        }
        anyhow::ensure!(
            port >= SSH_PORT_MIN,
            "the SSH port must be between {SSH_PORT_MIN} and 65535, got {port}"
        );
        anyhow::ensure!(port_is_free(port), "host port {port} is already in use");
        let link = self
            .link
            .as_ref()
            .context("this forward has no worker to move it")?;
        worker_request(link, &format!("forward 127.0.0.1:{port} 22"))
            .with_context(|| format!("opening the SSH forward on port {port}"))?;
        if let Err(e) = worker_request(link, &format!("unforward 127.0.0.1:{}", *current)) {
            // The guest is reachable on the new port either way; the old one lingering until the
            // worker is next replaced is untidy, not broken.
            log::warn!("forward: could not close the old SSH forward on {current}: {e:#}");
        }
        log::info!("forward: SSH forward moved from {current} to {port}");
        *current = port;
        Ok(())
    }
}

/// One request on a worker's network control link, answered `ok` or `error: …`.
pub(crate) fn worker_request(link: &Endpoint, request: &str) -> Result<()> {
    use std::io::{BufRead, BufReader};
    let stream = link
        .connect()
        .context("reaching the worker's network control")?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    (&stream).write_all(format!("{request}\n").as_bytes())?;
    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply)?;
    match reply.trim_end() {
        "ok" => Ok(()),
        "" => bail!("the worker closed its network control without answering"),
        other => bail!("{}", other.strip_prefix("error: ").unwrap_or(other)),
    }
}

// Host SSH-forward port allocation — pick a free port when the VM didn't request one, so several
// VMs can run in parallel without the caller hand-picking non-colliding ports.
// ---------------------------------------------------------------------------

/// True if `port` can be bound on loopback right now (nothing else holds it). A transient probe:
/// we bind then immediately drop the listener so the worker can take the port. Inherently TOCTOU,
/// but VMs start seconds apart and a worker binds its forward early in its start, so in practice
/// the next VM's scan already sees this one's port held.
pub(crate) fn port_is_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// First free port at or above `base` (scanning up to 65535) per the injected `is_free` probe.
/// Pure (probe injected) so the scan policy is unit-tested without binding real sockets.
fn first_free_port(base: u16, is_free: impl Fn(u16) -> bool) -> Option<u16> {
    (base..=u16::MAX).find(|&p| is_free(p))
}

/// Resolve a VM's host SSH-forward port: honor an explicit `requested` port (erroring early
/// if it is already in use, so the user gets a clear message instead of a silently-dead forward),
/// or auto-allocate the first free port from [`DEFAULT_SSH_PORT`] upward when `None` — what lets
/// two VMs that both omit `--ssh-port` run in parallel without colliding on 2222.
pub(crate) fn allocate_ssh_port(requested: Option<u16>) -> Result<u16> {
    match requested {
        Some(p) if port_is_free(p) => Ok(p),
        Some(p) => bail!(
            "host port {p} is already in use; pick another --ssh-port or omit it to auto-allocate"
        ),
        None => first_free_port(DEFAULT_SSH_PORT, port_is_free)
            .with_context(|| format!("no free host port at or above {DEFAULT_SSH_PORT}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_forward_refuses_a_port_below_the_unprivileged_range() {
        let f = SshForward::for_tests(2222);
        assert!(f.set(80).is_err());
        assert_eq!(f.port(), 2222);
        // Moving to where it already is asks the worker nothing.
        f.set(2222).unwrap();
    }

    #[test]
    fn first_free_port_returns_the_first_unused_from_the_base() {
        // Auto-allocation scans UP from the base, taking the first free port — so the first VM
        // gets the familiar 2222 and each later VM the next slot, with no manual port-picking.
        assert_eq!(
            first_free_port(2222, |_| true),
            Some(2222),
            "all free → base"
        );
        assert_eq!(
            first_free_port(2222, |p| p != 2222),
            Some(2223),
            "base taken → next"
        );
        assert_eq!(
            first_free_port(2222, |p| p >= 2224),
            Some(2224),
            "base + next taken → skip both"
        );
        assert_eq!(
            first_free_port(65535, |_| false),
            None,
            "nothing free in range → None"
        );
    }
}
