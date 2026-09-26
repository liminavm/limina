// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The host side of the limina-proto control plane (M5/D8).
//!
//! The supervisor owns this channel: it binds a unix socket the worker bridges to the
//! guest's vsock (`CID_HOST:CONTROL_PORT`), accepts agents as they connect, answers
//! HELLO with WELCOME, tracks the connected peers, and — the first real payoff — turns
//! window-close / SIGTERM into an **orderly guest power-off** by sending SHUTDOWN and
//! letting the agent run the guest's own shutdown path, instead of going straight to the
//! GPIO power button, which stock EFI guests ignore.
//!
//! The plane serves **multiple concurrent connections** (the clipboard spike settled the
//! guest topology: a root `limina-agent` plus per-session user helpers, each with its own
//! vsock connection and capability set — vsock connect needs no root). Each peer gets its
//! own serve thread and registry entry; requests are routed by capability (SHUTDOWN goes
//! to every `shutdown`-capable peer; power-off is idempotent, first one wins).
//!
//! Everything here is opportunistic: a guest without an agent simply never connects and
//! every caller falls back to the pre-existing teardown ladder. Agents may also
//! reconnect (guest reboot), so the accept loop runs for the supervisor's lifetime.

use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// The peer registry and each peer's write half are loom's locks in this crate's own tests under
// `--cfg loom`, so `loom_model` can interleave an agent joining with a host copy being offered.
#[cfg(all(test, loom))]
use loom::sync::Mutex as PeerMutex;
#[cfg(not(all(test, loom)))]
use std::sync::Mutex as PeerMutex;

use crate::clipboard::{Clipboard, PasteboardServer};
use anyhow::{Context, Result};
use limina_proto::{
    CHANNEL_CLIPBOARD, CHANNEL_CONTROL, Message, Shutdown, Welcome, read_message, write_message,
};

/// How long the orderly path gets before the caller escalates to the next rung (the power
/// button, then the stock guest agent).
pub const AGENT_GRACE: Duration = Duration::from_secs(5);

/// Socket path to remove on exit (the windowed path leaves via `process::exit`, which
/// skips destructors — same pattern as `gateway::cleanup`).
static CLEANUP_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Remove the control socket file (idempotent; safe from any exit path).
///
/// A SIGKILLed supervisor still leaves its socket behind; that is harmless, because the next
/// bind unlinks the path first. **Do not reap other runs' leftovers by testing whether the pid
/// in the name is alive**: pids are recycled, and a sweep of 6450 accumulated sockets that way
/// found five "live" ones that all belonged to unrelated system daemons.
pub fn cleanup() {
    if let Some(path) = CLEANUP_PATH.lock().unwrap().take() {
        let _ = std::fs::remove_file(path);
    }
}

/// The supervisor's handle to the control plane. Cheap to clone (Arc inside).
#[derive(Clone)]
pub struct ControlPlane {
    inner: Arc<Inner>,
}

/// A connected, handshaken agent: the write half of its stream plus what it declared
/// in HELLO. Registered after WELCOME, removed when its serve thread ends (or a write
/// to it fails).
///
/// The write half is mutexed because a peer has TWO writers: its serve thread (replies)
/// and broadcasters (the clipboard poller, shutdown requests). `write_message` is two
/// `write_all`s, so unsynchronized writers could interleave mid-frame and corrupt the
/// stream.
///
/// Generic over the write half only so `loom_model` can read back what each peer was sent.
struct Peer<W = UnixStream> {
    id: u64,
    agent: String,
    caps: Vec<String>,
    stream: Arc<PeerMutex<W>>,
    /// When the peer last said anything (updated by its serve loop on every inbound
    /// message — heartbeats included). The liveness monitor reads this.
    last_seen: Arc<Mutex<Instant>>,
    /// Whether the monitor currently considers this peer silent (so transitions are
    /// logged once, not every sweep).
    silent: Arc<AtomicBool>,
}

impl<W: Write> Peer<W> {
    fn has_cap(&self, cap: &str) -> bool {
        self.caps.iter().any(|c| c == cap)
    }

    fn send(&self, msg: &Message, channel: u32) -> std::io::Result<()> {
        write_message(&mut *self.stream.lock().unwrap(), channel, msg)
    }
}

