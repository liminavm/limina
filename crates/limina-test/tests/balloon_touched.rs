// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! A balloon inflated over memory the guest has touched and freed gives it back to the host.
//!
//! On owned-block guest RAM (docs/design/guest-ram-objects.md) the host gets pages back only by
//! whole block, and a stock 4 KiB guest's balloon takes single pages. The guest's free lists
//! hold a large free region as long runs, though, and the balloon takes its pages from them, so
//! blocks complete: measured 2026-10-22, 2004 of 2048 MiB came back on 2 MiB blocks and 2049 MiB
//! on anonymous RAM. Run it both ways with `LIMINA_GUEST_RAM=objects|anon`.

use std::time::{Duration, Instant};

use limina_test::{Guest, GuestConfig};

const MIB: u64 = 1 << 20;
const TOUCHED: u64 = 3072 * MIB;
const TARGET: u64 = 2048 * MIB;

fn mib(bytes: u64) -> u64 {
    bytes / MIB
}

#[test]
fn a_balloon_inflated_over_touched_memory_returns_it() {
    if !limina_test::require_hvf_or_skip("a_balloon_inflated_over_touched_memory_returns_it") {
        return;
    }
    let cfg = match GuestConfig::baseline_fedora_from_env() {
        Ok(mut cfg) => {
            cfg.ram_mib = 6144;
            cfg.with_net().with_balloon_control()
        }
        Err(e) => {
            eprintln!("SKIPPED a_balloon_inflated_over_touched_memory_returns_it: {e}");
            return;
        }
    };
    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh(Duration::from_secs(300))
        .expect("guest sshd never became reachable through the forward");

    guest
        .ssh_exec(&format!(
            "sudo sh -c 'dd if=/dev/zero of=/dev/shm/touch bs=1M count={} 2>/dev/null; \
             rm /dev/shm/touch' && echo touched",
            mib(TOUCHED)
        ))
        .expect("touching guest memory");
    let fp0 = guest
        .worker_phys_footprint()
        .expect("reading worker footprint");

    guest
        .set_balloon_target(TARGET)
        .expect("setting the balloon target");
    let deadline = Instant::now() + Duration::from_secs(60);
    let inflated = loop {
        let actual = guest.balloon_stats().expect("balloon stats").actual;
        if actual >= TARGET * 9 / 10 || Instant::now() > deadline {
            break actual;
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    std::thread::sleep(Duration::from_secs(2));
    let fp1 = guest
        .worker_phys_footprint()
        .expect("reading worker footprint");
    let returned = fp0.saturating_sub(fp1);
    eprintln!(
        "balloon inflated {} MiB over touched memory; worker footprint {} -> {} MiB: {} MiB \
         returned",
        mib(inflated),
        mib(fp0),
        mib(fp1),
        mib(returned)
    );
    assert!(inflated >= TARGET * 9 / 10, "the balloon did not inflate");
    assert!(
        returned >= inflated * 3 / 4,
        "the host got {} MiB back for {} MiB the balloon took over touched memory",
        mib(returned),
        mib(inflated)
    );

    guest
        .shutdown(Duration::from_secs(15))
        .expect("shutting down the guest");
}
