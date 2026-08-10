// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Allocation-burst guard for the `DEFLATE_ON_OOM` drop (M6 addendum).
//!
//! Dropping `VIRTIO_BALLOON_F_DEFLATE_ON_OOM` (transparent balloon accounting) removes
//! the guest kernel's last-resort synchronous deflate at OOM; the supervisor's PSI
//! policy becomes the only pressure response. The case that could outrun it is a fast
//! multi-GiB anonymous allocation against a balloon inflated near `min`. This test IS
//! that case, end-to-end: inflate the balloon via the policy (harness plays the agent,
//! as in `balloon_psi.rs`), then run a C-speed multi-GiB memset burst in the guest while
//! relaying the guest's REAL `/proc/pressure/memory` + `/proc/meminfo` to the policy at
//! the agent's cadence — and require the burst to complete with **no OOM kill**.
//!
//! Pre-drop (bit advertised) this passes via the kernel's OOM notifier; post-drop it
//! passes only if the policy's release path (unconditional deflate on high pressure +
//! the MemAvailable-floor starvation release) outruns the burst. That is exactly the
//! guarantee the addendum requires before the bit is dropped.
//!
//! SKIPs cleanly without `LIMINA_HVF_TESTS`, the GOP firmware, or the baseline disk.

use std::time::{Duration, Instant};

use limina_proto::{Hello, MemPressure, Message};
use limina_test::{AgentConn, Guest, GuestConfig};

const MIB: u64 = 1 << 20;
const MIN_MIB: usize = 2048;
const MAX_MIB: usize = 6144;
/// The balloon must be inflated to at least this before the burst means anything: near the
/// policy's own cap (max - min = 4096 MiB), so effective guest memory sits near `min` and the
/// 3 GiB burst CANNOT fit without the release path. A softer floor once let the burst complete
/// into ~3.6 GiB of still-free RAM — green without exercising anything.
const INFLATE_FLOOR: u64 = 3800 * MIB;
/// The burst: 12 × 256 MiB = 3 GiB of touched anonymous memory.
const BURST_CHUNKS: usize = 12;

fn mib(bytes: u64) -> u64 {
    bytes / MIB
}

/// An idle, memory-rich report (synthetic): drives the policy to inflate.
fn idle_report() -> MemPressure {
    let total = (MAX_MIB as u64) * 1024;
    MemPressure {
        some_avg10: 0,
        some_avg60: 0,
        full_avg10: 0,
        full_avg60: 0,
        mem_available_kib: total * 70 / 100,
        mem_total_kib: total,
    }
}