/// How long a peer may go without any inbound message before the supervisor reports it
/// silent. Agents heartbeat every second, so the default 5s means ~5 missed beats.
/// Override with `LIMINA_AGENT_SILENT_SECS` (tests shorten it).
fn silent_threshold() -> Duration {
    let secs = std::env::var("LIMINA_AGENT_SILENT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    Duration::from_secs(secs)
}

/// How long a write to a peer may block before we declare the peer wedged and drop it.
/// A peer that hasn't drained its socket buffer for this long has effectively stopped
/// serving its side of the protocol; without a bound, one such peer blocks whoever is
/// sending to it forever — historically the Ctrl-C shutdown ladder, since a blocking
/// send also serialized against registration and the liveness sweep. Override with
/// `LIMINA_CONTROL_WRITE_TIMEOUT_MS` (tests shorten it); 0 disables the bound.
fn write_timeout() -> Option<Duration> {
    let ms = std::env::var("LIMINA_CONTROL_WRITE_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000u64);
    (ms > 0).then(|| Duration::from_millis(ms))
}

/// One worker spawn's guest-agent port, plus the little state the periodic trim keeps about
/// it. Bound together because both timers reset at exactly the same moment — a reboot or a
/// resume makes a new port, and a fresh guest has neither settled nor been trimmed.
struct QgaSlot {
    client: Arc<crate::qga::client::Qga>,
    attached_at: Instant,
    last_trim: Option<Instant>,
}

struct Inner {
    peers: Peers,
    next_id: AtomicU64,
    /// The one pasteboard owner, shared with the vdagent transport (M12 #37): both the
    /// control plane's peers and `spice-vdagent` feed the same clipboard state.
    clipboard: Arc<crate::clipboard::Clipboard>,
    /// The vdagent conversation, once a worker spawn has handed us the port's host end.
    /// `None` before that, and on a guest with no spice port at all.
    vdagent: Mutex<Option<Arc<crate::vdagent::broker::VdAgent>>>,
    /// The stock `qemu-guest-agent` port, once a worker spawn has handed us its host end.
    /// `None` before that; present-but-silent on a guest with no agent installed.
    qga: Mutex<Option<QgaSlot>>,
    /// The guest's last-reported PSI `full` IO `avg10`, and when it said so. `None` on a
    /// stock guest, which never reports — the state [`crate::qga::trim::Gate`] must not read
    /// as "busy". A reading that stops being refreshed expires the same way; see
    /// [`crate::qga::trim::fresh_psi`].
    guest_io_full_avg10: Mutex<Option<(Instant, u32)>>,
    /// M6 PSI autoballoon policy, driven by guest `MemPressure` reports. `None` unless `--memory`
    /// configured a dynamic range.
    balloon_policy: Option<crate::balloon_policy::BalloonPolicy>,
    /// Dynamic vCPU policy, driven by guest `CpuPressure` reports. `None` unless
    /// `--cpu-reclaim` asked for one (and the VM has vCPUs to spare) — and `None` is what
    /// withholds the `vcpu` capability, so a guest whose host has no policy never reports.
    vcpu_policy: Option<crate::vcpu_policy::VcpuPolicy>,
    /// The host policy the guest's currently selected power profile asks for. Level-triggered:
    /// the guest sends its whole profile, never a delta, so this is simply overwritten and a
    /// reconnect or restore resynchronises for free. `Balanced` until a guest says otherwise,
    /// which is exactly today's defaults.
    power_policy: Mutex<crate::power_profile::PowerPolicy>,
    /// The guest's last-reported online vCPU count, and when it said so — from EITHER tier (the
    /// enhanced agent's `CpuPressure` or a QGA `guest-get-vcpus` poll). The only statement about
    /// online state anyone trusts (nothing acks a target), so it is what
    /// [`ControlPlane::restore_all_vcpus`] waits on.
    guest_vcpus_online: Mutex<Option<(Instant, u32)>>,
    /// When the ENHANCED agent last sent a `CpuPressure`. Kept apart from the reading above
    /// because it answers a different question: not "how many are online" but "is limina-agent
    /// driving this?". While it is fresh the QGA poller stands down — the agent's signal is
    /// richer and pushed rather than polled, so a guest that has one should not also be polled.
    agent_cpu_report_at: Mutex<Option<Instant>>,
    /// The online count of the last `CpuTarget` sent to the enhanced agent, `None` if none ever
    /// was. Lets [`ControlPlane::restore_all_vcpus`] tell "all online and nothing asked of the
    /// guest since" (no wait needed) from "a shrink may still be in flight".
    last_cpu_target: Mutex<Option<u32>>,
    /// Consecutive QGA vCPU failures. Fedora leaves every RPC enabled and gates the agent in
    /// SELinux instead (`virt_qemu_ga_t`), so a `guest-set-vcpus` that is refused looks like a
    /// normal error reply — and would otherwise be retried every tick forever. After
    /// [`QGA_VCPU_GIVE_UP`] in a row we stop asking this guest.
    qga_vcpu_failures: AtomicU64,
    /// M14 virtual FIDO authenticator: the per-VM passkey store, shared by every peer.
    /// `None` when this host has no Secure Enclave — then the `fido` capability is never
    /// advertised and a guest presents no authenticator (stock-degrade rule).
    fido_store: Option<Arc<crate::fido::store::FidoStore>>,
    /// Is the USB FIDO gadget serving this VM? Then the agent must NOT stand up a second
    /// authenticator — see [`welcome_caps`].
    usb_fido_gadget: bool,
}

impl ControlPlane {
    /// Bind `socket_path` and start the accept thread (which spawns a serve thread per
    /// connection). The returned handle is what shutdown paths use; the threads run for
    /// the process's lifetime.
    /// `fido_store` is the shared per-VM passkey store (built by [`crate::fido::store_if_capable`]
    /// — `Some` only where a Secure Enclave, or the test-approve knob, can back the authenticator).
    /// It is shared with the USB gadget transport so both speak to one store; `None` advertises
    /// no `fido` capability at all. `usb_fido_gadget` says whether that gadget is the transport
    /// this run serves — if it is, the agent must not raise a second authenticator, so the `fido`
    /// capability is withheld ([`welcome_caps`]).
    pub fn start(
        socket_path: &Path,
        balloon_policy: Option<crate::balloon_policy::BalloonPolicy>,
        vcpu_policy: Option<crate::vcpu_policy::VcpuPolicy>,
        fido_store: Option<Arc<crate::fido::store::FidoStore>>,
        usb_fido_gadget: bool,
    ) -> Result<ControlPlane> {
        let _ = std::fs::remove_file(socket_path);
        let listener = UnixListener::bind(socket_path)
            .with_context(|| format!("binding control socket {socket_path:?}"))?;
        *CLEANUP_PATH.lock().unwrap() = Some(socket_path.to_path_buf());

        let inner = Arc::new(Inner {
            peers: PeerMutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            power_policy: Mutex::new(crate::power_profile::PowerProfile::default().policy()),
            clipboard: Arc::new(crate::clipboard::Clipboard::new()),
            vdagent: Mutex::new(None),
            qga: Mutex::new(None),
            guest_io_full_avg10: Mutex::new(None),
            balloon_policy,
            vcpu_policy,
            guest_vcpus_online: Mutex::new(None),
            last_cpu_target: Mutex::new(None),
            agent_cpu_report_at: Mutex::new(None),
            qga_vcpu_failures: AtomicU64::new(0),
            fido_store,
            usb_fido_gadget,
        });
        let serve_inner = inner.clone();
        std::thread::Builder::new()
            .name("limina-control".into())
            .spawn(move || accept_loop(listener, serve_inner))
            .context("spawning the control-plane thread")?;

        // The clipboard poller: macOS has no pasteboard-change notification, so watch
        // changeCount and tell every transport about host copies.
        //
        // ONE poller for both transports, on purpose. `poll_local_change` consumes the
        // change (it advances the change-count high-water mark), so a second poller for
        // the vdagent port would see "no change" for copies this one already took, and
        // guests would get whichever half of the copies happened to land on their poller.
        let poll_inner = inner.clone();
        std::thread::Builder::new()
            .name("limina-clipboard".into())
            .spawn(move || {
                let every = crate::clipboard::Clipboard::poll_interval();
                loop {
                    std::thread::sleep(every);
                    let Some(text) = poll_inner.clipboard.poll_local_change() else {
                        continue;
                    };
                    if let Some(vdagent) = poll_inner.vdagent.lock().unwrap().clone() {
                        vdagent.host_copy(text.clone());
                    }
                    offer_host_copy(&poll_inner.peers, &poll_inner.clipboard, text);
                }
            })
            .context("spawning the clipboard poll thread")?;

        // The guest-clock sync sender. The guest kernel's CLOCK_REALTIME is CNTVCT-anchored
        // and CNTVCT freezes while the HOST sleeps, so a host nap lags a running guest's
        // clock by the nap's length (a 6h drift observed in dogfooding). Send the host wallclock to
        // timesync-capable agents: right after a detected host sleep (the oversleep trick —
        // a 2s tick that took ≥3× longer means the host napped), and periodically as drift
        // insurance. The on-connect seed lives in serve_agent (covers boot + post-restore
        // reconnect). Policy knob: LIMINA_TIMESYNC_SECS (default 60; tests shrink it).
        let tsync_inner = inner.clone();
        std::thread::Builder::new()
            .name("limina-timesync".into())
            .spawn(move || {
                const TICK: Duration = Duration::from_secs(2);
                let interval = std::env::var("LIMINA_TIMESYNC_SECS")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(Duration::from_secs)
                    .unwrap_or(Duration::from_secs(60));
                // This thread also carries the periodic guest trim. It rides here rather than
                // on a thread of its own because the two are the same shape — a rare, bounded
                // errand against the same port — and because sharing the tick is what lets
                // `time_sample` skip a beat while a trim holds the port instead of queueing
                // behind it. `None` = trimming switched off.
                let trim_interval = crate::qga::trim::interval_from_env();
                // NB: measured on the WALLCLOCK, not Instant — macOS Instant (mach
                // absolute time) freezes during host sleep, which would blind this
                // detector to exactly the event it exists to catch.
                let mut last_sent = std::time::SystemTime::now();
                let mut last_awake = std::time::SystemTime::now();
                loop {
                    std::thread::sleep(TICK);
                    let now = std::time::SystemTime::now();
                    let asleep = now
                        .duration_since(last_awake)
                        .unwrap_or(Duration::ZERO)
                        .saturating_sub(TICK);
                    let overslept = asleep >= TICK * 2;
                    let due = now
                        .duration_since(last_sent)
                        .map(|d| d >= interval)
                        .unwrap_or(true);
                    if overslept || due {
                        if overslept {
                            log::info!(
                                "control: host slept ~{:.0}s; syncing guest clocks",
                                asleep.as_secs_f64()
                            );
                        }
                        let took = tsync_inner.send_to_capable(
                            "timesync",
                            &time_sync_now(),
                            CHANNEL_CONTROL,
                        );
                        // Nobody capable is connected — a stock guest, or one whose agent
                        // is not running. Its clock has no other corrector: the RTC only
                        // helps if the guest kernel re-reads it (s2idle thaw), which a guest
                        // that stayed "running" through the host's nap never does. Ask the
                        // stock qemu-guest-agent instead, if one is there.
                        if took.is_empty() {
                            tsync_inner.qga_sync_clock();
                        }
                        last_sent = now;
                    }
                    if let Some(every) = trim_interval {
                        tsync_inner.qga_trim_tick(every);
                    }
                    // The stock tier's dynamic-vCPU loop. No-op unless a policy is configured
                    // AND no limina-agent is reporting — see `qga_vcpu_tick`.
                    tsync_inner.qga_vcpu_tick();
                    last_awake = std::time::SystemTime::now();
                }
            })
            .context("spawning the timesync thread")?;

        // The liveness monitor: agents heartbeat every second; report (once) any peer
        // that goes quiet past the threshold, and its recovery. This is the signal a
        // status surface (CLI/UI) consumes later — for now the supervisor log IS the
        // surface.
        let live_inner = inner.clone();
        std::thread::Builder::new()
            .name("limina-liveness".into())
            .spawn(move || {
                let threshold = silent_threshold();
                loop {
                    std::thread::sleep(threshold.min(Duration::from_secs(1)));
                    for peer in live_inner.peers.lock().unwrap().iter() {
                        let quiet = peer.last_seen.lock().unwrap().elapsed();
                        if quiet > threshold {
                            if !peer.silent.swap(true, Ordering::Relaxed) {
                                log::warn!(
                                    "control: agent {} silent for {:.1}s (no heartbeat)",
                                    peer.agent,
                                    quiet.as_secs_f64()
                                );
                            }
                        } else if peer.silent.swap(false, Ordering::Relaxed) {
                            log::info!("control: agent {} heartbeating again", peer.agent);
                        }
                    }
                }
            })
            .context("spawning the liveness monitor thread")?;
        Ok(ControlPlane { inner })
    }

    /// Tell the vCPU policy which process to read CPU time from — the worker, once spawned.
    ///
    /// The control plane is built before the worker exists, so the pid arrives afterwards. No-op
    /// without a policy.
    pub fn watch_worker(&self, pid: libc::pid_t) {
        if let Some(policy) = &self.inner.vcpu_policy {
            policy.watch_worker(pid);
        }
    }

    /// Ask the guest to bring every vCPU back online, and wait (briefly) until it says it did.
    ///
    /// The suspend bracket calls this before snapshotting. Task #41: the guest-visible online
    /// state lives in libkrun's `VcpuList` and is NOT in the M9 snapshot, so a snapshot taken
    /// while a vCPU is offline restores it as online and the guest kernel's bookkeeping diverges
    /// from the host's. Rather than change the snapshot format for it, we make sure a snapshot
    /// never contains an offline vCPU in the first place — which also costs nothing on the
    /// overwhelmingly common path, since a VM idle enough to be suspended is exactly the one the
    /// policy has been shrinking.
    ///
    /// Returns whether the guest confirmed. `false` is not a failure to act on: the bracket
    /// proceeds either way (a snapshot the user asked for must not be held hostage to a vCPU
    /// count), and a divergent restore is graceful — a later re-online times out rather than
    /// wedging, and the policy re-derives from the guest's own report on the next tick.
    pub fn restore_all_vcpus(&self, wait: Duration) -> bool {
        let Some(policy) = &self.inner.vcpu_policy else {
            return true; // no policy, so nothing was ever offlined by us
        };
        let target = policy.max_target();
        // What the guest last told us, and whether a shrink of ours could still be in flight
        // behind it — the guest applies a target AFTER the report it answers, so an "all online"
        // report can predate a shrink we sent in reply to it.
        let all_online = self
            .inner
            .guest_vcpus_online
            .lock()
            .unwrap()
            .is_some_and(|(_, online)| online >= target.online);
        let nothing_outstanding = self
            .inner
            .last_cpu_target
            .lock()
            .unwrap()
            .is_none_or(|t| t >= target.online);
        let asked = Instant::now();
        let sent_to =
            self.inner
                .send_to_capable("vcpu", &Message::CpuTarget(target), CHANNEL_CONTROL);
        if sent_to.is_empty() {
            // No enhanced agent. The stock tier may still have offlined vCPUs through QGA, and it
            // is driven by a poll rather than a push — so ask it directly rather than waiting for
            // a report nothing is going to send.
            return self.restore_all_vcpus_via_qga(target.online);
        }
        *self.inner.last_cpu_target.lock().unwrap() = Some(target.online);
        // Enhanced tier only: `last_cpu_target` tracks what the agent was sent, not what QGA did.
        if all_online && nothing_outstanding {
            log::info!(
                "suspend: all {} vCPUs already online and no shrink outstanding; not waiting (#41)",
                target.online
            );
            return true;
        }
        log::info!(
            "suspend: asking the guest for all {} vCPUs before the snapshot (#41)",
            target.online
        );
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            // Only a report made AFTER we asked can confirm the ask.
            if let Some((at, online)) = *self.inner.guest_vcpus_online.lock().unwrap()
                && at > asked
                && online >= target.online
            {
                log::info!(
                    "suspend: the guest confirmed all {} vCPUs online after {:?}",
                    target.online,
                    asked.elapsed()
                );
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        log::warn!(
            "suspend: the guest did not confirm all {} vCPUs online within {wait:?}; \
             snapshotting anyway (a restore may see a diverged online count — #41)",
            target.online
        );
        false
    }

    /// Ask the guest to power off, via every connected `shutdown`-capable agent (the
    /// guest's power-off is idempotent — whichever acts first wins). Returns `true` if
    /// at least one request was sent (the caller should give it [`AGENT_GRACE`] before
    /// escalating); `false` if no capable agent is connected or every send failed
    /// (escalate immediately). Peers whose send fails are dropped from the registry.
    pub fn request_shutdown(&self, grace: Duration) -> bool {
        let msg = Message::Shutdown(Shutdown {
            grace_ms: grace.as_millis() as u64,
        });
        let mut sent = false;
        for peer in self
            .inner
            .send_to_capable("shutdown", &msg, CHANNEL_CONTROL)
        {
            log::info!("control: SHUTDOWN sent to {peer}");
            sent = true;
        }
        sent
    }

    /// Ask a **stock** guest's `qemu-guest-agent` to power the guest off — the last rung of the
    /// stop ladder, tried after the GPIO power button has had its chance. There is no rung after
    /// it: a guest that ignores this one keeps running until the user forces it down.
    ///
    /// `false` means nothing was sent (no agent has ever answered on this port, or it does not
    /// offer `guest-shutdown`), so the caller should stop waiting rather than spend the
    /// remaining grace on a guest nobody asked.
    pub fn request_qga_shutdown(&self) -> bool {
        let Some(qga) = self
            .inner
            .qga
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.client.clone())
        else {
            return false;
        };
        match qga.shutdown() {
            Ok(()) => {
                log::info!("qga: asked the stock guest agent to power off");
                true
            }
            Err(e) => {
                log::debug!("qga: no guest-agent power-off ({e:#})");
                false
            }
        }
    }

    /// Adopt the host end of a worker's `org.qemu.guest_agent.0` port.
    ///
    /// Called once per worker *spawn*, like [`Self::attach_vdagent`]: the fresh port replaces
    /// the previous client, whose guest is gone. Nothing is asked of the agent here — at
    /// spawn time the guest has not booted — so this cannot tell whether an agent exists;
    /// the first use finds out.
    pub fn attach_qga(&self, host_fd: std::os::fd::OwnedFd) -> Result<()> {
        let qga = Arc::new(crate::qga::client::Qga::start(host_fd)?);
        *self.inner.qga.lock().unwrap() = Some(QgaSlot {
            client: qga.clone(),
            attached_at: Instant::now(),
            last_trim: None,
        });
        if let Some(dir) = crate::qga::bootstrap::dir_from_env() {
            self.inner.clone().spawn_bootstrap(qga, dir);
        }
        Ok(())
    }

    /// Adopt the host end of a worker's `com.redhat.spice.0` port and start brokering the
    /// clipboard over it (M12 #37).
    ///
    /// Called once per worker *spawn*: a relaunch (guest reboot, resume) creates a new
    /// socketpair and calls this again, which drops the previous broker — its reader
    /// thread is already unblocking on the closed socket — and greets the new guest from a
    /// clean state. That is the "announce once per port open" boundary.
    ///
    /// A failure here costs the stock-tier clipboard and nothing else, so callers log and
    /// carry on rather than failing the VM.
    pub fn attach_vdagent(&self, host_fd: std::os::fd::OwnedFd) -> Result<()> {
        let agent = crate::vdagent::broker::VdAgent::start(host_fd, self.inner.clipboard.clone())?;
        *self.inner.vdagent.lock().unwrap() = Some(agent);
        Ok(())
    }

    /// The stock tier's half of [`ControlPlane::restore_all_vcpus`]: bring every guest vCPU back
    /// through `guest-set-vcpus` and confirm with a re-read.
    ///
    /// Returns true when there was nothing to do or the guest confirms every vCPU online. A guest
    /// we cannot reach, or one whose agent refuses the write, returns false — and the caller
    /// snapshots anyway, because a snapshot must never be held hostage to a vCPU count.
    fn restore_all_vcpus_via_qga(&self, want_online: u32) -> bool {
        let Some(qga) = self
            .inner
            .qga
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.client.clone())
        else {
            return true; // no stock agent either; nothing of ours can be offline
        };
        if !qga.ready() {
            return true;
        }
        let Ok(vcpus) = qga.get_vcpus() else {
            return true; // never read the state, so we never offlined anything through it
        };
        let plan = crate::qga::vcpu::plan(&vcpus, want_online);
        if plan.is_empty() {
            return true;
        }
        log::info!(
            "suspend: asking the stock guest agent for all {want_online} vCPUs before the \
             snapshot (#41)"
        );
        if let Err(e) = qga.set_vcpus(&plan) {
            log::warn!(
                "suspend: guest-set-vcpus failed ({e:#}); snapshotting anyway (a restore may see \
                 a diverged online count — #41)"
            );
            return false;
        }
        // Believe the re-read, not the call: the agent reports a partial success as a count.
        match qga.get_vcpus() {
            Ok(after) => {
                let online = after.iter().filter(|v| v.online).count() as u32;
                if online >= want_online {
                    *self.inner.guest_vcpus_online.lock().unwrap() = Some((Instant::now(), online));
                    return true;
                }
                log::warn!(
                    "suspend: the stock guest came back with {online} of {want_online} vCPUs \
                     online; snapshotting anyway (#41)"
                );
                false
            }
            Err(_) => false,
        }
    }
}

impl Inner {
    /// Apply the host policy a guest-selected power profile asks for.
    ///
    /// Idempotent, because the guest reports level-triggered: the same profile arriving twice
    /// (a reconnect, a restore, a re-announce) must not disturb anything. That is also why
    /// nothing here is expressed as a delta.
    fn apply_power_profile(
        &self,
        profile: crate::power_profile::PowerProfile,
    ) -> Option<limina_proto::CpuTarget> {
        let want = profile.policy();
        {
            let mut held = self.power_policy.lock().unwrap();
            if *held == want {
                return None;
            }
            log::info!(
                "power: guest selected {} (little {:?}, rt band {}, reclaim {:?})",
                profile.as_str(),
                want.little,
                if want.rt_band { "on" } else { "off" },
                want.cpu_reclaim
            );
            *held = want;
        }

        // vCPU reclaim is ours to apply directly. A floor that ROSE means the machine must come
        // back now — a VM shrunk under power-saver and switched back to balanced must not wait
        // for the next pressure report to notice.
        let grow = self
            .vcpu_policy
            .as_ref()
            .filter(|policy| policy.set_reclaim(want.cpu_reclaim))
            .map(|policy| policy.max_target());

        // `little` and `rt_band` live in the worker: a QoS override needs the vCPU thread's
        // pthread_t, which only its own process has. They are held above for the worker channel
        // to read; until that lands the profile still moves reclaim, and `balanced` — what a
        // guest reports unless its user says otherwise — asks for today's defaults anyway.
        grow
    }

    /// Send `msg` to every peer with `cap`, returning the agents that took it and
    /// dropping any whose send fails (dead or wedged connection). See [`send_to_capable`].
    fn send_to_capable(&self, cap: &str, msg: &Message, channel: u32) -> Vec<String> {
        send_to_capable(&self.peers, cap, msg, channel)
    }

    /// Deliver a bootstrap kit into a guest that has no `limina-agent` (`crate::qga::bootstrap`).
    ///
    /// A thread of its own, and a detached one: it sleeps out the grace window and then holds
    /// the port for as long as the transfer and the kit's installer take, which is minutes.
    /// Nothing waits for it, and nothing fails because of it — a bootstrap that does not
    /// happen costs the enhanced tier, never the VM.
    fn spawn_bootstrap(
        self: Arc<Self>,
        qga: Arc<crate::qga::client::Qga>,
        dir: std::path::PathBuf,
    ) {
        let spawned = std::thread::Builder::new()
            .name("limina-qga-bootstrap".into())
            .spawn(move || {
                let grace = crate::qga::bootstrap::grace_from_env();
                std::thread::sleep(grace);
                // The whole point of the wait: a guest that brought up its own agent needs no
                // bootstrap, and reinstalling under a healthy one is a way to break it.
                let enhanced = crate::qga::bootstrap::AGENT;
                if self
                    .peers
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|p| p.agent.starts_with(enhanced))
                {
                    log::info!(
                        "qga: the guest already runs {}; not deploying the bootstrap kit",
                        enhanced.trim_end_matches('/')
                    );
                    return;
                }
                let kit = match crate::qga::bootstrap::load(&dir) {
                    Ok(kit) => kit,
                    Err(e) => {
                        // A named kit that cannot be read is a misconfiguration, not a guest
                        // property — say it at warn, where someone will see it.
                        log::warn!("qga: {e:#}");
                        return;
                    }
                };
                log::info!(
                    "qga: no {} after {}s; deploying the bootstrap kit from {} ({} file(s), {:.0} KiB)",
                    enhanced.trim_end_matches('/'),
                    grace.as_secs(),
                    dir.display(),
                    kit.files.len(),
                    kit.bytes() as f64 / 1024.0
                );
                if let Err(e) = crate::qga::bootstrap::deploy(&qga, &kit) {
                    log::warn!("qga: the bootstrap kit did not install ({e:#})");
                }
            });
        if let Err(e) = spawned {
            log::warn!("qga: could not spawn the bootstrap thread: {e}");
        }
    }

    /// One tick of the stock tier's dynamic-vCPU loop, over the stock `qemu-guest-agent`.
    ///
    /// This is what lets a guest with NO limina components take part: `guest-get-load` is the
    /// sensor, `guest-get-vcpus`/`guest-set-vcpus` the actuator, and the host policy in between is
    /// the very same [`crate::vcpu_policy::VcpuPolicy`] the enhanced tier drives. Per the two-tier
    /// rule, the enhanced agent makes this better — a richer signal, pushed rather than polled —
    /// but is not a precondition for having it.
    ///
    /// The signal is coarser: QGA offers no `nr_running` and no PSI, so the synthesized report
    /// carries loadavg alone. The policy needs no special case for that, because loadavg is
    /// already the strict half of its shrink gate; a missing `nr_running` reads as 0, which
    /// simply lets the load term decide.
    fn qga_vcpu_tick(&self) {
        let Some(policy) = &self.vcpu_policy else {
            return;
        };
        // The enhanced agent owns this whenever it is present and talking.
        if let Some(at) = *self.agent_cpu_report_at.lock().unwrap()
            && at.elapsed() < AGENT_CPU_FRESH
        {
            return;
        }
        if self.qga_vcpu_failures.load(Ordering::Relaxed) >= QGA_VCPU_GIVE_UP {
            return;
        }
        let Some(qga) = self.qga.lock().unwrap().as_ref().map(|s| s.client.clone()) else {
            return;
        };
        if !qga.ready() {
            return;
        }
        let caps = qga.caps();
        if !caps.has("guest-get-vcpus") || !caps.has("guest-get-load") {
            // An older qemu-ga simply cannot play. Say so once, then stop looking.
            if self
                .qga_vcpu_failures
                .fetch_add(QGA_VCPU_GIVE_UP, Ordering::Relaxed)
                == 0
            {
                log::info!(
                    "qga: this guest agent has no guest-get-load/guest-get-vcpus; the stock tier \
                     keeps every vCPU online"
                );
            }
            return;
        }

        let (vcpus, load1, load5) = match (qga.get_vcpus(), qga.load()) {
            (Ok(v), Ok((l1, l5, _))) => (v, l1, l5),
            (Err(e), _) | (_, Err(e)) => {
                let n = self.qga_vcpu_failures.fetch_add(1, Ordering::Relaxed) + 1;
                log::debug!("qga: could not read the guest's vCPU state ({e:#})");
                if n >= QGA_VCPU_GIVE_UP {
                    log::info!(
                        "qga: giving up on stock-tier vCPU offlining for this guest after {n} \
                         failures; every vCPU stays online"
                    );
                }
                return;
            }
        };
        let online = vcpus.iter().filter(|v| v.online).count() as u32;
        if online == 0 {
            return; // an empty or nonsense reading is not a reading
        }
        *self.guest_vcpus_online.lock().unwrap() = Some((Instant::now(), online));
        let report = limina_proto::CpuPressure {
            nr_running: 0,
            loadavg1_x100: load1,
            loadavg5_x100: load5,
            online,
            present: vcpus.len() as u32,
            ..Default::default()
        };
        let Some(target) = policy.on_pressure(&report) else {
            self.qga_vcpu_failures.store(0, Ordering::Relaxed);
            return;
        };
        let plan = crate::qga::vcpu::plan(&vcpus, target.online);
        if plan.is_empty() {
            return;
        }
        match qga.set_vcpus(&plan) {
            Ok(changed) => {
                self.qga_vcpu_failures.store(0, Ordering::Relaxed);
                log::info!(
                    "qga: asked the stock guest for {} online vCPU(s); it changed {changed}",
                    target.online
                );
            }
            Err(e) => {
                let n = self.qga_vcpu_failures.fetch_add(1, Ordering::Relaxed) + 1;
                log::warn!("qga: guest-set-vcpus failed ({e:#})");
                if n >= QGA_VCPU_GIVE_UP {
                    log::warn!(
                        "qga: stock-tier vCPU offlining gave up after {n} refusals. On an \
                         SELinux-Enforcing Fedora guest the likeliest reason is the \
                         virt_qemu_ga_t domain, not the command being absent — an enabled RPC \
                         only means the agent will attempt it."
                    );
                }
            }
        }
    }

    /// One tick of the periodic guest trim (`crate::qga::trim`).
    ///
    /// Runs on the timesync thread, whose 2 s tick is far finer than the hours-long cadence —
    /// so this is written to cost almost nothing on the overwhelming majority of ticks: a
    /// mutex and two `Instant` comparisons before anything is measured or asked.
    ///
    /// The trim itself runs on a **detached thread**, because it can hold the port for
    /// minutes. Nothing waits for it: the timesync tick that follows takes the port only if
    /// it is free ([`crate::qga::client::Qga::time_sample`]), and a trim that fails is a
    /// missed housekeeping pass, never a fault the VM should notice.
    fn qga_trim_tick(&self, interval: Duration) {
        let now = Instant::now();
        let client = {
            let mut slot = self.qga.lock().unwrap();
            let Some(slot) = slot.as_mut() else { return };
            if !crate::qga::trim::due(now, slot.attached_at, slot.last_trim, interval) {
                return;
            }
            // Deliberately NOT gated on `client.ready()`. The client only probes when someone
            // asks it something, and on an ENHANCED guest nobody does — `limina-agent` takes
            // the clock, so the qga port is never touched and `ready()` would be false
            // forever. The trim is allowed to be the first question asked on this port; the
            // client's own probe-and-backoff handles a guest with no agent at all.
            let last_psi = *self.guest_io_full_avg10.lock().unwrap();
            let gate = crate::qga::trim::Gate {
                host_calm: crate::balloon_policy::sample_host_pressure().blended
                    == crate::balloon_policy::HostPressure::Normal,
                guest_io_full_avg10: crate::qga::trim::fresh_psi(now, last_psi),
            };
            if let Err(why) = crate::qga::trim::gate_ok(gate) {
                log::debug!("qga: not trimming now — {why}");
                return;
            }
            // Charge the cadence up front: the trim is about to run for minutes, and a
            // second one must not start behind it.
            slot.last_trim = Some(now);
            slot.client.clone()
        };

        let spawned = std::thread::Builder::new()
            .name("limina-qga-trim".into())
            .spawn(move || match client.fstrim(crate::qga::trim::MIN_EXTENT) {
                Ok(r) => {
                    log::info!(
                        "qga: trimmed {} guest filesystem(s); the agent walked {:.1} GiB of free \
                         ranges (host space returned is smaller){}",
                        r.trimmed,
                        r.walked_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
                        if r.failed.is_empty() {
                            String::new()
                        } else {
                            format!("; skipped: {}", r.failed.join(", "))
                        }
                    );
                }
                // No agent installed, an agent that blocks `guest-fstrim`, a filesystem that
                // refused — all the same thing here: a housekeeping pass that did not happen,
                // and nothing the VM should notice. The next cadence tries again.
                Err(e) => log::debug!("qga: the guest trim did not run ({e:#})"),
            });
        if let Err(e) = spawned {
            log::warn!("qga: could not spawn the trim thread: {e}");
        }
    }

    /// Correct a **stock** guest's clock through `qemu-guest-agent`.
    ///
    /// The last rung of the clock ladder, reached only when no `timesync`-capable peer took
    /// the host's `TimeSync` — the enhanced tier always wins, and both mechanisms stepping
    /// the same clock would just fight. Runs on the `limina-timesync` thread, so it inherits
    /// both of that thread's triggers for free: the oversleep detector (the host napped and
    /// the guest's counter did not) and the periodic tick (drift insurance).
    ///
    /// Every failure here is silent-by-design at `debug`: a guest with no `qemu-guest-agent`
    /// installed is the normal case on Debian, and it already said so once when the port
    /// went quiet.
    fn qga_sync_clock(&self) {
        let Some(qga) = self.qga.lock().unwrap().as_ref().map(|s| s.client.clone()) else {
            return;
        };
        let sample = match qga.time_sample() {
            Ok(s) => s,
            Err(e) => {
                log::debug!("qga: no guest clock reading ({e:#})");
                return;
            }
        };
        match crate::qga::policy::decide(&sample) {
            crate::qga::policy::Action::Nothing => {}
            crate::qga::policy::Action::Resample => {
                log::debug!(
                    "qga: clock sample too noisy to act on (rtt {:?}); waiting for the next tick",
                    sample.rtt
                );
            }
            crate::qga::policy::Action::Step { delta_ns } => {
                let secs = delta_ns as f64 / 1e9;
                let now = crate::qga::client::unix_ns(std::time::SystemTime::now());
                match qga.set_time(now) {
                    Ok(()) => log::info!(
                        "qga: the guest clock was {:.1}s {}; stepped it to the host's",
                        secs.abs(),
                        if secs > 0.0 { "behind" } else { "ahead" }
                    ),
                    Err(e) => log::warn!("qga: stepping the guest clock failed: {e:#}"),
                }
            }
        }
    }
}

