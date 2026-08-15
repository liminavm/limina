//! L2 — the **synoik session smoke test** (task #40).
//!
//! synoik is our Vulkan compositor (the gnome-shell/mutter replacement). The suite has never
//! booted it: every seated L2 test drives the GNOME enhanced image, so a synoik-only regression
//! ships unguarded. This is that guard, and it exists because one already shipped.
//!
//! # The regression it was written against
//!
//! guest-tools r9 (2026-08-15) was the first payload built after our kernel fork dropped two
//! virtio-gpu commits on 2026-08-04 — `74ae69adc645` (advertise `DRM_FORMAT_MOD_LINEAR` on
//! planes) and `1f4c2049b30b` (widen the primary plane format list). Eleven days with no kernel
//! rebuild meant every VM kept running the pre-drop binary, so nothing noticed. On 7.1.8, the
//! first build without them, the primary plane advertises `XR24` with the **implicit/`INVALID`**
//! modifier while synoik allocates explicit `XR24 + LINEAR` through Vulkan — the intersection
//! `DrmCompositor::new` needs is empty:
//!
//! ```text
//! WARN synoik::backend::tty: error connecting connector: error creating DRM compositor for the
//!      Vulkan renderer
//!      No supported plane buffer format found
//! ```
//!
//! No compositor → gdm never takes the display → `plymouth-quit-wait` never completes →
//! `graphical.target` parks forever and the screen sits on a plymouth frame. The guest is
//! otherwise healthy: **ssh works and no unit has failed**, which is exactly why a boot-reached-
//! sshd oracle would have called this green. See `docs/images.md` §KNOWN DRIFT and task #39.
//!
//! # Oracles, in the order they fire
//!
//! 1. **The format negotiation succeeded** — no `No supported plane buffer format found` in
//!    `journalctl -t synoik`. Checked first and failed fast: in the broken state the guest is
//!    reachable but `graphical.target` never arrives, so waiting for anything else just burns
//!    the timeout to reach the same conclusion with a worse message.
//! 2. **The compositor is up and serving** — synoik logged `listening on Wayland socket` (INFO,
//!    so it survives a journald level change) and its process is still alive.
//! 3. **A session took seat0** — `loginctl` shows a seated session, i.e. something is actually
//!    driving the display rather than the compositor merely not having logged an error.
//! 4. **It painted** — the captured scanout shows a rich frame. Deliberately **last and gated
//!    behind 1–3**: the stuck plymouth frame is *not* black, so a naive "the capture has pixels"
//!    assertion passes in the broken state. This oracle's job is a future
//!    negotiates-fine-but-renders-black regression, not this one.
//!
//! # Do NOT re-add `systemctl is-system-running` (task #41)
//!
//! It was oracle 2 in the first draft and the green control killed it: on a **healthy** synoik
//! guest — compositor running, Wayland socket up, seat0 session, scanout buffers exported —
//! nothing ever tells plymouth to quit, so `plymouth --wait` blocks forever,
//! `plymouth-quit-wait.service` never completes, and `multi-user.target`/`graphical.target` sit
//! `waiting` indefinitely. `is-system-running` reports `starting` for the life of the VM. On
//! GNOME images gdm performs that handoff; under a synoik session it does not happen. Asserting
//! on systemd's boot state here makes the test unpassable, not strict. (Same stall also strands
//! `limina-kernel-promote.service` — task #42.)
//!
//! Boots through the **GOP firmware** (`seated_efi_synoik_from_env`) — that is load-bearing, see
//! the helper's doc comment: the injected-kernel seated path runs a 6.12 test kernel that still
//! advertises LINEAR and would be green regardless of what the compositor does.

use limina_test::{Guest, GuestConfig};
use std::time::{Duration, Instant};

/// Marker synoik logs when the plane/renderer format intersection comes up empty.
const NO_FORMAT: &str = "No supported plane buffer format found";
/// The WARN synoik logs around it, one line up — the same failure, wider net.
const NO_CONNECTOR: &str = "error connecting connector";
/// INFO synoik logs once the compositor is up and accepting clients.
const SERVING: &str = "listening on Wayland socket";

/// How long the compositor gets to come up after sshd answers. It is serving within a couple of
/// seconds of the session starting on a healthy guest; the cap exists so the broken path fails
/// *here*, with a dump, instead of riding the per-command ssh timeout.
const COMPOSITOR_UP: Duration = Duration::from_secs(180);

