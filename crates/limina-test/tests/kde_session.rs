// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! KDE Plasma reaches a rendered desktop with KWin compositing on OpenGL (virgl → vrend).
//!
//! KWin reads a GPU render-time query with a blocking `glGetQueryObject(GL_QUERY_RESULT)` after
//! every page flip. Guest Mesa asks the host for the result once and then re-reads the query
//! buffer until the host marks it done, so a renderer that does not finish a query which was not
//! ready when first asked leaves KWin spinning forever: black screen with a cursor, KWin's D-Bus
//! silent, plasmashell restarting every ~40 s behind it (`spikes/kde-kwin-gl-hang/`). GNOME never
//! exposed this — mutter polls its queries without blocking.
//!
//! Oracles, all read from the guest and the host capture:
//!  1. KWin answers `supportInformation` with `Compositing Type: OpenGL` on a `virgl` renderer, so
//!     a QPainter or llvmpipe fallback cannot pass;
//!  2. the host capture shows a real desktop, not black;
//!  3. plasmashell never restarted.
//!
//! EFI-boots the KDE golden (`seated_efi_kde_from_env`); SKIPs without it, the GOP firmware or
//! KosmicKrisp. Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::{Duration, Instant};

use limina_test::{Guest, GuestConfig};

/// Runs a command as the seated user, on its session bus. The user is whoever owns KWin, not
/// assumed to be the ssh user.
const AS_SESSION: &str = "p=$(pgrep -xo kwin_wayland); u=$(ps -o user= -p $p); uid=$(id -u $u); \
     sudo -u $u env XDG_RUNTIME_DIR=/run/user/$uid \
     DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$uid/bus";

/// KWin's own report of how it composites. Bounded: a hung KWin never answers, and busctl's
/// default wait is 25 s per poll.
fn kwin_support_information(guest: &Guest) -> String {
    guest
        .ssh_exec(&format!(
            "{AS_SESSION} timeout 10 busctl --user call org.kde.KWin /KWin org.kde.KWin \
             supportInformation 2>&1"
        ))
        .unwrap_or_else(|e| format!("(ssh failed: {e})"))
}

#[test]
fn kde_plasma_composites_with_opengl_and_renders() {
    if !limina_test::require_hvf_or_skip("kde_plasma_composites_with_opengl_and_renders") {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED kde_plasma_composites_with_opengl_and_renders: no KosmicKrisp ICD under \
             /Volumes/mesa-cs/build-kk (mount third_party/mesa-cs.sparseimage and ninja)"
        );
        return;
    }
    let cfg = match GuestConfig::seated_efi_kde_from_env() {
        Ok(cfg) => cfg
            .with_coexist_display(1280, 800)
            .with_net()
            .with_supervisor_log(),
        Err(e) => {
            eprintln!("SKIPPED kde_plasma_composites_with_opengl_and_renders: {e:#}");
            return;
        }
    };

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = guest
        .wait_for_ssh(Duration::from_secs(240))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");
    guest
        .ssh_poll("pgrep -x kwin_wayland >/dev/null", Duration::from_secs(180))
        .expect("kwin_wayland never appeared — the Plasma session didn't come up");

    // --- Oracle 1: KWin answers, compositing with OpenGL on virgl ---
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut info = kwin_support_information(&guest);
    while !info.contains("Compositing Type: OpenGL") && Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(5));
        info = kwin_support_information(&guest);
    }
    let renderer = info
        .split("\\n")
        .find(|l| l.starts_with("OpenGL renderer string:"))
        .unwrap_or("(no renderer line)")
        .to_string();
    eprintln!("KWin: {renderer}");
    assert!(
        info.contains("Compositing Type: OpenGL"),
        "KWin never reported OpenGL compositing. A KWin that does not answer at all is the \
         unfinished-query hang (spikes/kde-kwin-gl-hang/). supportInformation:\n{info}"
    );
    assert!(
        renderer.contains("virgl"),
        "KWin composites on {renderer:?}, not virgl — the vrend path this test guards is not \
         under test"
    );

    // --- Oracle 2: the host capture shows the desktop ---
    let frame = guest
        .wait_for_rich_capture(Duration::from_secs(120), 1000, 0.90)
        .unwrap_or_else(|e| {
            panic!(
                "the Plasma desktop never presented a rich frame through the host capture: {e:#}"
            )
        });
    let (colors, dominance) = frame.richness();
    eprintln!(
        "first rich Plasma frame via host capture: {colors} distinct colors, dominant {dominance:.2}"
    );

    // --- Oracle 3: plasmashell came up once and stayed up ---
    let restarts = guest
        .ssh_exec(&format!(
            "{AS_SESSION} systemctl --user show plasma-plasmashell.service -p NRestarts --value"
        ))
        .expect("reading plasmashell's restart count");
    eprintln!("plasmashell restarts: {}", restarts.trim());
    assert_eq!(
        restarts.trim(),
        "0",
        "plasmashell restarted — it times out waiting on a KWin that stopped answering"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}