/// Accept connections forever, one serve thread per peer (agents are independent: the
/// root daemon and per-session helpers must be able to talk concurrently).
/// The connected agents. A type of its own so the functions below, which are everything that
/// reads or changes the registry, can be driven by `loom_model` over an in-memory write half.
type Peers<W = UnixStream> = PeerMutex<Vec<Arc<Peer<W>>>>;

/// Send `msg` to every peer with `cap`, returning the agents that took it and
/// dropping any whose send fails (dead or wedged connection).
///
/// Sends happen on a SNAPSHOT of the registry, never while holding the peers lock:
/// each peer's socket has a write timeout ([`write_timeout`]) rather than blocking
/// forever, but even a bounded stall must not serialize registration, the liveness
/// sweep, and the shutdown ladder behind one slow peer. (Snapshotting is what makes
/// this safe: a peer that deregisters concurrently just gets a failed send here.)
fn send_to_capable<W: Write>(
    peers: &Peers<W>,
    cap: &str,
    msg: &Message,
    channel: u32,
) -> Vec<String> {
    let targets: Vec<Arc<Peer<W>>> = peers
        .lock()
        .unwrap()
        .iter()
        .filter(|p| p.has_cap(cap))
        .cloned()
        .collect();
    let mut delivered = Vec::new();
    let mut failed = Vec::new();
    for peer in targets {
        match peer.send(msg, channel) {
            Ok(()) => delivered.push(peer.agent.clone()),
            Err(e) => {
                log::warn!("control: send to {} failed ({e}); dropping it", peer.agent);
                failed.push(peer.id);
            }
        }
    }
    if !failed.is_empty() {
        peers.lock().unwrap().retain(|p| !failed.contains(&p.id));
    }
    delivered
}

