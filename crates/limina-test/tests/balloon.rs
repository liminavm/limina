// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! M6 dynamic-memory baseline: a stock 4 KiB Fedora guest's **free-page reporting** actually
//! returns memory to macOS — the worker's `phys_footprint` rises when the guest faults pages in and
//! DROPS again after the guest frees them.
//!
//! This is the gating end-to-end test for M6 Step 1 (libkrun patch 0033): `process_frq` reclaims
//! reported-free guest pages with `MADV_FREE_REUSABLE` (coalesced to whole 16 KiB host pages),
//! replacing the shipped `MADV_DONTNEED` which — spike-proven on macOS 26.5
//! (`spikes/balloon-madvise/RESULTS.md`) — returns *nothing*. The balloon
//! device is attached unconditionally by libkrun, so this needs no new CLI: a stock guest's own
//! `page_reporting` drives the FRQ and the fix does the rest.
//!
//! Vehicle: the stock 4 KiB autologin baseline (`Fedora-Workstation-44.boot.raw`) — its kernel
//! ships `VIRTIO_BALLOON` + `PAGE_REPORTING`, and it *is* the two-tier compatibility floor M6 must
//! keep working. Headless (no display) + NAT for SSH; we only need a shell to fault and free memory.
//!
//! The single run is self-validating against the "differential didn't reach the system under test"
//! trap (CLAUDE.md): we assert the footprint RISE on allocation (proves the worker's MAP_ANON and
//! our measurement track guest faults) AND the DROP after free (proves reclaim). With the old
//! `MADV_DONTNEED` the rise holds and the drop never comes — exactly the RED this fix turns GREEN.
//!
//! SKIPs cleanly without `LIMINA_HVF_TESTS`, the GOP firmware, or the baseline disk. Heavy: a full
//! stock boot to sshd. Gated behind `LIMINA_HVF_TESTS`.

use std::time::{Duration, Instant};

use limina_test::{Guest, GuestConfig};

const MIB: u64 = 1 << 20;

/// How much anonymous memory the guest faults in. The baseline boots with 6 GiB here and idles
/// well under 1 GiB headless, so 2 GiB is safe and gives an unmistakable signal.
const ALLOC_BYTES: u64 = 2 * 1024 * MIB;
/// The worker footprint must rise by at least this much while the guest holds the allocation —
/// generous vs the ~2 GiB faulted, to tolerate measurement/idle noise.
const RISE_MIN: u64 = 1400 * MIB;
/// After the guest frees the allocation, the worker footprint must fall by at least this much from
/// its peak. Old `MADV_DONTNEED` returns 0; the fix returns ~all of the 2 GiB, so 700 MiB is a
/// large, unambiguous floor that still tolerates lazy/partial page-reporting cadence.
const RECLAIM_MIN: u64 = 700 * MIB;

fn mib(bytes: u64) -> u64 {
    bytes / MIB
}

