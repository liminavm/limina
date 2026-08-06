// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Worker fd-census regression test for the venus carrier-fd ratchets (EMFILE class).
//!
//! Every cross-context attach of a venus blob — any dmabuf shared between two guest
//! processes or VkInstances, which compositors do constantly — sends the exporter's
//! SHM carrier fd to the render server over SCM_RIGHTS, minting a new host fd on
//! receive. The venus SHM import used to mmap and then DROP that received fd
//! (virglrenderer patch 0032): one leaked fd per attach, owned by no context scope,
//! surviving guest teardown, ratcheting the worker to RLIMIT_NOFILE over a session
//! (a class of live compositor crashes). The same
//! census also guards the earlier ratchet classes (ring/reply shmem, export-side
//! carrier fd — virgl 0022/0030).
//!
//! Vehicle: `guest/vkfdcycle.py` (pure python3 + ctypes, nothing to install on the
//! guest), two VkInstances = two venus contexts, dma_buf export/import + full
//! teardown, N times. Oracle: `lsof` on the worker counting `vkr-metal-mem` POSIX-shm
//! carrier fds — it must return to baseline after the run. Pre-0032 this fails at
//! baseline + 2·N (both contexts' attaches leak); post-0032 it is flat.
//!
//! Boots the seated ENHANCED golden (`seated_fedora_from_env`): dma_buf external
//! memory needs our mesa's venus — STOCK mesa's venus advertises neither
//! `VK_KHR_external_memory_fd` nor dma_buf against this host, so the cycler can't run
//! on the stock image (verified empirically). The idle gnome-shell session
//! holds its own carriers; both censuses are settle-polled so its startup churn can't
//! straddle them. Same prereqs as `venus_replay`: 16 KiB kernel + enhanced.test disk +
//! KosmicKrisp; SKIPs cleanly if missing. Gated behind LIMINA_HVF_TESTS; run via
//! `scripts/test-boot.sh`.

use std::process::Command;
use std::time::{Duration, Instant};

use limina_test::{Guest, GuestConfig};

const VKFDCYCLE: &str = include_str!("../guest/vkfdcycle.py");
const CYCLES: usize = 20;

/// Count the worker's open `vkr-metal-mem` POSIX-shm carrier fds (one lsof line per fd).
/// This is the exact census the field diagnosis used: every venus VkDeviceMemory export
/// on a macOS host is such a carrier, so a census that doesn't return to baseline is a
/// leaked-fd ratchet regardless of which class leaked it.
fn carrier_fd_census(worker: libc::pid_t) -> usize {
    // lsof exits non-zero when some fds are unreadable; the listing is still usable,
    // so only spawn failure is fatal.
    let out = Command::new("lsof")
        .args(["-p", &worker.to_string()])
        .output()
        .expect("running lsof against the worker");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("vkr-metal-mem"))
        .count()
}

/// The census once it stops moving: two consecutive identical readings 1s apart
/// (venus teardown is asynchronous relative to the guest process exiting).
fn settled_census(worker: libc::pid_t, timeout: Duration) -> usize {
    let deadline = Instant::now() + timeout;
    let mut last = carrier_fd_census(worker);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(1));
        let now = carrier_fd_census(worker);
        if now == last {
            return now;
        }
        last = now;
    }
    last
}

#[test]
fn cross_context_attach_leaves_worker_fd_census_flat() {
    if !limina_test::require_hvf_or_skip("cross_context_attach_leaves_worker_fd_census_flat") {
        return;
    }
    if limina_test::kosmickrisp_icd().is_none() {
        eprintln!(
            "SKIPPED cross_context_attach_leaves_worker_fd_census_flat: no KosmicKrisp ICD under \
             /Volumes/mesa-cs/build-kk (mount third_party/mesa-cs.sparseimage and ninja)"
        );
        return;
    }
    let cfg = match GuestConfig::seated_fedora_from_env() {
        Ok(cfg) => cfg.with_coexist_display(1280, 800).with_net(),
        Err(e) => {
            eprintln!("SKIPPED cross_context_attach_leaves_worker_fd_census_flat: {e}");
            return;
        }
    };
    eprintln!(
        "booting the seated enhanced venus desktop (coexist + NAT) via {:?}",
        cfg.limina_bin
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(240))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");

    // Let the seated session finish coming up before the baseline census: gnome-shell on
    // venus allocates its own carriers at startup, and those must not land BETWEEN the
    // two censuses.
    guest
        .ssh_poll("pgrep -x gnome-shell >/dev/null", Duration::from_secs(180))
        .expect("gnome-shell never appeared — the seated enhanced session didn't come up");

    // Stage the dependency-free cycler (pure python3 + libvulkan, nothing to install).
    guest
        .ssh_exec(&format!(
            "cat > /tmp/vkfdcycle.py <<'VKFDCYCLE_PY_EOF'\n{VKFDCYCLE}\nVKFDCYCLE_PY_EOF"
        ))
        .expect("staging vkfdcycle.py in the guest");

    let worker = guest.worker_pid().expect("finding the limina-vmm worker");

    // Baseline once the census stops moving: gnome-shell (a venus client here) holds a
    // steady carrier set on an idle desktop, so a settled reading isolates the cycler's
    // churn as the only delta between the two censuses.
    let baseline = settled_census(worker, Duration::from_secs(30));
    eprintln!("baseline carrier-fd census: {baseline} (worker pid {worker})");

    let out = guest
        .ssh_exec(&format!(
            "VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json \
             timeout 180 python3 /tmp/vkfdcycle.py Venus {CYCLES} 2>&1 || true"
        ))
        .expect("running vkfdcycle in the guest");
    eprintln!("--- vkfdcycle ---\n{out}");
    assert!(
        out.contains(&format!("FDCYCLE PASS {CYCLES}")),
        "the guest cycler did not complete {CYCLES} cross-context export/import cycles — \
         the census below would be vacuous.\n{out}"
    );

    // The decisive check: after the cycler exits (all its memory freed, both venus
    // contexts destroyed), the worker's carrier-fd census must return to baseline.
    // Give host-side teardown time to settle, then poll up to the deadline.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut census = carrier_fd_census(worker);
    while census != baseline && Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(1));
        census = carrier_fd_census(worker);
    }
    eprintln!("post-run carrier-fd census: {census}");
    assert_eq!(
        census,
        baseline,
        "the worker leaked {} vkr-metal-mem carrier fds across {CYCLES} cross-context \
         attach cycles — an EMFILE ratchet of the virgl-0032 class (an SCM_RIGHTS-received \
         carrier fd not consumed by the venus import path)",
        census as i64 - baseline as i64
    );

    let outcome = guest
        .shutdown(Duration::from_secs(10))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}
