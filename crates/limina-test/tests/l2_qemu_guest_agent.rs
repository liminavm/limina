// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 — **the stock `qemu-guest-agent`**, woken by its port and used to fix a guest clock.
//!
//! # Why this transport exists
//!
//! Fedora's comps make `qemu-guest-agent` **mandatory** in every desktop variant, and
//! `99-qemu-guest-agent.rules` starts it the moment a virtio-serial port named
//! `org.qemu.guest_agent.0` appears. So, exactly like the SPICE port next to it, the whole
//! feature lands on a guest with **nothing of ours installed** — the two-tier guarantee's
//! baseline. What it buys first is the guest clock: a guest that stays *running* across a
//! host nap, or that never re-reads its RTC, has no other corrector — the PL031 injection
//! only reaches a kernel that consults it (s2idle thaw), and our own `TimeSync` needs
//! `limina-agent`.
//!
//! # What each oracle is actually for
//!
//! 0. **The package is installed.** If it is not, the subject of this test does not exist in
//!    the image — that is an image property, so it SKIPs rather than fails.
//! 1. **The port is on the bus** (`/dev/virtio-ports/org.qemu.guest_agent.0`). Fails first
//!    and loudest, because everything below is a slower way to discover the same thing.
//! 2. **udev started the daemon.** `qemu-guest-agent.service` active proves the *stock*
//!    trigger fired — not that we installed or enabled anything.
//! 3. **The host reached it.** The supervisor's probe line names the agent's version and how
//!    many commands it offers, i.e. our client parsed a real `guest-info`.
//! 4. **A skewed clock is corrected**, and 5. it still works **after the agent restarts**
//!    (the port-reopen path, which `systemctl restart` and every package update take).
//!
//! # The discriminator, and why it is load-bearing
//!
//! The suite boots the *enhanced* image, where `limina-agent` is installed and its
//! `TimeSync` already fixes a skewed clock — so a naive "the clock came back" assertion
//! would go green with the qga path entirely broken. Oracle 4 therefore **stops
//! `limina-agent` first** (no `timesync`-capable peer can take the message, which is exactly
//! the condition the fallback is gated on) and **stops `chronyd`** (so NTP cannot be the
//! corrector either). With both gone, only the guest agent can move that clock. Passing for
//! the right reason is the entire point of this file.
//!
//! # Traps this test is shaped around
//!
//! - The supervisor's clock ladder ticks on `LIMINA_TIMESYNC_SECS` (60 s in production);
//!   this shrinks it so the test does not spend minutes waiting for the drift watchdog.
//! - Stopping `limina-agent` is not enough on its own: the peer only disappears when its
//!   socket closes, so the assertions poll rather than check once.
//! - `date -s` needs root, and the guest's clock is compared against the *host's* — the two
//!   are the same wallclock by construction (PL031 is anchored to host `CLOCK_REALTIME`).

use limina_test::{Guest, GuestConfig};
use std::time::{Duration, SystemTime};

/// The udev-matched port name. Its presence in the guest is the whole stock-tier trigger.
const PORT: &str = "/dev/virtio-ports/org.qemu.guest_agent.0";

/// How far back to shove the guest's clock. Comfortably past the 1 s step threshold and far
/// past any plausible drift, so a correction cannot be confused with normal timekeeping.
const SKEW: Duration = Duration::from_secs(2 * 3600);

/// How long a correction gets. The supervisor ticks every `LIMINA_TIMESYNC_SECS`, and the
/// agent's probe/backoff can add one interval on top.
const CORRECT_WITHIN: Duration = Duration::from_secs(90);