/// Run a guest command, tolerating a non-zero exit (several oracles below query state through
/// commands that report the state *as* their exit code) and a transient ssh failure.
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
fn synoik_session_reaches_a_rendered_desktop() {
    if !limina_test::require_hvf_or_skip("synoik_session_reaches_a_rendered_desktop") {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED synoik_session_reaches_a_rendered_desktop: no KosmicKrisp ICD under \
             /Volumes/mesa-cs/build-kk (mount third_party/mesa-cs.sparseimage and ninja)"
        );
        return;
    }
    let cfg = match GuestConfig::seated_efi_synoik_from_env() {
        Ok(cfg) => cfg
            .with_coexist_display(1280, 800)
            .with_net()
            .with_supervisor_log(),
        Err(e) => {
            eprintln!("SKIPPED synoik_session_reaches_a_rendered_desktop: {e:#}");
            return;
        }
    };
    eprintln!("EFI-booting the synoik enhanced image: {:?}", cfg.boot);

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");

    // --- Oracles 1 + 2, interleaved so the failure marker wins the race ---
    //
    // synoik logs the format failure within seconds of the session starting, well before the
    // deadline, so polling both together means the broken case reports *why* the compositor never
    // came up rather than just that it didn't.
    let deadline = Instant::now() + COMPOSITOR_UP;
    loop {
        let synoik_log = ssh_soft(
            &guest,
            "sudo journalctl --boot=0 -t synoik --no-pager 2>/dev/null | tail -n 300",
        );
        assert!(
            !synoik_log.contains(NO_FORMAT) && !synoik_log.contains(NO_CONNECTOR),
            "synoik could not negotiate a scanout format — the DRM plane's formats and the Vulkan \
             renderer's have an empty intersection, so no compositor takes the display and the \
             boot parks on plymouth. This is the stock-virtio-gpu INVALID-modifier case (task \
             #39, docs/images.md §KNOWN DRIFT): the plane advertises XR24+INVALID, synoik \
             allocates XR24+LINEAR. synoik log tail:\n{synoik_log}"
        );

        // Serving *and* still alive: the log line alone would also be satisfied by a compositor
        // that came up and then died.
        if synoik_log.contains(SERVING) && !ssh_soft(&guest, "pgrep -x synoik").is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            let sessions = ssh_soft(&guest, "loginctl list-sessions --no-pager");
            let procs = ssh_soft(&guest, "pgrep -a 'synoik|gdm|plymouth'");
            let jobs = ssh_soft(&guest, "systemctl list-jobs --no-pager");
            panic!(
                "synoik never reported `{SERVING}` with a live process within {}s — it did NOT \
                 log the known format failure, so this is a different stall.\n\
                 == loginctl ==\n{sessions}\n== synoik/gdm/plymouth processes ==\n{procs}\n\
                 == systemctl list-jobs (FYI: plymouth-quit-wait parks forever here, task #41) \
                 ==\n{jobs}\n== synoik journal ==\n{synoik_log}",
                COMPOSITOR_UP.as_secs()
            );
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    eprintln!("synoik is up and serving");

    // --- Oracle 3: a session actually took seat0 ---
    let sessions = ssh_soft(&guest, "loginctl list-sessions --no-pager");
    assert!(
        sessions.contains("seat0"),
        "no session on seat0 — synoik started without a failure but nothing is driving the \
         display:\n{sessions}"
    );
    eprintln!("sessions:\n{sessions}");

    // --- Oracle 4: it painted ---
    //
    // Gated behind the three above on purpose: the stuck-plymouth frame is a *rendered* frame, so
    // this alone does not discriminate the #39 failure. A real desktop yields thousands of
    // distinct colors with no single dominant one; a black or single-color session yields ~1 color
    // at ~100% dominance. The capture is sparse for a static screen — poll and keep the best frame.
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut best_colors = 0usize;
    let mut best_dominance = 1.0f64;
    while Instant::now() < deadline {
        if let Ok(frame) = guest.read_capture() {
            let colors = frame.distinct_colors();
            let (_, dominance) = frame.dominant_color();
            if colors > best_colors {
                best_colors = colors;
                best_dominance = dominance;
            }
            if best_colors >= 1000 && best_dominance < 0.90 {
                break; // unambiguously a rendered desktop — stop early
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    eprintln!("richest synoik frame: {best_colors} distinct colors, dominant {best_dominance:.2}");
    assert!(
        best_colors >= 1000 && best_dominance < 0.90,
        "synoik came up and took seat0 but never painted a rich frame (richest: {best_colors} \
         colors, {best_dominance:.2} dominant) — scanout or Vulkan-render regression"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(30))
        .expect("supervisor did not stop");
    eprintln!("teardown outcome: {outcome:?}");
}