/// Read the guest's REAL pressure + meminfo, as the agent would report them.
/// `avg10=1.23` parses to `123` (percent × 100 — the `MemPressure` scale).
fn real_report(guest: &Guest) -> Option<MemPressure> {
    let out = guest
        .ssh_exec(
            "cat /proc/pressure/memory; awk '/MemTotal|MemAvailable/{print $1, $2}' /proc/meminfo",
        )
        .ok()?;
    let pct100 = |line_tag: &str, field: &str| -> u32 {
        out.lines()
            .find(|l| l.starts_with(line_tag))
            .and_then(|l| l.split(&format!("{field}=")).nth(1))
            .and_then(|r| r.split_whitespace().next())
            .and_then(|v| v.parse::<f64>().ok())
            .map(|f| (f * 100.0) as u32)
            .unwrap_or(0)
    };
    let mem_kib = |tag: &str| -> u64 {
        out.lines()
            .find(|l| l.starts_with(tag))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    Some(MemPressure {
        some_avg10: pct100("some", "avg10"),
        some_avg60: pct100("some", "avg60"),
        full_avg10: pct100("full", "avg10"),
        full_avg60: pct100("full", "avg60"),
        mem_available_kib: mem_kib("MemAvailable:"),
        mem_total_kib: mem_kib("MemTotal:"),
    })
}

#[test]
fn allocation_burst_survives_inflated_balloon() {
    if !limina_test::require_hvf_or_skip("allocation_burst_survives_inflated_balloon") {
        return;
    }

    let cfg = match GuestConfig::baseline_fedora_from_env() {
        Ok(cfg) => cfg
            .with_net()
            .with_memory(MIN_MIB, MAX_MIB)
            .with_balloon_control()
            .with_control_socket(),
        Err(e) => {
            eprintln!("SKIPPED allocation_burst_survives_inflated_balloon: {e}");
            return;
        }
    };
    eprintln!("booting stock 4 KiB F44 baseline ({MIN_MIB}..{MAX_MIB} MiB, PSI autoballoon)");

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = guest
        .wait_for_ssh_banner(Duration::from_secs(300))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");

    let mut conn: AgentConn = guest
        .connect_control(Duration::from_secs(30))
        .expect("connecting to the control plane");
    conn.send(&Message::Hello(Hello {
        agent: "limina-test-burst/0".to_string(),
        caps: vec!["mempressure".to_string()],
        pagesize: 4096,
    }))
    .expect("sending Hello");
    match conn.recv(Duration::from_secs(10)) {
        Ok((_, Message::Welcome(_))) => {}
        other => panic!("expected Welcome from the control plane, got {other:?}"),
    }

    // Inflate toward the floor with synthetic idle reports (the balloon_psi drive).
    eprintln!("inflating the balloon via idle pressure reports");
    // ~32 steps of 256 MiB at one per ~2s, plus boot settle: give it room.
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut inflated = 0;
    while Instant::now() < deadline {
        conn.send(&Message::MemPressure(idle_report()))
            .expect("sending idle MemPressure");
        std::thread::sleep(Duration::from_millis(1200));
        let actual = guest.balloon_stats().expect("reading balloon stats").actual;
        inflated = actual;
        if actual >= INFLATE_FLOOR {
            break;
        }
    }
    assert!(
        inflated >= INFLATE_FLOOR,
        "could not inflate the balloon to the test floor: actual={} MiB, want >= {} MiB \
         (the burst means nothing against an empty balloon)",
        mib(inflated),
        mib(INFLATE_FLOOR)
    );
    eprintln!("balloon inflated: actual={} MiB", mib(inflated));

    // Journal watermark: only OOM kills AFTER this instant count.
    let since = guest
        .ssh_exec("date +%s")
        .expect("reading the guest clock")
        .trim()
        .to_string();

    // THE BURST: C-speed multi-GiB anonymous allocation (ctypes.memset touches every
    // page), started detached so the harness can relay real pressure reports while it
    // runs. The verdict lands in /tmp/burst.out.
    // NOTE the explicit "\n    " indentation: a `\`-continued Rust string literal strips the
    // next line's leading whitespace, which once flattened the loop body to column 0 and made
    // the staged script die instantly with IndentationError (the burst silently never ran).
    let burst_py = format!(
        "import ctypes\nchunks = []\nCH = 256 * 1024 * 1024\n\
         for i in range({BURST_CHUNKS}):\n\
         \x20   b = ctypes.create_string_buffer(CH)\n\
         \x20   ctypes.memset(b, 1, CH)\n\
         \x20   chunks.append(b)\n\
         \x20   print('chunk', i + 1, flush=True)\n\
         print('BURST-OK', flush=True)\n"
    );
    guest
        .ssh_exec(&format!(
            "cat > /tmp/burst.py <<'BURST_PY_EOF'\n{burst_py}\nBURST_PY_EOF"
        ))
        .expect("staging the burst allocator");
    guest
        .ssh_exec(
            "rm -f /tmp/burst.out; \
             setsid nohup python3 /tmp/burst.py </dev/null >/tmp/burst.out 2>&1 & \
             echo spawned",
        )
        .expect("spawning the burst allocator");

    // Relay the guest's REAL pressure to the policy at the agent's cadence while the
    // burst runs — this is the production release loop, played faithfully.
    eprintln!("burst running; relaying real pressure reports");
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut verdict = String::new();
    while Instant::now() < deadline {
        if let Some(report) = real_report(&guest) {
            eprintln!(
                "  relay: some_avg10={} avail={} MiB total={} MiB",
                report.some_avg10,
                report.mem_available_kib / 1024,
                report.mem_total_kib / 1024
            );
            let _ = conn.send(&Message::MemPressure(report));
        }
        let out = guest
            .ssh_exec("cat /tmp/burst.out 2>/dev/null")
            .unwrap_or_default();
        if out.contains("BURST-OK") {
            verdict = "ok".into();
            break;
        }
        // The allocator process vanished without its verdict = it was killed. The bracketed
        // pattern keeps pgrep from matching the ssh-spawned shell whose own command line
        // carries the pattern (the self-match once made a dead allocator look alive forever).
        let alive = guest
            .ssh_exec("pgrep -f '[b]urst.py' | head -1")
            .unwrap_or_default();
        if alive.trim().is_empty() && !out.contains("BURST-OK") {
            verdict = "died".into();
            break;
        }
        std::thread::sleep(Duration::from_millis(1000));
    }

    let burst_out = guest
        .ssh_exec("cat /tmp/burst.out 2>/dev/null")
        .unwrap_or_default();
    let oom = guest
        .ssh_exec(&format!(
            "sudo journalctl -k --since=@{since} 2>/dev/null | grep -ci 'out of memory\\|oom-kill' || true"
        ))
        .unwrap_or_default();
    let actual_after = guest
        .balloon_stats()
        .expect("post-burst balloon stats")
        .actual;
    eprintln!(
        "post-burst: verdict={verdict} oom-lines={} balloon actual={} MiB\n--- burst.out ---\n{burst_out}",
        oom.trim(),
        mib(actual_after)
    );

    assert_eq!(
        verdict, "ok",
        "the {BURST_CHUNKS}x256 MiB allocation burst did not complete against the inflated \
         balloon (verdict={verdict}) — the release path (policy deflate / OOM net) was outrun \
         \n--- burst.out ---\n{burst_out}"
    );
    assert_eq!(
        oom.trim(),
        "0",
        "the kernel OOM killer fired during the burst — the release path is too slow \
         \n--- burst.out ---\n{burst_out}"
    );

    let outcome = guest
        .shutdown(Duration::from_secs(15))
        .expect("shutting down the guest");
    eprintln!("teardown outcome: {outcome:?}");
}