/// The poller saw a host copy: offer it to every clipboard-capable peer.
fn offer_host_copy<W: Write, S: PasteboardServer>(
    peers: &Peers<W>,
    clipboard: &Clipboard<S>,
    text: String,
) {
    let offer = clipboard.make_offer(text);
    send_to_capable(peers, "clipboard", &offer, CHANNEL_CLIPBOARD);
}

/// Bring a handshaken peer into the registry, and greet it with what a late joiner needs: the
/// host's current clipboard, and the host clock (`time_sync`, for a `timesync` peer).
///
/// Registered FIRST, then greeted. The greeting reads the clipboard as it is at that moment, so
/// a host copy offered between the read and the registration reached every peer but this one,
/// and the poller never offers the same copy twice: that guest kept the older clipboard until
/// the next host copy (found by `loom_model`). Registered first, a copy offered at any point is
/// either broadcast to it or already on the pasteboard the greeting reads. A broadcast that
/// overtakes the greeting on the wire is harmless: the guest ignores an offer older than the
/// newest it has seen.
fn admit<W: Write, S: PasteboardServer>(
    peers: &Peers<W>,
    peer: Arc<Peer<W>>,
    clipboard: &Clipboard<S>,
    time_sync: impl FnOnce() -> Message,
) {
    peers.lock().unwrap().push(peer.clone());
    // A late joiner needs the CURRENT host clipboard, not just the next change.
    if peer.has_cap("clipboard")
        && let Some(offer) = clipboard.initial_offer()
    {
        let _ = peer.send(&offer, CHANNEL_CLIPBOARD);
    }
    // Seed the guest clock immediately: the agent (re)connects at boot AND right after a
    // snapshot restore — exactly the moments the guest's CNTVCT-anchored clock is stale.
    if peer.has_cap("timesync") {
        let _ = peer.send(&time_sync(), CHANNEL_CONTROL);
    }
}

