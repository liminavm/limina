// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! A hand-driven seated desktop on the **landmarks test's own guest**, for debugging it.
//!
//! `l2_desktop_restore_landmarks` boots a machine no other tool here reproduces: the enhanced
//! golden, EFI-booted through the GOP firmware into its own kernel, on a windowed coexist
//! display at 2560x1440, with the same device trims and pinned MAC. A poke VM started from the
//! CLI is a different machine on every one of those axes, so a fault that only shows up in the
//! test cannot be chased in one.
//!
//! This boots **exactly guest 1 of that test** — same config constructor, same trims, same
//! display — stages the workload assets and builds `vkstill`, then stops. It launches nothing
//! and shuts nothing down: the desktop sits there until told to quit, so the workload can be
//! started, killed and restarted by hand while the VM stays up.
//!
//! It never runs in the suite: without `LIMINA_POKE=1` it returns immediately.
//!
//! ```sh
//! LIMINA_POKE=1 scripts/test-boot.sh debug -E 'test(poke_seated_desktop)' --no-capture
//! ```
//!
//! It prints an ssh port and a **command file**. Writing a verb into that file drives the parts
//! of the machine that live on the host side of the wire, which ssh cannot reach:
//!
//! | verb | effect |
//! |---|---|
//! | `push-identity` | the landmarks test's identity — `LMN`/`L2 Landmark`, 96 dpi |
//! | `push-default` | the same size with no EDID override, i.e. back to the device default |
//! | `resize <W>x<H>` | drive the guest to another resolution |
//! | `snapshot` | take the suspend-leg snapshot, leaving the guest running |
//! | `quit` | shut the guest down and end the run |
//!
//! Everything guest-side is just ssh on the printed port.

use std::time::Duration;

use limina_test::{DisplayControl, EdidSpec, Guest, GuestConfig};

const DISPLAY_W: u32 = 2560;
const DISPLAY_H: u32 = 1440;

/// The identity `l2_desktop_restore_landmarks` pushes before it snapshots. Kept byte-identical
/// to that test's: a poke that pushed a *similar* identity would answer a question nobody asked.
fn pushed_identity() -> DisplayControl {
    DisplayControl {
        display_id: 0,
        position: None,
        size: Some((DISPLAY_W, DISPLAY_H)),
        connected: None,
        edid: Some(EdidSpec {
            refresh_hz: 60,
            dpi: 96,
            vendor: *b"LMN",
            product_id: 0x4C32,
            serial: 0x0000_0C2D,
            name: "L2 Landmark".into(),
            serial_string: Some("L2-LANDMARK".into()),
            range: None,
            modes: Vec::new(),
            alt_mode: None,
        }),
    }
}

/// The same push with the EDID left alone — the control for [`pushed_identity`], so "the push
/// did it" can be told apart from "any display update did it".
fn pushed_size_only() -> DisplayControl {
    DisplayControl {
        display_id: 0,
        position: None,
        size: Some((DISPLAY_W, DISPLAY_H)),
        connected: None,
        edid: None,
    }
}

