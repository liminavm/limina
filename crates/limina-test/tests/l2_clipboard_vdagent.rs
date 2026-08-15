//! L2 — **clipboard over spice-vdagent**, the stock-tier transport (task #37).
//!
//! This is the test the deletions in #37 wait on. Until a real guest round-trips through
//! `spice-vdagent`, removing the gnome-shell extension / ext-data-control backend / the
//! `LIMINA_CLIPBOARD_RD` path would take the clipboard away rather than move it.
//!
//! # Why this transport exists
//!
//! Stock Fedora already ships `spice-vdagent`, and `70-spice-vdagentd.rules` starts it the
//! moment a virtio-serial port named `com.redhat.spice.0` appears. So the whole feature
//! lands on a guest with **nothing of ours installed** — the two-tier guarantee's baseline.
//! `limina-agent` keeps the enhanced-tier capabilities on the control plane; both feed the
//! one pasteboard owner in the supervisor.
//!
//! # What each oracle is actually for
//!
//! 1. **The port is on the bus** (`/dev/virtio-ports/com.redhat.spice.0`). Fails first and
//!    loudest, because everything downstream is a slower way to discover the same thing.
//! 2. **udev started the daemon.** `spice-vdagentd` active means the *stock* trigger fired
//!    — not that we installed something.
//! 3. **A session agent is running.** `spice-vdagentd` is a system daemon; the per-session
//!    `spice-vdagent` is what actually owns the guest clipboard. Without it, both
//!    directions fail with everything else looking healthy.
//! 4. **host → guest** and **5. guest → host**, the round trip.
//!
//! # The discriminator, and why it is load-bearing
//!
//! The suite boots the *enhanced* image, where the old transport (`limina-agent-session` +
//! the `clipboard@limina` shell extension, over the control plane) is still installed and
//! live. A round trip proves *a* clipboard works — not *which* one carried it. If the
//! vdagent broker were entirely broken, the old path would deliver both tokens and this
//! test would go green, and #37's step 4 would then delete the only working transport on
//! the strength of that green. So oracle 3.5 **stops the session helper first** and proves
//! it stayed stopped: with no control-plane clipboard peer left, only vdagent can move the
//! text. Passing for the right reason is the entire point of this file.
//!
//! # Traps this test is shaped around
//!
//! - It drives a **private named pasteboard** (`LIMINA_PASTEBOARD`), never the general one.
//!   A test that clobbers the developer's real clipboard on every suite run is not a test
//!   anyone will keep running.
//! - `wl-copy`/`wl-paste` need the session's `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR`, and a
//!   non-login ssh shell has neither — the same false negative that makes `vulkaninfo`
//!   enumerate nothing over ssh. Both are set explicitly.
//! - The host→guest direction is **poll-driven**: the supervisor watches `changeCount` on a
//!   timer (500 ms by default), so the copy is not instant and a single check right after
//!   the write is a race, not an assertion.

use limina_test::{Guest, GuestConfig};
use std::time::{Duration, Instant};

/// The udev-matched port name. Its presence in the guest is the whole stock-tier trigger.
const PORT: &str = "/dev/virtio-ports/com.redhat.spice.0";

/// How long a copy gets to cross. Generous next to the 500 ms poll: a busy guest session
/// can take a moment to hand the selection over, and a flaky-looking clipboard test is
/// worse than a slow one.
const CROSS: Duration = Duration::from_secs(45);

/// Run a guest command inside the seated session's environment.
///
/// `wl-copy`/`wl-paste` talk to the Wayland compositor, so they need the session's socket
/// and runtime dir; an ssh shell is not a login shell and has neither. The `WAYLAND_DISPLAY`
/// is discovered rather than assumed — it is `wayland-0` in practice, but a session that
/// picked a different number would otherwise look like a broken clipboard.
fn in_session(cmd: &str) -> String {
    format!(
        "export XDG_RUNTIME_DIR=/run/user/$(id -u); \
         export WAYLAND_DISPLAY=$(ls \"$XDG_RUNTIME_DIR\" 2>/dev/null | grep -m1 '^wayland-[0-9]\\+$'); \
         {cmd}"
    )
}

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