fn accept_loop(listener: UnixListener, inner: Arc<Inner>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                set_nosigpipe(&stream);
                // Bound every write to this peer (the option rides the socket, so the
                // serve thread's `try_clone` write half inherits it): a peer that stops
                // draining must produce a send ERROR to be dropped over, not a hang.
                if let Err(e) = stream.set_write_timeout(write_timeout()) {
                    log::warn!("control: set_write_timeout failed ({e}); sends may block");
                }
                let peer_inner = inner.clone();
                let spawned = std::thread::Builder::new()
                    .name("limina-control-peer".into())
                    .spawn(move || {
                        if let Err(e) = serve_agent(stream, &peer_inner) {
                            log::warn!("control: agent connection ended with error: {e}");
                        }
                    });
                if let Err(e) = spawned {
                    log::warn!("control: cannot spawn peer thread: {e}");
                }
            }
            Err(e) if accept_error_is_transient(&e) => {
                log::warn!("control: accept failed ({e}); retrying");
                std::thread::sleep(ACCEPT_RETRY);
            }
            Err(e) => {
                // Listener broken (e.g. socket unlinked early) — nothing left to serve.
                log::warn!("control: accept failed: {e}");
                return;
            }
        }
    }
}

/// Pause after a transient accept failure, so a full descriptor table is waited out
/// rather than spun on.
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

/// Whether an accept failure leaves the listener usable: the process or system ran out of
/// descriptors or memory for the new socket, or one connection went away mid-accept.
fn accept_error_is_transient(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(
            libc::EMFILE
                | libc::ENFILE
                | libc::ENOBUFS
                | libc::ENOMEM
                | libc::ECONNABORTED
                | libc::EINTR
        )
    )
}

/// One agent session: HELLO → WELCOME, register, then serve until EOF. Heartbeats keep
/// liveness; unknown types get ERROR(UNSUPPORTED) — never fatal, per the protocol's
/// ground rule. The peer is deregistered on any exit.
fn serve_agent(mut stream: UnixStream, inner: &Inner) -> std::io::Result<()> {
    let (_, first) = read_message(&mut stream)?;
    let hello = match first {
        Message::Hello(h) => h,
        other => {
            log::warn!("control: peer's first message was not HELLO ({other:?}); dropping");
            return Ok(());
        }
    };
    log::info!(
        "control: guest agent connected: {} caps={:?} pagesize={}",
        hello.agent,
        hello.caps,
        hello.pagesize
    );
    write_message(
        &mut stream,
        CHANNEL_CONTROL,
        &Message::Welcome(Welcome {
            caps: welcome_caps(
                inner.fido_store.is_some(),
                inner.usb_fido_gadget,
                inner.vcpu_policy.is_some(),
            ),
        }),
    )?;

    let id = inner.next_id.fetch_add(1, Ordering::Relaxed);
    let writer = Arc::new(PeerMutex::new(stream.try_clone()?));
    let last_seen = Arc::new(Mutex::new(Instant::now()));
    let peer = Arc::new(Peer {
        id,
        agent: hello.agent.clone(),
        caps: hello.caps,
        stream: writer.clone(),
        last_seen: last_seen.clone(),
        silent: Arc::new(AtomicBool::new(false)),
    });
    admit(&inner.peers, peer, &inner.clipboard, time_sync_now);

    // Per-peer CTAPHID state: the agent creates one uhid FIDO device per connection,
    // so channel ids and reassembly are connection-scoped by construction; the passkey
    // store is shared VM-wide. Absent a Secure Enclave there is no store and no
    // authenticator — the guest was never told `fido`, so no CBOR frames arrive.
    let mut fido = inner
        .fido_store
        .clone()
        .map(crate::fido::FidoAuthenticator::new);
    let result = serve_loop(&mut stream, &writer, inner, &last_seen, &mut fido);
    inner.peers.lock().unwrap().retain(|p| p.id != id);
    log::info!("control: guest agent disconnected: {}", hello.agent);
    result
}