/// Run a guest command, tolerating a non-zero exit and a transient ssh failure.
fn ssh_soft(guest: &Guest, cmd: &str) -> String {
    let wrapped = format!("{{ {cmd} ; }} 2>&1 || true");
    let mut last_err = String::new();
    for _ in 0..4 {
        match guest.ssh_exec(&wrapped) {
            Ok(out) => return out.trim().to_string(),
            Err(e) => last_err = e.to_string(),
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    panic!("ssh `{cmd}` kept failing: {last_err}");
}

/// The guest's `CLOCK_REALTIME` in seconds since the epoch.
fn guest_epoch(guest: &Guest) -> i64 {
    let out = ssh_soft(guest, "date +%s");
    out.lines()
        .last()
        .and_then(|l| l.trim().parse::<i64>().ok())
        .unwrap_or_else(|| panic!("the guest did not answer with an epoch: {out:?}"))
}

fn host_epoch() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("host clock before the epoch")
        .as_secs() as i64
}

/// Wait until the guest's clock agrees with the host's, or give up. Each sample is printed:
/// when this test fails, *when* the clock moved is the whole diagnosis.
fn wait_for_agreement(guest: &Guest, within: Duration) -> i64 {
    let deadline = std::time::Instant::now() + within;
    let mut last = i64::MAX;
    while std::time::Instant::now() < deadline {
        let (g, h) = (guest_epoch(guest), host_epoch());
        last = (g - h).abs();
        eprintln!("clock sample: guest {g} host {h} off {last}s");
        if last <= 2 {
            return last;
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    last
}

#[test]
fn a_stock_guest_agent_corrects_a_skewed_clock() {
    if !limina_test::require_hvf_or_skip("a_stock_guest_agent_corrects_a_skewed_clock") {
        return;
    }
    let cfg = match GuestConfig::seated_efi_fedora_from_env() {
        Ok(cfg) => cfg
            // The image boots to gdm; without a display device gdm dies and systemd
            // restarts it forever, which is noise this test does not need next to a
            // timing assertion.
            .with_coexist_display(1280, 800)
            .with_net()
            .with_supervisor_log()
            // The clock ladder's tick. Production is 60 s; the drift watchdog and the
            // oversleep detector both ride it.
            .with_env("LIMINA_TIMESYNC_SECS", "5")
            // Every request and reply into the supervisor log: when a clock assertion fails,
            // "did we even ask?" is the first question, and it should not need a re-run.
            .with_env("LIMINA_QGA_TRACE", "1"),
        Err(e) => {
            eprintln!("SKIPPED a_stock_guest_agent_corrects_a_skewed_clock: {e:#}");
            return;
        }
    };

    // The harness stops VMs on a 3 s grace, which is deliberately too short to hold the
    // guest-agent rung (`supervisor::qga_rung_due` skips it rather than ask a guest to power
    // off a moment before SIGKILLing it). Oracle 6 needs the production-shaped ladder.
    let cfg = GuestConfig {
        shutdown_grace: Duration::from_secs(30),
        ..cfg
    };

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");

    // --- Oracle 0: the agent is even installed ---
    let rpm = ssh_soft(&guest, "rpm -q qemu-guest-agent");
    if !rpm.contains("qemu-guest-agent-") {
        eprintln!(
            "SKIPPED a_stock_guest_agent_corrects_a_skewed_clock: this image has no \
             qemu-guest-agent ({rpm}). Fedora's comps make it mandatory in every desktop \
             variant, so an image without it is a property of the image, not a failure."
        );
        guest.shutdown(Duration::from_secs(60)).ok();
        return;
    }
    eprintln!("guest agent package: {rpm}");

    // --- Oracle 1: the port reached the guest ---
    let port = ssh_soft(&guest, &format!("ls -l {PORT}"));
    assert!(
        port.contains("org.qemu.guest_agent.0"),
        "the guest has no {PORT} — the named virtio-serial port never reached it, so nothing \
         downstream can work. Check that the supervisor passed --qga-fd and that the worker \
         attached it (crates/limina-vmm/src/krun/console.rs). Got: {port}"
    );

    // --- Oracle 2: udev started the stock daemon, with no help from us ---
    guest
        .ssh_poll(
            "systemctl is-active qemu-guest-agent >/dev/null",
            Duration::from_secs(60),
        )
        .unwrap_or_else(|e| {
            panic!(
                "qemu-guest-agent never became active ({e}); the port is there but the udev \
                 trigger did not fire. status: {}",
                ssh_soft(
                    &guest,
                    "systemctl status qemu-guest-agent --no-pager -l | tail -30"
                )
            )
        });

    // --- Oracle 3: our client spoke to it ---
    //
    // Force the first probe by making the ladder run: with limina-agent still up this only
    // proves the transport, which is what oracle 3 is for.
    ssh_soft(&guest, "sudo systemctl stop limina-agent");
    guest
        .wait_for_supervisor_log("qga: guest agent", Duration::from_secs(60))
        .unwrap_or_else(|e| {
            panic!(
                "the supervisor never reported a live guest agent ({e}); qga log lines: {}",
                supervisor_qga_lines(&guest)
            )
        });

    // --- Oracle 3b: the inventory the first probe gathers ---
    //
    // Log-only by design, so the log IS the surface: an incident report that cannot say what
    // the guest was is the thing this exists to prevent.
    //
    // Wait for the LAST of the probe's three lines before snapshotting. The oracle-3 wait above
    // matches the first one ("...answered on..."), and the identity and inventory lines follow it
    // as separate writes — a snapshot taken the instant the wait returns can land between them,
    // which is exactly how this failed once with all three lines present in the panic message.
    guest
        .wait_for_supervisor_log("qga: guest filesystems: ", Duration::from_secs(30))
        .unwrap_or_else(|e| {
            panic!(
                "no filesystem inventory — guest-get-fsinfo did not land ({e}); qga log lines: {}",
                supervisor_qga_lines(&guest)
            )
        });
    let log = guest.supervisor_log();
    assert!(
        log.contains("qga: guest is "),
        "the supervisor never logged what the guest is. qga log lines: {}",
        supervisor_qga_lines(&guest)
    );
    assert!(
        log.lines()
            .any(|l| l.contains("qga: guest is ") && l.contains("kernel ")),
        "the identity line carries no kernel — guest-get-osinfo did not land: {}",
        supervisor_qga_lines(&guest)
    );
    assert!(
        log.contains("qga: guest filesystems: "),
        "no filesystem inventory — guest-get-fsinfo did not land: {}",
        supervisor_qga_lines(&guest)
    );

    // --- Oracle 4: the discriminator — a skewed clock, with every other corrector stopped ---
    ssh_soft(&guest, "sudo systemctl stop chronyd");
    // limina-agent is already stopped above; prove it, because a live one would correct the
    // clock over the control plane and this test would pass for the wrong reason.
    let agent = ssh_soft(&guest, "systemctl is-active limina-agent");
    assert!(
        !agent.contains("\nactive") && agent.trim() != "active",
        "limina-agent is still {agent} — its TimeSync would fix the clock over the control \
         plane, so a green here would say nothing about the guest agent"
    );

    let before = guest_epoch(&guest);
    ssh_soft(
        &guest,
        &format!("sudo date -s '@{}'", before - SKEW.as_secs() as i64),
    );
    let skewed = (guest_epoch(&guest) - host_epoch()).abs();
    assert!(
        skewed > 3600,
        "the guest clock did not actually move (off by {skewed}s); `date -s` failed, so the \
         rest of this test would assert nothing"
    );

    // Wait on the SUPERVISOR's own account of what it did, not on the clock: the guest's
    // clock is already right the instant `guest-set-time` returns, so a test that polls the
    // clock first can see agreement a beat before the line explaining it lands in the log —
    // which reads exactly like "something else corrected it" (it did, twice, on 2026-08-26).
    guest
        .wait_for_supervisor_log("stepped it to the host's", CORRECT_WITHIN)
        .unwrap_or_else(|e| {
            eprintln!(
                "guest-side time state: {}",
                ssh_soft(
                    &guest,
                    "systemctl is-active limina-agent chronyd systemd-timesyncd; timedatectl",
                )
            );
            panic!(
                "the supervisor never stepped the guest clock ({e}); with limina-agent and \
                 chronyd both stopped, the guest agent is the only corrector left. \
                 qga log lines: {}",
                supervisor_qga_lines(&guest)
            )
        });

    let off = wait_for_agreement(&guest, Duration::from_secs(20));
    assert!(
        off <= 2,
        "the supervisor says it stepped the clock, but the guest is still {off}s off — the \
         `guest-set-time` did not take. qga log lines: {}",
        supervisor_qga_lines(&guest)
    );

    // --- Oracle 5: the agent survives a restart (the port-reopen path) ---
    ssh_soft(&guest, "sudo systemctl restart qemu-guest-agent");
    guest
        .ssh_poll(
            "systemctl is-active qemu-guest-agent >/dev/null",
            Duration::from_secs(60),
        )
        .expect("qemu-guest-agent did not come back after a restart");
    let before = guest_epoch(&guest);
    ssh_soft(
        &guest,
        &format!("sudo date -s '@{}'", before - SKEW.as_secs() as i64),
    );
    let steps_before = step_lines(&guest);
    let deadline = std::time::Instant::now() + CORRECT_WITHIN;
    while step_lines(&guest) == steps_before && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(2));
    }
    assert!(
        step_lines(&guest) > steps_before,
        "after `systemctl restart qemu-guest-agent` the supervisor never stepped the clock \
         again — the client did not resynchronize across the port reopen. qga log lines: {}",
        supervisor_qga_lines(&guest)
    );
    let off = wait_for_agreement(&guest, Duration::from_secs(20));
    assert!(
        off <= 2,
        "the clock was stepped after the agent restart but the guest is still {off}s off"
    );

    // --- Oracle 6: the stop ladder's guest-agent rung ---
    //
    // With `limina-agent` stopped there is no orderly control-plane power-off, and telling
    // logind to ignore the power key takes the *next* rung away too — which is exactly the
    // seated-desktop case this rung exists for (a guest that will not answer the button).
    // What is left is `guest-shutdown` from the stock agent, and the VM must end by powering
    // itself off rather than by the SIGKILL at the end of the grace.
    ssh_soft(
        &guest,
        "sudo mkdir -p /etc/systemd/logind.conf.d && \
         printf '[Login]\\nHandlePowerKey=ignore\\nHandlePowerKeyLongPress=ignore\\n' | \
         sudo tee /etc/systemd/logind.conf.d/99-limina-test.conf >/dev/null && \
         sudo systemctl restart systemd-logind",
    );

    // SIGTERM by hand rather than `Guest::shutdown`: that consumes the guest — and its scratch
    // dir, and with it the supervisor log, which is where the evidence for *which* rung carried
    // the shutdown lives.
    unsafe { libc::kill(guest.supervisor_pid(), libc::SIGTERM) };
    guest
        .wait_for_supervisor_log(
            "asked the stock guest agent to power off",
            Duration::from_secs(60),
        )
        .unwrap_or_else(|e| {
            panic!(
                "the ladder never reached the guest-agent rung ({e}); with limina-agent stopped \
                 and the power key ignored, nothing else should have been able to stop this \
                 guest. qga log lines: {}",
                supervisor_qga_lines(&guest)
            )
        });

    let outcome = guest
        .wait_for_exit(Duration::from_secs(90))
        .expect("waiting for the VM to power itself off");
    assert!(
        !outcome.forced && outcome.signal.is_none() && outcome.code == Some(0),
        "the VM did not power itself off through the guest agent ({outcome:?}); it was forced \
         down at the end of the grace instead"
    );
}

/// How many clock steps the supervisor has reported so far — the second skew needs a *new*
/// one, and the first one's line is still in the log.
fn step_lines(guest: &Guest) -> usize {
    guest
        .supervisor_log()
        .lines()
        .filter(|l| l.contains("stepped it to the host's"))
        .count()
}

/// Everything the supervisor said about this transport, for a failure message.
fn supervisor_qga_lines(guest: &Guest) -> String {
    let log = guest.supervisor_log();
    let lines: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("qga") || l.contains("timesync"))
        .collect();
    if lines.is_empty() {
        "(none — the supervisor never logged about the guest agent)".to_string()
    } else {
        format!("\n{}", lines.join("\n"))
    }
}