#[test]
fn a_seated_guest_shares_the_clipboard_through_spice_vdagent() {
    if !limina_test::require_hvf_or_skip(
        "a_seated_guest_shares_the_clipboard_through_spice_vdagent",
    ) {
        return;
    }
    let pb_name = format!("limina-l2-vdagent-{}", std::process::id());
    let cfg = match GuestConfig::seated_efi_fedora_from_env() {
        Ok(cfg) => cfg
            .with_net()
            .with_supervisor_log()
            .with_env("LIMINA_PASTEBOARD", &pb_name)
            // Tighten the pasteboard poll so the host→guest direction does not spend most
            // of its budget waiting for the next tick.
            .with_env("LIMINA_CLIP_POLL_MS", "200"),
        Err(e) => {
            eprintln!("SKIPPED a_seated_guest_shares_the_clipboard_through_spice_vdagent: {e:#}");
            return;
        }
    };

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");

    // --- Oracle 1: the port reached the guest ---
    let port = ssh_soft(&guest, &format!("ls -l {PORT}"));
    assert!(
        port.contains("com.redhat.spice.0"),
        "the guest has no {PORT} — the named virtio-serial port never reached it, so nothing \
         downstream can work. Check that the supervisor passed --spice-fd and that the worker \
         attached the port (crates/limina-vmm/src/krun/console.rs). ls said:\n{port}"
    );
    eprintln!("spice port present: {port}");

    // --- Oracle 2: udev started the system daemon, with no help from us ---
    let daemon = ssh_soft(
        &guest,
        "systemctl is-active spice-vdagentd || systemctl status spice-vdagentd --no-pager | tail -20",
    );
    assert!(
        daemon.starts_with("active"),
        "spice-vdagentd is not active even though {PORT} exists. Either the guest has no \
         spice-vdagent package installed (then this image cannot serve the stock tier — install \
         it, it is in Fedora's default Workstation set) or its udev rule did not fire:\n{daemon}"
    );

    // --- Oracle 3: the SESSION agent is the one that owns the clipboard ---
    //
    // Give it a moment: the seated session may still be starting when sshd answers.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if !ssh_soft(&guest, "pgrep -x spice-vdagent").is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            let sessions = ssh_soft(&guest, "loginctl list-sessions --no-pager");
            let units = ssh_soft(
                &guest,
                "systemctl --user list-units 'spice*' --no-pager || true",
            );
            panic!(
                "no per-session `spice-vdagent` process. spice-vdagentd (the system daemon) is \
                 active, but it is the session agent that owns the guest clipboard — without it \
                 both directions fail while everything else looks healthy.\n\
                 == loginctl ==\n{sessions}\n== user units ==\n{units}"
            );
        }
        std::thread::sleep(Duration::from_secs(3));
    }
    eprintln!("session agent running");

    // --- 3.5: silence the OTHER transport, so the round trip can only mean vdagent ---
    //
    // `systemctl --user`, not `pkill`: the unit is `Restart=always`, so a killed helper is
    // back in two seconds and the test would pass through the very path it is supposed to
    // exclude. A guest without the helper installed makes this a harmless no-op.
    ssh_soft(
        &guest,
        &in_session(
            "export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus; \
             systemctl --user stop limina-agent-session.service",
        ),
    );
    std::thread::sleep(Duration::from_secs(2)); // longer than RestartSec, so a respawn shows
    let still_up = ssh_soft(&guest, "pgrep -x limina-agent-session");
    assert!(
        still_up.is_empty(),
        "limina-agent-session is still running (pids {still_up:?}) after being stopped, so a \
         successful round trip below would not prove spice-vdagent carried it — and #37's \
         deletions must not be cleared by a test that passes through the transport they remove."
    );
    eprintln!("control-plane clipboard peer stopped; vdagent is the only transport left");

    // --- Oracle 4: host → guest ---
    let to_guest = "limina-host-copy-4417";
    limina_test::set_pasteboard_text(&pb_name, to_guest);
    let deadline = Instant::now() + CROSS;
    let last;
    loop {
        let seen = ssh_soft(&guest, &in_session("wl-paste --no-newline"));
        if seen.contains(to_guest) {
            last = seen;
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the host copy never reached the guest clipboard within {}s (guest sees {seen:?}). \
             The port, the daemon and the session agent are all up, so this is the broker's \
             grab/request handshake — check the supervisor log for `vdagent:` warnings.",
            CROSS.as_secs()
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    eprintln!("host→guest OK: guest clipboard holds {last:?}");

    // --- Oracle 5: guest → host ---
    let to_host = "limina-guest-copy-9265";
    ssh_soft(&guest, &in_session(&format!("wl-copy '{to_host}'")));
    let deadline = Instant::now() + CROSS;
    loop {
        if limina_test::pasteboard_text(&pb_name).as_deref() == Some(to_host) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the guest copy never reached the host pasteboard within {}s (pasteboard holds {:?}). \
             The guest's agent grabs and we request eagerly, so a failure here is the request or \
             the reply — check the supervisor log for `vdagent:` warnings.",
            CROSS.as_secs(),
            limina_test::pasteboard_text(&pb_name)
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    eprintln!("guest→host OK: pasteboard holds the guest text");

    let outcome = guest
        .shutdown(Duration::from_secs(30))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
}
