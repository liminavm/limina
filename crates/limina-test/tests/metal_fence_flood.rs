// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 guard: one render-node client cannot fill the virtio-gpu control queue with fences.
//!
//! A Metal-transport FENCE keeps its control-queue descriptor until the ring reaches its seqno.
//! A client asking for fences on a seqno that never comes would hold every descriptor, and every
//! other virtio-gpu command, the display's included, would block behind them. The guest module
//! refuses a file's fences with EBUSY past a bound, so the device keeps answering.
//!
//! `guest/liminafenceflood.py` floods from one file and holds it open; a second process then asks
//! the device for its INFO, which goes through the same control queue.
//!
//! Prereqs as the other metal L2s: the `metal.test` image with the vendor drivers; SKIPs cleanly if
//! missing. Gated behind LIMINA_HVF_TESTS; run via `scripts/test-boot.sh`.

use std::time::Duration;

use limina_test::{Guest, GuestConfig};

const FLOOD: &str = include_str!("../guest/liminafenceflood.py");

#[test]
fn a_fence_flood_from_one_client_leaves_the_device_answering() {
    let test = "a_fence_flood_from_one_client_leaves_the_device_answering";
    if !limina_test::require_hvf_or_skip(test) {
        return;
    }
    let cfg = match GuestConfig::metal_efi_fedora_from_env() {
        Ok(cfg) => cfg.with_coexist_display(1280, 800).with_net(),
        Err(e) => {
            eprintln!("SKIPPED {test}: {e}");
            return;
        }
    };

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh(Duration::from_secs(240))
        .expect("guest sshd never became reachable through the forward");
    // sshd can answer before the GPU driver has probed.
    guest
        .ssh_poll("ls /dev/dri/renderD*", Duration::from_secs(120))
        .expect("no render node appeared");
    guest
        .ssh_exec(&format!(
            "cat > /tmp/liminafenceflood.py <<'FLOOD_PY_EOF'\n{FLOOD}\nFLOOD_PY_EOF"
        ))
        .expect("writing liminafenceflood.py to the guest");
    guest
        .ssh_exec(
            "sudo -n sh -c 'nohup python3 /tmp/liminafenceflood.py flood \
             >/tmp/flood.out 2>&1 </dev/null &'; echo started",
        )
        .expect("starting the flood");

    let flood = guest
        .ssh_poll(
            "grep -h '^FLOOD\\|Error\\|error' /tmp/flood.out",
            Duration::from_secs(30),
        )
        .unwrap_or_else(|e| {
            let out = guest.ssh_exec("cat /tmp/flood.out").unwrap_or_default();
            panic!("the flooding client never got its ioctls back ({e}): {out}")
        });
    assert!(flood.contains("FLOOD "), "the flood failed: {flood}");
    let field = |k: &str| -> u32 {
        flood
            .split_whitespace()
            .find_map(|f| f.strip_prefix(k)?.strip_prefix('='))
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("no {k} in {flood:?}"))
    };
    assert!(field("busy") > 0, "no fence was refused: {flood}");

    let probe = guest
        .ssh_exec_timeout(
            "timeout 10 sudo -n python3 /tmp/liminafenceflood.py probe 2>&1; echo rc=$?",
            Duration::from_secs(30),
        )
        .expect("ssh died during the probe");
    assert!(
        probe.contains("PROBE ok"),
        "the device stopped answering behind the flood: {probe}"
    );

    let _ = guest.ssh_exec("sudo -n pkill -f liminafenceflood.py");
    guest
        .shutdown(Duration::from_secs(30))
        .expect("guest shutdown");
}
