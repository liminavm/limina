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
//! The suite boots the *enhanced* image, where the other transport (`limina-agent-session`
//! over the control plane) is installed and live. A round trip proves *a* clipboard works
//! — not *which* one carried it. If the vdagent broker were entirely broken, the other path
//! would deliver both tokens and this test would go green; #37's step 4 deleted the GNOME
//! shell-extension tier on the strength of this green, so a green that could come from
//! elsewhere would have been worth nothing. So oracle 3.5 **stops the session helper first**
//! and proves it stayed stopped: with no control-plane clipboard peer left, only vdagent can
//! move the text. Passing for the right reason is the entire point of this file.
//!
//! Oracle 3c is the other half of that: it asserts the helper *yielded* on its own (a HELLO
//! with no `clipboard` capability) before 3.5 stops anything, which is what keeps the two
//! transports from both owning the selection in normal operation.
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
            // A GPU is not optional here even though nothing draws: without a display device
            // gdm has no seat to start a session on, dies with "no session desktop files
            // installed", and systemd restarts it forever — which surfaces as a pile of
            // sequential tty sessions and no spice-vdagent, i.e. looking exactly like a
            // broken clipboard (2026-08-15). The agent rides the graphical session, so the
            // session is a prerequisite, not scenery.
            .with_coexist_display(1280, 800)
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

    // --- Oracle 0: there IS a seated session ---
    //
    // Ahead of anything clipboard-shaped, because every oracle below is downstream of it and
    // would otherwise mis-report a dead session as a clipboard failure.
    guest
        .ssh_poll("pgrep -x gnome-shell >/dev/null", Duration::from_secs(180))
        .expect("gnome-shell never appeared — the seated session didn't come up");

    // --- Oracle 1: the port reached the guest ---
    let port = ssh_soft(&guest, &format!("ls -l {PORT}"));
    assert!(
        port.contains("com.redhat.spice.0"),
        "the guest has no {PORT} — the named virtio-serial port never reached it, so nothing \
         downstream can work. Check that the supervisor passed --spice-fd and that the worker \
         attached the port (crates/limina-vmm/src/krun/console.rs). ls said:\n{port}"
    );
    eprintln!("spice port present: {port}");

    // --- Precondition: the guest kernel can host vdagentd at all ---
    //
    // `spice-vdagentd` opens /dev/uinput (it injects pointer/tablet events) and treats a
    // failure as FATAL — it quits, and the session agent exits cleanly right after because
    // its socket closed. A guest kernel without uinput therefore cannot serve the stock-tier
    // clipboard no matter what we do, so this is a SKIP (the subject under test is
    // unreachable), never a pass. The F44 enhanced kernel has it; the stale F43 pair's
    // 6.12 kernel does not — see task #31.
    if ssh_soft(&guest, "test -e /dev/uinput && echo yes").trim() != "yes" {
        let kernel = ssh_soft(&guest, "uname -r");
        eprintln!(
            "SKIPPED a_seated_guest_shares_the_clipboard_through_spice_vdagent: guest kernel \
             {kernel} has no /dev/uinput, so spice-vdagentd dies on startup (\"Fatal uinput \
             error\") and no session agent can survive. This is an IMAGE property, not a \
             clipboard bug — run against the F44 family (LIMINA_FEDORA_REL=44), or refresh \
             the F43 pair (task #31)."
        );
        return;
    }

    // --- Oracle 2: the stock trigger fired, with no help from us ---
    //
    // The SOCKET, not the service: Fedora socket-activates `spice-vdagentd`, so the service
    // is legitimately `inactive (dead)` until a session client connects to it. Asserting on
    // the service here failed a guest that was in fact perfectly healthy (2026-08-15) — the
    // unit was loaded, the package installed, and `TriggeredBy: ● spice-vdagentd.socket`
    // said so in the very output the assertion printed. The service does have to come up,
    // but that is oracle 3b, after the client exists to trigger it.
    let sock = ssh_soft(
        &guest,
        "systemctl is-active spice-vdagentd.socket || systemctl status spice-vdagentd.socket --no-pager",
    );
    assert!(
        sock.starts_with("active"),
        "spice-vdagentd.socket is not active even though {PORT} exists. Either the guest has no \
         spice-vdagent package installed (then this image cannot serve the stock tier — install \
         it, it is in Fedora's default Workstation set) or its udev rule did not fire:\n{sock}"
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
            // The agent is started by /etc/xdg/autostart/spice-vdagent.desktop, i.e. by the
            // graphical session — so when it is missing, the question is almost always "is
            // there a healthy session at all?", not "is the clipboard broken?". Collect
            // enough to tell those apart in one go: a session that keeps restarting shows up
            // as a pile of sequential VTs in loginctl and as repeated gdm entries.
            // Which guest is this, and does it have the one device vdagentd refuses to live
            // without? `spice-vdagentd` opens /dev/uinput (it injects pointer/tablet events)
            // and treats a failure as fatal, taking the session agent down with it.
            let ident = ssh_soft(
                &guest,
                "uname -r; ls -l /dev/uinput 2>&1; lsmod | grep -c uinput; \
                 grep -c uinput /lib/modules/$(uname -r)/modules.devname 2>&1",
            );
            let sessions = ssh_soft(&guest, "loginctl list-sessions --no-pager");
            let procs = ssh_soft(&guest, "pgrep -a -f vdagent");
            // Fedora ships BOTH an XDG autostart entry and a user unit (the .desktop carries
            // X-systemd-skip=true), so "why didn't it start" is a question about the unit's
            // conditions and the agent's own log, not about us.
            let unit = ssh_soft(
                &guest,
                &in_session(
                    "export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus; \
                     systemctl --user status spice-vdagent.service --no-pager; \
                     systemctl --user cat spice-vdagent.service --no-pager",
                ),
            );
            let agent_log = ssh_soft(
                &guest,
                "journalctl -b --no-pager -t spice-vdagent | tail -20",
            );
            // The session agent exits when its connection to the system daemon closes, so the
            // daemon's own account (and ours — it is talking to OUR broker over the port) is
            // what explains a clean early exit.
            let daemon_log = ssh_soft(
                &guest,
                "journalctl -b --no-pager -u spice-vdagentd -t spice-vdagentd | tail -30",
            );
            let ours: String = guest
                .supervisor_log()
                .lines()
                .filter(|l| l.contains("vdagent") || l.contains("clipboard"))
                .collect::<Vec<_>>()
                .join("\n");
            let gdm = ssh_soft(&guest, "journalctl -b --no-pager -u gdm | tail -40");
            let shell = ssh_soft(
                &guest,
                "journalctl -b --no-pager -t gnome-shell -t gnome-session-binary | tail -40",
            );
            panic!(
                "no per-session `spice-vdagent` process. It is started by the graphical \
                 session's XDG autostart, so check the session's health FIRST — many \
                 sequential tty sessions below means the session is restarting and the \
                 clipboard is a symptom, not the disease.\n\
                 == guest / uinput ==\n{ident}\n== loginctl ==\n{sessions}\n\
                 == vdagent processes ==\n{procs}\n\
                 == spice-vdagent user unit ==\n{unit}\n== agent log ==\n{agent_log}\n\
                 == vdagentd ==\n{daemon_log}\n== our broker ==\n{ours}\n\
                 == gdm ==\n{gdm}\n== session ==\n{shell}"
            );
        }
        std::thread::sleep(Duration::from_secs(3));
    }
    eprintln!("session agent running");

    // --- Oracle 3b: the client's connection actually brought the daemon up ---
    let daemon = ssh_soft(
        &guest,
        "systemctl is-active spice-vdagentd || systemctl status spice-vdagentd --no-pager",
    );
    assert!(
        daemon.starts_with("active"),
        "the session agent is running but socket activation never started spice-vdagentd, so \
         nothing is bridging the port to the guest selection:\n{daemon}"
    );

    // --- Oracle 3c: arbitration — the helper YIELDED to vdagent (#37 step 3) ---
    //
    // Both transports are present on this guest, and two selection owners in one session
    // fight through mutter's X11<->Wayland bridging. The switch is thrown at capability
    // negotiation: with a live spice-vdagent, limina-agent-session must HELLO with NO
    // `clipboard` capability, so the host never routes clipboard traffic to it. The host
    // log line is the oracle because it records what the guest actually announced.
    //
    // A helper that never connects at all is not a pass — it would make the assertion
    // vacuous — so we require the HELLO to be there, and to be capability-less. Guests
    // that simply have no helper installed are excluded first, positively.
    let helper_installed = !ssh_soft(&guest, "command -v limina-agent-session").is_empty();
    if helper_installed {
        let deadline = Instant::now() + Duration::from_secs(60);
        let hellos = loop {
            let hellos: Vec<String> = guest
                .supervisor_log()
                .lines()
                .filter(|l| l.contains("guest agent connected") && l.contains("agent-session"))
                .map(|l| l.to_string())
                .collect();
            if !hellos.is_empty() || Instant::now() >= deadline {
                break hellos;
            }
            std::thread::sleep(Duration::from_secs(2));
        };
        assert!(
            !hellos.is_empty(),
            "limina-agent-session is installed but never connected to the control plane, so \
             the arbitration below would assert nothing. Check the user unit in the session."
        );
        let claimed: Vec<&String> = hellos.iter().filter(|l| l.contains("clipboard")).collect();
        assert!(
            claimed.is_empty(),
            "limina-agent-session announced the clipboard capability while spice-vdagent is \
             serving this session — two selection owners, the exact fight #37 step 3 forbids. \
             If the code is right, this image is carrying a pre-arbitration helper: rebuild \
             the guest tools and re-run install-enhanced.sh.\n{}",
            claimed
                .iter()
                .map(|l| l.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        );
        eprintln!(
            "helper yielded the clipboard to vdagent ({} HELLO)",
            hellos.len()
        );
    }

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
                                                // `pgrep -x` refuses names longer than 15 characters (it warns and matches nothing), and
                                                // "limina-agent-session" is 20 — so match the command line instead. The bracket keeps the
                                                // pattern from matching the shell that carries it.
    let still_up = ssh_soft(&guest, "pgrep -f '[l]imina-agent-session'");
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
    // `wl-copy` must STAY RESIDENT to serve the selection, so it has to be detached from the
    // ssh channel: left attached it holds the channel's fds open and ssh never returns, which
    // looks like a hung test rather than a working clipboard (it hung for 10 minutes on
    // 2026-08-15 while the copy itself was perfectly fine).
    ssh_soft(
        &guest,
        &in_session(&format!(
            "setsid --fork wl-copy '{to_host}' >/dev/null 2>&1 </dev/null"
        )),
    );
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
