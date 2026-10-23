// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 — **dynamic memory on a stock guest**, sensed through the stock `qemu-guest-agent`.
//!
//! A stock guest has no `limina-agent` to report memory pressure, so the policy used to make
//! no decision at all there. The supervisor now reads the guest's `/proc/meminfo`,
//! `/proc/vmstat` and PSI files through the agent's `guest-file-*` verbs and feeds the same
//! policy the enhanced tier drives.
//!
//! Oracles:
//! 1. **The supervisor read the guest**, and says which files it could: meminfo and vmstat
//!    always; PSI only where the guest lets the agent read it. Enforcing Fedora gives
//!    `virt_qemu_ga_t` no read on `proc_psi_t`, so there it must say PSI is unreadable.
//! 2. **The policy acted on it**: an idle stock desktop with room to spare gets a balloon
//!    target. Before this sensor existed the target stayed at 0 for the life of the VM.
//! 3. **PSI is picked up when the guest allows it**: switching the guest to permissive makes
//!    the next poll read PSI, with no restart of anything.

use std::time::{Duration, Instant};

use limina_test::{Guest, GuestConfig};

const MIN_MIB: usize = 2048;
const MAX_MIB: usize = 6144;

/// The memory-reading line's prefix (`control::Inner::qga_mem_tick`).
const READING: &str = "qga: the stock guest's memory reading: meminfo read, vmstat read, PSI ";

#[test]
fn a_stock_guest_feeds_the_balloon_policy_through_its_guest_agent() {
    let name = "a_stock_guest_feeds_the_balloon_policy_through_its_guest_agent";
    if !limina_test::require_hvf_or_skip(name) {
        return;
    }
    let cfg = match GuestConfig::baseline_fedora_from_env() {
        Ok(cfg) => cfg
            .with_net()
            .with_memory(MIN_MIB, MAX_MIB)
            .with_balloon_control()
            .with_supervisor_log(),
        Err(e) => {
            eprintln!("SKIPPED {name}: {e}");
            return;
        }
    };
    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    guest
        .wait_for_ssh(Duration::from_secs(300))
        .expect("guest sshd never became reachable through the forward");
    let mode = guest
        .ssh_exec("getenforce")
        .expect("asking the guest for its SELinux mode")
        .trim()
        .to_string();

    // --- Oracle 1: the reading, and what it could read ---
    let expect = if mode == "Enforcing" {
        "unreadable"
    } else {
        "read"
    };
    guest
        .wait_for_supervisor_log(&format!("{READING}{expect}"), Duration::from_secs(120))
        .unwrap_or_else(|e| {
            panic!(
                "no stock-tier memory reading with PSI {expect} on a {mode} guest ({e}); \
                 qga lines: {}",
                qga_lines(&guest)
            )
        });

    // --- Oracle 2: the policy acted on it ---
    let deadline = Instant::now() + Duration::from_secs(300);
    let mut target = 0;
    while Instant::now() < deadline {
        target = guest
            .balloon_stats()
            .expect("reading the worker's stats")
            .target;
        if target > 0 {
            break;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    assert!(
        target > 0,
        "an idle stock desktop at {MIN_MIB}..{MAX_MIB} MiB never got a balloon target: the \
         policy is not being fed. qga lines: {}",
        qga_lines(&guest)
    );
    eprintln!("the policy set a {} MiB target", target >> 20);

    // --- Oracle 3: PSI is picked up as soon as the guest allows it ---
    if mode == "Enforcing" {
        guest
            .ssh_exec("sudo setenforce 0")
            .expect("switching the guest to permissive");
        guest
            .wait_for_supervisor_log(&format!("{READING}read"), Duration::from_secs(30))
            .unwrap_or_else(|e| {
                panic!(
                    "a permissive guest's PSI was never read ({e}); qga lines: {}",
                    qga_lines(&guest)
                )
            });
    } else {
        eprintln!("the guest is {mode}; PSI was readable from the start");
    }
    let _ = guest.shutdown(Duration::from_secs(30));
}

/// Everything the supervisor said about the guest agent, for a failure message.
fn qga_lines(guest: &Guest) -> String {
    let log = guest.supervisor_log();
    let lines: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("qga") || l.contains("balloon"))
        .collect();
    format!("\n{}", lines.join("\n"))
}