/// The post-handshake read loop for one peer. Replies go through the peer's `writer`
/// mutex — never the raw read stream — because broadcasters write concurrently.
fn serve_loop(
    stream: &mut UnixStream,
    writer: &PeerMutex<UnixStream>,
    inner: &Inner,
    last_seen: &Mutex<Instant>,
    fido: &mut Option<crate::fido::FidoAuthenticator>,
) -> std::io::Result<()> {
    let reply = |msg: &Message, channel: u32| -> std::io::Result<()> {
        write_message(&mut *writer.lock().unwrap(), channel, msg)
    };
    // This peer's guest→host offer high-water mark (see the ClipOffer arm below).
    let mut guest_serial: u64 = 0;
    loop {
        let msg = read_message(stream);
        if msg.is_ok() {
            // ANY inbound message proves the peer alive, not just heartbeats.
            *last_seen.lock().unwrap() = Instant::now();
        }
        match msg {
            Ok((_, Message::Heartbeat(_))) => {} // liveness only; tracked above
            Ok((_, Message::ShutdownAck)) => {
                log::info!("control: agent acknowledged shutdown");
            }
            // The clipboard conversation (see crate::clipboard for the protocol rules).
            // `guest_serial` is deliberately a local: each guest session's helper numbers
            // its offers from 1, so the ratchet must be per-connection. Sharing one across
            // peers let the session with the highest count silence all the others.
            Ok((_, Message::ClipOffer(o))) => {
                if let Some(msg) = inner.clipboard.on_offer(o, &mut guest_serial) {
                    reply(&msg, CHANNEL_CLIPBOARD)?;
                }
            }
            Ok((_, Message::ClipRequest(r))) => {
                if let Some(msg) = inner.clipboard.on_request(r) {
                    reply(&msg, CHANNEL_CLIPBOARD)?;
                }
            }
            Ok((_, Message::ClipData(d))) => inner.clipboard.on_data(d, guest_serial),
            // M14: one CTAP HID report from the guest's uhid FIDO device. Only reachable
            // when a Secure Enclave is present (the guest was told `fido`), so `fido` is
            // Some. Transport frames (INIT/PING/errors) answer immediately; a CTAP2 CBOR
            // command may block on a host Touch ID prompt, so it runs on a worker thread
            // while we pump CTAPHID_KEEPALIVE (else libfido2/browsers time out FIDO_ERR_RX).
            Ok((_, Message::FidoReport(r))) => {
                if let Some(fido) = fido.as_mut() {
                    let send = |f: [u8; crate::fido::REPORT_SIZE]| -> std::io::Result<()> {
                        reply(
                            &Message::FidoReport(limina_proto::FidoReport { data: f.to_vec() }),
                            limina_proto::CHANNEL_FIDO,
                        )
                    };
                    // The shared keepalive engine (identical to the USB gadget transport).
                    crate::fido::pump(fido, &r.data, send)?;
                }
            }
            // M6: feed guest memory-pressure reports to the autoballoon policy (if configured).
            Ok((_, Message::MemPressure(p))) => {
                // Also the only view the host gets of how busy the guest's disk is, which is
                // what gates the periodic trim (`crate::qga::trim`). Kept even when no balloon
                // policy is configured — the two consumers are independent.
                *inner.guest_io_full_avg10.lock().unwrap() =
                    Some((Instant::now(), p.io_full_avg10));
                if let Some(policy) = &inner.balloon_policy {
                    policy.on_pressure(&p);
                }
            }
            // The CPU sibling of the report above: how many tasks the guest has runnable and how
            // many vCPUs it actually has online. The policy answers with a count to aim for; the
            // guest is the only side that can act on it, and it never acks — its next report IS
            // the ack, which is why a dropped or refused target simply retries.
            Ok((_, Message::CpuPressure(p))) => {
                let now = Instant::now();
                *inner.guest_vcpus_online.lock().unwrap() = Some((now, p.online));
                *inner.agent_cpu_report_at.lock().unwrap() = Some(now);
                if let Some(policy) = &inner.vcpu_policy
                    && let Some(target) = policy.on_pressure(&p)
                {
                    reply(&Message::CpuTarget(target), CHANNEL_CONTROL)?;
                    *inner.last_cpu_target.lock().unwrap() = Some(target.online);
                }
            }
            // The guest desktop's power profile. The guest needs no limina components to have
            // one — on a stock Fedora guest `tuned-ppd` owns `net.hadess.PowerProfiles` — so the
            // agent only reports which is active and the host decides what it means. Nothing is
            // ever sent back: this is the user's choice, not a negotiation.
            Ok((_, Message::PowerProfile(p))) => {
                let profile = crate::power_profile::PowerProfile::from_wire(p.profile);
                if let Some(target) = inner.apply_power_profile(profile) {
                    reply(&Message::CpuTarget(target), CHANNEL_CONTROL)?;
                    *inner.last_cpu_target.lock().unwrap() = Some(target.online);
                }
            }
            // M15: the guest's own monitor arrangement, which the host cannot infer — an
            // absolute pointer is spread across the guest's whole desktop, so mapping into it
            // needs to know what order the compositor put the monitors in.
            //
            // Deliberately last-writer-wins with no host-side ownership rule: arbitration is
            // the GUEST's (limina-agent-session's `layout_gate` — only the helper whose uid
            // owns the seat's ACTIVE logind session reports, and it re-sends on activation).
            // The host cannot rank channels itself (it has no view of guest session
            // activity), and a pre-gate or stock helper writing unconditionally is the
            // documented degraded floor, not a state to defend against here.
            Ok((_, Message::DisplayLayout(l))) => {
                crate::window::arrangement::publish_guest_layout(&l.monitors);
            }
            Ok((_, Message::Error(e))) => {
                log::warn!("control: agent reported error: {e:?}");
            }
            Ok((_, Message::Unknown { msg_type, .. })) => {
                reply(&Message::unsupported(msg_type), CHANNEL_CONTROL)?;
            }
            // HELLO twice / host-only messages from a guest: ignore rather than die.
            Ok((_, Message::CpuTarget(_)))
            | Ok((_, Message::Hello(_)))
            | Ok((_, Message::Welcome(_)))
            | Ok((_, Message::Shutdown(_)))
            | Ok((_, Message::TimeSync(_))) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}

/// What WELCOME advertises to a freshly connected agent.
///
/// `fido` is the interesting one, and it carries a **policy**, not just a capability: it is what
/// makes the agent create its uhid authenticator (`guest/limina-agent`), so advertising it
/// alongside the USB gadget gives the guest **two** FIDO devices with the same VID:PID and one
/// shared passkey store. Browsers dispatch a ceremony to every attached HID authenticator in
/// parallel, so both signed — a Touch ID sheet each — and on registration both *minted* a
/// credential, leaving orphans the site never saw and one RP's sign counter split across two
/// credentials (which reads as a cloned authenticator to any RP that enforces monotonicity).
///
/// So: exactly one transport per guest. USB wins where it exists, because it is the tier every
/// guest has (no agent required); uhid stays for `--no-usb` runs, where there is no controller to
/// hang a gadget on and it is the only way to keep passkeys — which is precisely why `--no-fido`
/// is a separate flag from `--no-usb` (see `Cli::fido_enabled`). Withholding the cap is also the
/// whole fix: agents already gate the device on it, so no guest-side change is needed.
fn welcome_caps(has_fido_store: bool, usb_fido_gadget: bool, dynamic_vcpus: bool) -> Vec<String> {
    let mut caps = vec!["shutdown".to_string(), "clipboard".to_string()];
    if has_fido_store && !usb_fido_gadget {
        caps.push("fido".to_string());
    }
    // The guest samples and reports its runnable-task load only while this is offered, so a VM
    // with no dynamic range costs exactly what it costs today — no extra frame per second, and
    // no chance of a CPU going offline on a host that never asked.
    if dynamic_vcpus {
        caps.push("vcpu".to_string());
    }
    caps
}

/// How recently the enhanced agent must have reported for the QGA poller to stand down. Agents
/// report on their ~1s heartbeat tick, so this is several missed beats.
const AGENT_CPU_FRESH: Duration = Duration::from_secs(5);

/// Consecutive QGA vCPU failures before we stop asking this guest. Fedora's `virt_qemu_ga_t`
/// domain is the real gate on what the stock agent may do, and a domain denial is not something
/// retrying fixes.
const QGA_VCPU_GIVE_UP: u64 = 3;

/// The host's authoritative wallclock as a [`Message::TimeSync`] frame.
fn time_sync_now() -> Message {
    let unix_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    Message::TimeSync(limina_proto::TimeSync { unix_ns })
}

/// Writing to a dead peer must fail with EPIPE, not raise SIGPIPE and kill the
/// supervisor (macOS has no MSG_NOSIGNAL).
fn set_nosigpipe(stream: &UnixStream) {
    use std::os::fd::AsRawFd;
    let on: libc::c_int = 1;
    unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            &on as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use limina_proto::Hello;

    /// A full descriptor table is a moment, not the end of the listener: an accept that
    /// fails on it must be retried, or the control plane stops for the VM's whole life.
    #[test]
    fn accept_retries_exhaustion_and_stops_on_a_dead_listener() {
        use std::io::Error;
        for errno in [
            libc::EMFILE,
            libc::ENFILE,
            libc::ENOBUFS,
            libc::ENOMEM,
            libc::ECONNABORTED,
            libc::EINTR,
        ] {
            assert!(
                accept_error_is_transient(&Error::from_raw_os_error(errno)),
                "errno {errno} should be retried"
            );
        }
        for errno in [libc::EBADF, libc::EINVAL, libc::ENOTSOCK] {
            assert!(
                !accept_error_is_transient(&Error::from_raw_os_error(errno)),
                "errno {errno} should stop the loop"
            );
        }
    }

    /// Connect a fake agent to the plane's socket, handshake, and return the stream.
    fn connect_agent(path: &Path, caps: &[&str]) -> UnixStream {
        let mut s = UnixStream::connect(path).unwrap();
        write_message(
            &mut s,
            CHANNEL_CONTROL,
            &Message::Hello(Hello {
                agent: "test-agent/0".into(),
                caps: caps.iter().map(|c| c.to_string()).collect(),
                pagesize: 4096,
            }),
        )
        .unwrap();
        let (_, welcome) = read_message(&mut s).unwrap();
        assert!(matches!(welcome, Message::Welcome(_)));
        s
    }

    /// One guest, one FIDO authenticator. The agent creates its uhid device iff WELCOME says
    /// `fido`, so serving the USB gadget must withhold the cap — otherwise the guest carries two
    /// authenticators (measured: `/dev/hidraw0` USB + `/dev/hidraw1` uhid, both `ID_FIDO_TOKEN=1`,
    /// same 1D6B:0F1D), browsers dispatch to both, and one registration mints two credentials.
    #[test]
    fn the_usb_gadget_and_the_agent_never_both_serve_fido() {
        assert!(
            !welcome_caps(true, true, false).iter().any(|c| c == "fido"),
            "the gadget is serving; the agent must not raise a second authenticator"
        );
        // No gadget (--no-usb): uhid is the only way to keep passkeys, so it is offered.
        assert!(welcome_caps(true, false, false).iter().any(|c| c == "fido"));
        // No store (no Secure Enclave, or --no-fido): no authenticator either way.
        assert!(
            !welcome_caps(false, false, false)
                .iter()
                .any(|c| c == "fido")
        );
        assert!(!welcome_caps(false, true, false).iter().any(|c| c == "fido"));
        // The rest of the handshake is unaffected.
        assert!(
            welcome_caps(true, true, false)
                .iter()
                .any(|c| c == "clipboard")
        );
        assert!(
            welcome_caps(true, true, false)
                .iter()
                .any(|c| c == "shutdown")
        );
    }

    /// The `vcpu` capability is the whole opt-in: a guest is only asked to sample and report its
    /// runnable-task load while the host actually runs a policy. A VM with no dynamic range must
    /// cost exactly what it costs today — no extra frame per second, and no way for a CPU to go
    /// offline on a host that never asked for it.
    #[test]
    fn the_vcpu_capability_is_only_offered_when_a_policy_exists() {
        assert!(
            !welcome_caps(false, false, false)
                .iter()
                .any(|c| c == "vcpu")
        );
        assert!(welcome_caps(false, false, true).iter().any(|c| c == "vcpu"));
    }

    /// End-to-end over the socket: selecting `power-saver` in the guest desktop tightens the
    /// host's reclaim floor, and going back to `balanced` both restores it and grows the machine
    /// at once rather than waiting for the next pressure report.
    ///
    /// A VM started with reclaim *off* is used deliberately — that is the default, and the whole
    /// reason a policy is now constructed even for `Disabled`: the `vcpu` capability is
    /// negotiated once at WELCOME, so a VM that could not reclaim at boot must still be able to
    /// when its user asks.
    #[test]
    fn the_guests_power_profile_moves_the_reclaim_floor() {
        let path = std::env::temp_dir().join(format!(
            "limina-ctl-power-{}-{:?}.sock",
            std::process::id(),
            std::thread::current().id()
        ));
        let policy =
            crate::vcpu_policy::VcpuPolicy::new(10, crate::vcpu_policy::CpuReclaim::Disabled);
        let plane = ControlPlane::start(&path, None, policy, None, false).unwrap();
        let mut agent = UnixStream::connect(&path).unwrap();
        write_message(
            &mut agent,
            CHANNEL_CONTROL,
            &Message::Hello(Hello {
                agent: "test-agent/0".into(),
                caps: vec!["vcpu".into()],
                pagesize: 16384,
            }),
        )
        .unwrap();
        let (_, _welcome) = read_message(&mut agent).unwrap();

        let floor = || plane.inner.vcpu_policy.as_ref().unwrap().floor();
        assert_eq!(floor(), 10, "a disabled policy floors at the whole machine");

        // power-saver tightens, and asks for no grow — nothing has been given up yet.
        write_message(
            &mut agent,
            CHANNEL_CONTROL,
            &Message::PowerProfile(limina_proto::PowerProfileMsg { profile: 0 }),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while floor() == 10 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(floor(), 2, "power-saver should squeeze toward two");

        // ...and going back restores the whole machine, with a CpuTarget on the way out so the
        // guest re-onlines immediately.
        write_message(
            &mut agent,
            CHANNEL_CONTROL,
            &Message::PowerProfile(limina_proto::PowerProfileMsg { profile: 1 }),
        )
        .unwrap();
        match read_message(&mut agent).unwrap() {
            (_, Message::CpuTarget(t)) => assert_eq!(t.online, 10),
            other => panic!("expected a CpuTarget grow, got {other:?}"),
        }
        assert_eq!(floor(), 10);
    }

    /// End-to-end over the socket: a guest short of CPUs gets a `CpuTarget` back on the same
    /// connection. Uses the GROW path deliberately — it is the one with no dwell, so the test
    /// needs no clock control, and it is also the path whose latency a user would feel.
    #[test]
    fn a_guest_short_of_cpus_is_answered_with_a_target() {
        let path = std::env::temp_dir().join(format!(
            "limina-ctl-vcpu-{}-{:?}.sock",
            std::process::id(),
            std::thread::current().id()
        ));
        let policy =
            crate::vcpu_policy::VcpuPolicy::new(10, crate::vcpu_policy::CpuReclaim::Moderate);
        let _plane = ControlPlane::start(&path, None, policy, None, false).unwrap();
        let mut agent = UnixStream::connect(&path).unwrap();
        write_message(
            &mut agent,
            CHANNEL_CONTROL,
            &Message::Hello(Hello {
                agent: "test-agent/0".into(),
                caps: vec!["vcpu".into()],
                pagesize: 16384,
            }),
        )
        .unwrap();
        let (_, welcome) = read_message(&mut agent).unwrap();
        match welcome {
            Message::Welcome(w) => assert!(w.caps.iter().any(|c| c == "vcpu")),
            other => panic!("expected WELCOME, got {other:?}"),
        }

        // Two CPUs online, eight tasks wanting to run.
        write_message(
            &mut agent,
            CHANNEL_CONTROL,
            &Message::CpuPressure(limina_proto::CpuPressure {
                nr_running: 8,
                loadavg1_x100: 800,
                online: 2,
                present: 10,
                ..Default::default()
            }),
        )
        .unwrap();
        agent
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (_, msg) = read_message(&mut agent).unwrap();
        assert_eq!(
            msg,
            Message::CpuTarget(limina_proto::CpuTarget { online: 10 }),
            "a guest with more runnable tasks than CPUs must get the whole machine back"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A guest that already has every vCPU online, and that the host has never asked to shrink,
    /// has nothing to confirm: the bracket must not sit out the agent's report tick waiting for
    /// a fresh "all online" (that wait was ~1 s of every suspend of a busy guest).
    #[test]
    fn a_guest_at_full_strength_does_not_hold_up_the_snapshot() {
        let path = std::env::temp_dir().join(format!(
            "limina-ctl-fullvcpu-{}-{:?}.sock",
            std::process::id(),
            std::thread::current().id()
        ));
        let policy =
            crate::vcpu_policy::VcpuPolicy::new(10, crate::vcpu_policy::CpuReclaim::Disabled);
        let plane = ControlPlane::start(&path, None, policy, None, false).unwrap();
        let mut agent = vcpu_agent(&path);
        report_online(&plane, &mut agent, 10);

        let started = Instant::now();
        assert!(plane.restore_all_vcpus(Duration::from_secs(3)));
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "a guest with nothing offline must not wait for a report ({:?})",
            started.elapsed()
        );
        let _ = std::fs::remove_file(&path);
    }

    /// ...but a shrink the host sent may not have been applied yet — the guest acts on a target
    /// AFTER the report it answered, so an "all online" report can predate it. Then the bracket
    /// must still wait for a report made after its own ask.
    #[test]
    fn an_outstanding_shrink_still_waits_for_the_guest() {
        let path = std::env::temp_dir().join(format!(
            "limina-ctl-shrinkvcpu-{}-{:?}.sock",
            std::process::id(),
            std::thread::current().id()
        ));
        let policy =
            crate::vcpu_policy::VcpuPolicy::new(10, crate::vcpu_policy::CpuReclaim::Disabled);
        let plane = ControlPlane::start(&path, None, policy, None, false).unwrap();
        let mut agent = vcpu_agent(&path);
        report_online(&plane, &mut agent, 10);
        *plane.inner.last_cpu_target.lock().unwrap() = Some(4);

        let started = Instant::now();
        assert!(!plane.restore_all_vcpus(Duration::from_millis(400)));
        assert!(started.elapsed() >= Duration::from_millis(400));
        let _ = std::fs::remove_file(&path);
    }

    /// Connect a fake enhanced agent that negotiates the `vcpu` capability.
    fn vcpu_agent(path: &std::path::Path) -> UnixStream {
        let mut agent = UnixStream::connect(path).unwrap();
        write_message(
            &mut agent,
            CHANNEL_CONTROL,
            &Message::Hello(Hello {
                agent: "test-agent/0".into(),
                caps: vec!["vcpu".into()],
                pagesize: 16384,
            }),
        )
        .unwrap();
        let (_, _welcome) = read_message(&mut agent).unwrap();
        agent
    }

    /// Send an idle `CpuPressure` with `online` vCPUs and wait until the plane has recorded it.
    fn report_online(plane: &ControlPlane, agent: &mut UnixStream, online: u32) {
        write_message(
            agent,
            CHANNEL_CONTROL,
            &Message::CpuPressure(limina_proto::CpuPressure {
                online,
                present: online,
                ..Default::default()
            }),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while plane.inner.guest_vcpus_online.lock().unwrap().is_none() {
            assert!(Instant::now() < deadline, "the report never landed");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The #41 mitigation must never be able to hold up a snapshot. With no policy (or no guest)
    /// there is nothing of ours offline, so the bracket proceeds immediately.
    #[test]
    fn restoring_vcpus_before_a_snapshot_is_a_no_op_without_a_policy() {
        let path = std::env::temp_dir().join(format!(
            "limina-ctl-novcpu-{}-{:?}.sock",
            std::process::id(),
            std::thread::current().id()
        ));
        let plane = ControlPlane::start(&path, None, None, None, false).unwrap();
        let started = Instant::now();
        assert!(plane.restore_all_vcpus(Duration::from_secs(30)));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "no policy must not cost the suspend bracket any wait at all"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A shutdown-capable agent that never drains its socket must not stall
    /// `request_shutdown`: `Peer::send` used to be a blocking write with no timeout,
    /// made while holding the peers lock — one wedged agent froze the Ctrl-C shutdown
    /// ladder, new-peer registration, and the liveness sweep. The plane must time the
    /// write out and drop the peer instead.
    #[test]
    fn wedged_peer_cannot_stall_request_shutdown() {
        unsafe { std::env::set_var("LIMINA_CONTROL_WRITE_TIMEOUT_MS", "200") };
        let path =
            std::env::temp_dir().join(format!("limina-ctl-test-{}.sock", std::process::id()));
        let plane = ControlPlane::start(&path, None, None, None, false).unwrap();
        // HELLO, then never read again: the socket buffers fill and stay full.
        let wedged = connect_agent(&path, &["shutdown"]);

        // Hammer SHUTDOWN until the buffers fill. Run in a thread so a regression
        // (send blocking forever) fails the test instead of hanging it. Each frame is
        // ~30 bytes; 8192 sends is comfortably past any default unix-socket buffering.
        let (tx, rx) = std::sync::mpsc::channel();
        let hammer = plane.clone();
        std::thread::spawn(move || {
            for _ in 0..8192 {
                if !hammer.request_shutdown(Duration::from_secs(0)) {
                    break; // peer dropped after the write timed out
                }
            }
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(20))
            .expect("request_shutdown wedged on a non-draining peer");

        // The wedged peer must have been dropped from the registry.
        assert!(!plane.request_shutdown(Duration::from_secs(0)));
        drop(wedged);
        let _ = std::fs::remove_file(&path);
    }
}

/// loom models of an agent joining while the host clipboard changes: the poller offering a
/// host copy races [`admit`] greeting a new peer with the current clipboard.
///
/// Run with `cargo xtask check loom crates/limina`. The registry, each peer's write half, the
/// clipboard's locks and its serial are loom's, and so is the lock of the stand-in pasteboard
/// server, which another app writes to as AppKit does: `clearContents` (which bumps the change
/// count) and then `setString` (which does not).
///
/// After each run the poller is ticked until it has nothing left to offer, as it would be within
/// half a second, and then every peer is asked what its guest ends up with. A guest keeps the
/// newest host offer it has seen (`limina-agent-session` ignores an older serial) and requests
/// its data; the host must serve that request, with what is on the pasteboard now. The request
/// is made last, which is the latest the guest can be: a request is answered from the host's
/// state when it arrives, and nothing bounds how long it takes to get there.
#[cfg(all(test, loom))]
mod loom_model {
    use std::io::Cursor;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;

    use limina_proto::{ClipRequest, Message, read_message};
    use loom::sync::Mutex;
    use loom::thread;

    use super::{Peer, PeerMutex, Peers, admit, offer_host_copy, time_sync_now};
    use crate::clipboard::{Clipboard, PasteboardServer, TEXT_MIME};

    /// The pasteboard server: its change count and the text on it.
    #[derive(Clone)]
    struct Server(Arc<Mutex<(isize, Option<String>)>>);

    impl Server {
        fn holding(text: &str) -> Server {
            Server(Arc::new(Mutex::new((1, Some(text.into())))))
        }
        /// Another app copies `text`: two calls, and a poll can land between them.
        fn copy(&self, text: &str) {
            self.clear();
            self.0.lock().unwrap().1 = Some(text.into());
        }
        fn clear(&self) {
            let mut s = self.0.lock().unwrap();
            s.0 += 1;
            s.1 = None;
        }
    }

    impl PasteboardServer for Server {
        fn change_count(&self) -> isize {
            self.0.lock().unwrap().0
        }
        fn text(&self) -> Option<String> {
            self.0.lock().unwrap().1.clone()
        }
        fn replace(&self, text: &str) {
            self.copy(text);
        }
    }

    type Wire = Arc<PeerMutex<Vec<u8>>>;

    fn peer(id: u64) -> (Arc<Peer<Vec<u8>>>, Wire) {
        let wire: Wire = Arc::new(PeerMutex::new(Vec::new()));
        let peer = Arc::new(Peer {
            id,
            agent: format!("session-{id}"),
            caps: vec!["clipboard".into()],
            stream: wire.clone(),
            last_seen: Arc::new(std::sync::Mutex::new(Instant::now())),
            silent: Arc::new(AtomicBool::new(false)),
        });
        (peer, wire)
    }

    /// One poller tick, as `ControlPlane::start`'s poll thread runs it.
    fn poll(peers: &Peers<Vec<u8>>, clipboard: &Clipboard<Server>) -> bool {
        match clipboard.poll_local_change() {
            Some(text) => {
                offer_host_copy(peers, clipboard, text);
                true
            }
            None => false,
        }
    }

    /// Settle, then check what each guest ends up with.
    fn check(
        peers: &Peers<Vec<u8>>,
        clipboard: &Clipboard<Server>,
        server: &Server,
        wires: &[&Wire],
    ) {
        while poll(peers, clipboard) {}
        let want = server.text();
        assert_eq!(
            peers.lock().unwrap().len(),
            wires.len(),
            "every peer is registered"
        );
        for (i, wire) in wires.iter().enumerate() {
            let bytes = wire.lock().unwrap().clone();
            let mut cursor = Cursor::new(bytes.as_slice());
            let mut newest = None;
            while let Ok((_, msg)) = read_message(&mut cursor) {
                if let Message::ClipOffer(o) = msg {
                    newest = newest.max(Some(o.serial));
                }
            }
            let Some(serial) = newest else {
                panic!("peer {i} was never offered the host clipboard ({want:?})");
            };
            let answer = clipboard.on_request(ClipRequest {
                serial,
                mime_type: TEXT_MIME.into(),
            });
            let got = match answer {
                Some(Message::ClipData(d)) => Some(String::from_utf8(d.data).unwrap()),
                Some(other) => panic!("peer {i}'s request for {serial} was answered {other:?}"),
                None => None,
            };
            assert_eq!(
                got, want,
                "peer {i} holds offer {serial}; its guest must be served the host clipboard"
            );
        }
    }

    /// Someone copied on the host, and before the poller offers it an agent connects: the
    /// poller's tick races the new peer's greeting. An agent already connected holds the
    /// offer of what was there before.
    #[test]
    fn an_agent_joins_as_a_host_copy_is_offered() {
        loom::model(|| {
            let server = Server::holding("before");
            let clipboard = Arc::new(Clipboard::with_server(server.clone()));
            let peers: Arc<Peers<Vec<u8>>> = Arc::new(PeerMutex::new(Vec::new()));
            let (a, wire_a) = peer(1);
            admit(&peers, a, &clipboard, time_sync_now);
            server.copy("copied");

            let poller = {
                let (peers, clipboard) = (peers.clone(), clipboard.clone());
                thread::spawn(move || {
                    poll(&peers, &clipboard);
                })
            };
            let (b, wire_b) = peer(2);
            admit(&peers, b, &clipboard, time_sync_now);
            poller.join().unwrap();
            check(&peers, &clipboard, &server, &[&wire_a, &wire_b]);
        });
    }

    /// All three at once: another app copies while the poller ticks and an agent connects. The
    /// poller can take the copy before the greeting reads the pasteboard and offer it after,
    /// or the other way round.
    #[test]
    fn an_agent_joins_as_a_host_copy_lands_and_is_offered() {
        loom::model(|| {
            let server = Server::holding("before");
            let clipboard = Arc::new(Clipboard::with_server(server.clone()));
            let peers: Arc<Peers<Vec<u8>>> = Arc::new(PeerMutex::new(Vec::new()));
            let (a, wire_a) = peer(1);
            admit(&peers, a, &clipboard, time_sync_now);

            let copier = {
                let server = server.clone();
                thread::spawn(move || server.copy("copied"))
            };
            let poller = {
                let (peers, clipboard) = (peers.clone(), clipboard.clone());
                thread::spawn(move || {
                    poll(&peers, &clipboard);
                })
            };
            let (b, wire_b) = peer(2);
            admit(&peers, b, &clipboard, time_sync_now);
            copier.join().unwrap();
            poller.join().unwrap();
            check(&peers, &clipboard, &server, &[&wire_a, &wire_b]);
        });
    }

    /// An agent connects while another app is copying: its greeting reads a pasteboard that is
    /// changing under it.
    #[test]
    fn an_agent_joins_during_a_host_copy() {
        loom::model(|| {
            let server = Server::holding("before");
            let clipboard = Arc::new(Clipboard::with_server(server.clone()));
            let peers: Arc<Peers<Vec<u8>>> = Arc::new(PeerMutex::new(Vec::new()));
            let (a, wire_a) = peer(1);
            admit(&peers, a, &clipboard, time_sync_now);

            let copier = {
                let server = server.clone();
                thread::spawn(move || server.copy("copied"))
            };
            let (b, wire_b) = peer(2);
            admit(&peers, b, &clipboard, time_sync_now);
            copier.join().unwrap();
            check(&peers, &clipboard, &server, &[&wire_a, &wire_b]);
        });
    }
}
