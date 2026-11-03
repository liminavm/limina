// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The worker's memory trace: what its memory is, logged while the `memory-trace` lever is on.
//!
//! The lever is the supervisor's (the Debug menu, `limina debug <vm> lever memory-trace on`),
//! which forwards it here and starts every later worker with `LIMINA_MEMORY_TRACE=1` while it is
//! on. On, this process
//!
//! - runs the allocation census (`limina_debug::alloc_census`), sampling one Rust allocation in
//!   `LIMINA_MEMORY_TRACE_SAMPLE` (256 by default);
//! - logs a full report (`report`) at once, then a `summary` line every
//!   `LIMINA_MEMORY_TRACE_INTERVAL_S` (300 s by default) and a full report every
//!   [`FULL_EVERY`]th one, all at warn and prefixed `memory:`, so a dogfood log left with the
//!   lever on for days holds the growth curve and, hourly, where it went.
//!
//! `limina debug <vm> memory` gets one report on demand, lever on or off.
//!
//! None of it suspends the process: `vmmap` and friends do, and that freezes the guest.

use std::time::{Duration, Instant};

use limina_debug::alloc_census;
use limina_debug::lever::Lever;
use limina_debug::memstat;

pub static MEMORY_TRACE: Lever = Lever::new(
    "memory-trace",
    "LIMINA_MEMORY_TRACE",
    "memory: periodic reports of what holds each process's memory, and which Rust call sites \
     keep their allocations (sampled)",
);

/// A full report every this many summaries.
const FULL_EVERY: u32 = 12;

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&v| v > 0)
        .unwrap_or(default)
}

/// Everything this process reports: its memory, its census, and the GPU host's footprint.
pub fn report() -> Vec<String> {
    let mut lines = memstat::report();
    lines.extend(alloc_census::report());
    if let Some(pid) = crate::krun::metal_ext::gpu_host_pid() {
        lines.push(memstat::proc_line("gpu-host", pid));
    }
    lines
}

/// Turn the lever on or off; the census and the log follow within a second.
pub fn set(on: bool) {
    MEMORY_TRACE.set(on);
}

fn census(on: bool) {
    alloc_census::set_enabled(
        on,
        env_u64(
            "LIMINA_MEMORY_TRACE_SAMPLE",
            alloc_census::DEFAULT_SAMPLE_EVERY,
        ),
    );
}

fn log_lines(lines: &[String]) {
    for l in lines {
        log::warn!("memory: {l}");
    }
}

/// Start the thread that follows the lever. Seeded from `LIMINA_MEMORY_TRACE`, so a worker the
/// supervisor started with the lever on traces from its first second.
pub fn start() {
    let interval = Duration::from_secs(env_u64("LIMINA_MEMORY_TRACE_INTERVAL_S", 300));
    let spawned = std::thread::Builder::new()
        .name("memory-trace".into())
        .spawn(move || {
            let mut was_on = false;
            let mut next = Instant::now();
            let mut ticks = 0u32;
            loop {
                let on = MEMORY_TRACE.on();
                if on != was_on {
                    census(on);
                    if on {
                        log::warn!(
                            "memory: trace on (summary every {}s, full report every {})",
                            interval.as_secs(),
                            FULL_EVERY
                        );
                        log_lines(&report());
                        next = Instant::now() + interval;
                        ticks = 0;
                    } else {
                        log::warn!("memory: trace off");
                    }
                    was_on = on;
                } else if on && Instant::now() >= next {
                    ticks += 1;
                    if ticks.is_multiple_of(FULL_EVERY) {
                        log_lines(&report());
                    } else {
                        log_lines(&[memstat::summary(), alloc_census::report().remove(0)]);
                    }
                    next += interval;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        });
    if let Err(e) = spawned {
        log::error!("memory-trace: could not start its thread: {e}");
    }
}