#[test]
fn free_page_reporting_returns_memory_to_the_host() {
    if !limina_test::require_hvf_or_skip("free_page_reporting_returns_memory_to_the_host") {
        return;
    }

    let cfg = match GuestConfig::baseline_fedora_from_env() {
        Ok(mut cfg) => {
            cfg.ram_mib = 6144; // headroom for a 2 GiB anonymous allocation
                                // F_REPORTING is masked by default since the M9.2 s2idle work (a stock guest that
                                // negotiates it crashes on suspend — upstream virtballoon_freeze bug). This test
                                // exercises the FRQ reclaim *mechanism* and never suspends, so opt in explicitly.
            cfg.with_net()
                .with_supervisor_arg("--balloon-free-page-reporting")
        }
        Err(e) => {
            eprintln!("SKIPPED free_page_reporting_returns_memory_to_the_host: {e}");
            return;
        }
    };
    eprintln!(
        "booting stock 4 KiB F44 baseline (headless, NAT) with a {} MiB balloon-reclaim probe",
        mib(ALLOC_BYTES)
    );

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");

    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");

    // Baseline footprint of the worker (it owns the guest-RAM MAP_ANON).
    let f0 = guest
        .worker_phys_footprint()
        .expect("reading worker footprint");
    eprintln!("worker phys_footprint baseline: {} MiB", mib(f0));

    // A self-contained allocator: fault `ALLOC_BYTES` of anonymous memory in 1 MiB chunks, then hold
    // it resident for 60 s so we can observe the rise, then exit (which frees it).
    let alloc_py = "import mmap,sys,time\n\
        n=int(sys.argv[1]); hold=int(sys.argv[2])\n\
        b=mmap.mmap(-1,n)\n\
        chunk=1<<20; buf=b'\\xab'*chunk\n\
        o=0\n\
        while o<n:\n    b[o:o+chunk]=buf; o+=chunk\n\
        time.sleep(hold)\n";
    guest
        .ssh_exec(&format!(
            "cat > /tmp/alloc.py <<'PY'\n{alloc_py}PY\necho wrote"
        ))
        .expect("writing the guest allocator");

    // Launch it fully detached: all std fds redirected so the SSH channel can close (a backgrounded
    // job that inherits ssh's stdout would hang ssh — the m3-networking lesson). Echo the bg PID so
    // we can kill it by pid below — `pkill -f /tmp/alloc.py` would self-match (the killer's own
    // command line contains that string) and suicide the SSH session.
    let pid = guest
        .ssh_exec(&format!(
            "nohup python3 /tmp/alloc.py {ALLOC_BYTES} 60 >/dev/null 2>&1 </dev/null & echo $!"
        ))
        .expect("launching the guest allocator");
    let pid = pid.trim().to_string();

    // Let it fault all pages in (HVF demand-pages them into the worker's MAP_ANON), then measure the
    // peak. The rise proves the differential reaches the system under test.
    std::thread::sleep(Duration::from_secs(15));
    let f1 = guest
        .worker_phys_footprint()
        .expect("reading worker footprint");
    eprintln!(
        "worker phys_footprint after guest faulted {} MiB: {} MiB (+{} MiB)",
        mib(ALLOC_BYTES),
        mib(f1),
        mib(f1.saturating_sub(f0))
    );
    assert!(
        f1.saturating_sub(f0) >= RISE_MIN,
        "guest allocation did not raise the worker footprint (got +{} MiB, want >= {} MiB) — the \
         measurement/demand-paging path isn't reaching the worker, so a later 'no drop' would be \
         meaningless",
        mib(f1.saturating_sub(f0)),
        mib(RISE_MIN)
    );

    // Free it: kill the allocator by pid (munmap), then nudge compaction so the buddy allocator
    // surfaces the freed blocks for page-reporting promptly (reporting is otherwise lazy, ~2 s
    // cadence). Best-effort: the trailing `echo` keeps the remote exit 0 even if kill/sudo don't.
    guest
        .ssh_exec(&format!(
            "kill {pid} 2>/dev/null; sudo sh -c 'echo 1 > /proc/sys/vm/compact_memory' 2>/dev/null; echo freed"
        ))
        .expect("freeing the guest allocation");

    // Poll the worker footprint until it falls back. With the fix, page-reporting → process_frq →
    // MADV_FREE_REUSABLE returns the pages; without it (MADV_DONTNEED) the footprint stays at peak.
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut lowest = f1; // best (lowest) footprint seen; robust against the guest churning it back up
    loop {
        std::thread::sleep(Duration::from_secs(3));
        let cur = guest
            .worker_phys_footprint()
            .expect("reading worker footprint");
        lowest = lowest.min(cur);
        eprintln!(
            "  worker phys_footprint: {} MiB (reclaimed {} MiB of the {} MiB peak)",
            mib(cur),
            mib(f1.saturating_sub(lowest)),
            mib(f1.saturating_sub(f0))
        );
        if f1.saturating_sub(lowest) >= RECLAIM_MIN || Instant::now() >= deadline {
            break;
        }
    }
    let reclaimed = f1.saturating_sub(lowest);

    assert!(
        reclaimed >= RECLAIM_MIN,
        "the guest freed {} MiB but the worker footprint only fell {} MiB (want >= {} MiB) — \
         free-page reporting did NOT return memory to macOS. This is the RED state of the shipped \
         MADV_DONTNEED (returns nothing on macOS); patch 0033's MADV_FREE_REUSABLE turns it GREEN.",
        mib(ALLOC_BYTES),
        mib(reclaimed),
        mib(RECLAIM_MIN)
    );
    eprintln!(
        "RECLAIM CONFIRMED: worker footprint fell {} MiB after the guest freed {} MiB",
        mib(reclaimed),
        mib(ALLOC_BYTES)
    );

    let outcome = guest
        .shutdown(Duration::from_secs(15))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}