fn ssh_retry(guest: &Guest, cmd: &str) -> String {
    let mut last_err = String::new();
    for _ in 0..4 {
        match guest.ssh_exec(cmd) {
            Ok(out) => return out.trim().to_string(),
            Err(e) => last_err = e.to_string(),
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    panic!("ssh `{cmd}` kept failing: {last_err}");
}

#[test]
fn poke_seated_desktop() {
    if std::env::var_os("LIMINA_POKE").is_none() {
        eprintln!("SKIPPED poke_seated_desktop: set LIMINA_POKE=1 to hold a desktop open");
        return;
    }
    if !limina_test::require_hvf_or_skip("poke_seated_desktop") {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED poke_seated_desktop: no KosmicKrisp ICD under /Volumes/mesa-cs/build-kk"
        );
        return;
    }
    let base_cfg = match GuestConfig::seated_efi_fedora_from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("SKIPPED poke_seated_desktop: {e}");
            return;
        }
    };

    // The landmarks test's guest 1, verbatim — trims, MAC, display and snapshot arming. The MAC
    // differs by one octet so this can run beside that test without both claiming .2.
    const NET_MAC: &str = "5a:94:ef:44:0f:ad";
    unsafe { std::env::set_var("LIMINA_BRACKET_NO_BUTTON", "1") };
    let cfg = base_cfg
        .with_supervisor_arg("--no-snd")
        .with_supervisor_arg("--no-battery")
        .with_net_mac(NET_MAC)
        .with_windowed_coexist_display(DISPLAY_W, DISPLAY_H)
        .with_net()
        .with_supervisor_log()
        .with_snapshot();

    eprintln!("POKE booting the seated desktop (this is the landmarks test's guest 1)");
    let mut g = Guest::boot(&cfg).expect("spawning the limina supervisor");
    g.wait_for_ssh_banner(Duration::from_secs(240))
        .expect("guest sshd never became reachable through gvproxy");
    g.ssh_poll("pgrep -x gnome-shell >/dev/null", Duration::from_secs(180))
        .expect("gnome-shell never appeared — the seated session didn't come up");

    // Stage what the workload needs, but start none of it: the point of a poke is to launch
    // each piece by hand and watch. vkstill is built in the guest, as the test builds it.
    let assets = limina_test::repo_root().join("crates/limina-test/assets");
    g.scp_to_guest(&assets.join("webgl-still.html"), "/tmp/webgl-still.html")
        .expect("pushing the WebGL page into the guest");
    ssh_retry(&g, "rm -rf /tmp/vkstill && mkdir -p /tmp/vkstill");
    for f in [
        "vkstill.c",
        "vkstill-spv.h",
        "xdg-shell.xml",
        "vkstill-build.sh",
    ] {
        g.scp_to_guest(&assets.join(f), &format!("/tmp/vkstill/{f}"))
            .unwrap_or_else(|e| panic!("pushing {f} into the guest: {e}"));
    }
    let built = ssh_retry(&g, "/tmp/vkstill/vkstill-build.sh 2>&1");
    assert!(
        built.contains("vkstill built"),
        "vkstill did not build in the guest:\n{built}"
    );

    let cmd_file = g.scratch_dir().join("poke-cmd");
    std::fs::write(&cmd_file, "").expect("creating the poke command file");
    eprintln!(
        "\nPOKE READY\n  ssh     ssh -p {} claude@127.0.0.1\n  cmd     echo push-identity > \
         {}\n  scratch {}\n  verbs   push-identity | push-default | resize WxH | snapshot | \
         quit\n",
        g.ssh_port(),
        cmd_file.display(),
        g.scratch_dir().display(),
    );

    loop {
        std::thread::sleep(Duration::from_millis(500));
        let Ok(raw) = std::fs::read_to_string(&cmd_file) else {
            continue;
        };
        let verb = raw.trim().to_string();
        if verb.is_empty() {
            continue;
        }
        // Consume it first: a verb that panics must not be replayed on the next tick.
        std::fs::write(&cmd_file, "").expect("clearing the poke command file");
        eprintln!("POKE verb: {verb}");
        match verb.as_str() {
            "push-identity" => {
                g.update_display(pushed_identity())
                    .expect("pushing the landmark identity");
            }
            "push-default" => {
                g.update_display(pushed_size_only())
                    .expect("pushing the default identity");
            }
            "snapshot" => g.snapshot().expect("snapshotting the guest"),
            "quit" => break,
            other => match other
                .strip_prefix("resize ")
                .and_then(|s| s.split_once('x'))
            {
                Some((w, h)) => match (w.trim().parse(), h.trim().parse()) {
                    (Ok(w), Ok(h)) => g.resize_display(w, h).expect("resizing the display"),
                    _ => eprintln!("POKE: unparseable resize in {other:?}"),
                },
                None => eprintln!("POKE: unknown verb {other:?}"),
            },
        }
    }

    eprintln!("POKE shutting down");
    let outcome = g
        .shutdown(Duration::from_secs(60))
        .expect("shutting the guest down");
    eprintln!("POKE done: {outcome:?}");
}
