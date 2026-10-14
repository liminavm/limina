// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The enhanced guest's vblank falls on the host's vsync grid, a lead ahead of it.
//!
//! The supervisor states a display's grid with `vsync id=… at=… period=… lead=…` on the
//! display-control socket; the worker restates it in guest counter time on the GPU extension's
//! timing page, and `virtio-gpu-limina` aims each tick of the CRTC's vblank timer at it
//! (docs/design/host-vblank.md). This pushes a grid of its own — an odd period, so a timer left at
//! the mode's rate cannot land on it by accident — and reads where the guest's vblanks fall.
//!
//! Oracles, all guest-side and none of them the module's own claim:
//! - the vblank timestamps userspace gets from `DRM_IOCTL_WAIT_VBLANK` (the ones flip-done events
//!   carry, `guest/vblankgrid.py`), mapped to the guest counter through CLOCK_MONOTONIC_RAW (see
//!   [`vblank_ticks`]), sit on `at - lead` modulo the period;
//! - the page's anchor is exactly `at - lead - vtimer offset`, and the guest's counter is the
//!   host's `mach_absolute_time` (offset 0 on a fresh boot), read across an ssh round trip;
//! - a new grid moves the vblanks, and `off` hands the CRTC back to the stock timer, which keeps
//!   ticking.

use std::time::Duration;

use limina_test::{DisplayCommand, Guest, GuestConfig};

const VBLANKGRID: &str = include_str!("../guest/vblankgrid.py");

/// 59.95 Hz on the 24 MHz counter: 400 333.6 ticks, so the fraction matters over a few frames.
const PERIOD_FP: u64 = ((24_000_000u128 << 32) * 100 / 5_995) as u64;
const LEAD: u64 = 96_000; // 4 ms
/// How far a vblank may sit from the grid: the module states its timestamps from the grid itself,
/// so anything past rounding means it is not on it.
const TOLERANCE_TICKS: i64 = 24 * 50; // 50 µs
const SAMPLES: usize = 30;

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
}

fn host_ticks() -> u64 {
    // SAFETY: no arguments, no state.
    unsafe { mach_absolute_time() }
}

#[derive(Debug, Default)]
struct Timing {
    counter: u64,
    ktime_ns: u64,
    raw_ns: u64,
    rate: u64,
    flags: u32,
    anchor: u64,
    period: u64,
    aimed: u64,
    stock: u64,
    next_tick: u64,
}

fn field(line: &str, key: &str) -> Option<u64> {
    line.split_whitespace()
        .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('=')?.parse().ok())
}

/// The module's debugfs view of scanout 0, and the (counter, ktime) pair it was read at.
fn read_timing(g: &Guest) -> Timing {
    let out = g
        .ssh_exec("sudo sh -c 'cat /sys/kernel/debug/dri/*/limina_timing'")
        .expect("reading limina_timing (is the module's timing support loaded?)");
    let mut t = Timing::default();
    for line in out.lines() {
        if line.starts_with("clock ") {
            t.counter = field(line, "counter").expect("clock counter");
            t.ktime_ns = field(line, "ktime_ns").expect("clock ktime_ns");
            t.raw_ns = field(line, "raw_ns").expect("clock raw_ns");
            t.rate = field(line, "rate").expect("clock rate");
        } else if line.starts_with("scanout 0 ") {
            t.flags = field(line, "flags").expect("flags") as u32;
            t.anchor = field(line, "anchor").expect("anchor");
            t.period = field(line, "period").expect("period");
            t.aimed = field(line, "aimed").expect("aimed");
            t.stock = field(line, "stock").expect("stock");
            t.next_tick = field(line, "next_tick").expect("next_tick");
        }
    }
    assert!(t.rate > 0, "no clock line in limina_timing: {out}");
    t
}

/// Vblank timestamps of CRTC 0, in guest counter ticks. They are CLOCK_MONOTONIC, which NTP slews
/// against the counter (chrony at up to 1/12, and changing its rate as it goes, while it pulls a
/// fresh guest's clock in). CLOCK_MONOTONIC_RAW is not slewed: each stamp is moved onto it by the
/// (MONOTONIC, RAW) pair read just after its vblank, and from it onto the counter by the module's
/// (counter, RAW) pair.
fn vblank_ticks(g: &Guest) -> Vec<u64> {
    let t = read_timing(g);
    let out = g
        .ssh_exec(&format!(
            "python3 /tmp/vblankgrid.py /dev/dri/card0 0 {SAMPLES}"
        ))
        .expect("waiting for vblanks");
    let samples: Vec<(i128, i128, i128)> = out
        .lines()
        .filter_map(|l| {
            Some((
                field(l, "t_ns")? as i128,
                field(l, "mono_ns")? as i128,
                field(l, "raw_ns")? as i128,
            ))
        })
        .collect();
    let ticks: Vec<u64> = (0..samples.len())
        .map(|i| {
            let (stamp, mono, raw) = samples[i];
            // The stamp lies the script's wake-up latency before the pair, a distance in slewed
            // MONOTONIC: scaled to RAW by the rate the two moved at over the interval holding it,
            // since the previous pair.
            let (a, b) = (
                samples[i.saturating_sub(1)],
                samples[i.max(1).min(samples.len() - 1)],
            );
            let (d_mono, d_raw) = (b.1 - a.1, b.2 - a.2);
            let gap = if d_mono > 0 {
                (mono - stamp) * d_raw / d_mono
            } else {
                mono - stamp
            };
            let raw_stamp = raw - gap;
            (t.counter as i128 + (raw_stamp - t.raw_ns as i128) * t.rate as i128 / 1_000_000_000)
                as u64
        })
        .collect();
    let latency_us: Vec<i128> = samples.iter().map(|s| (s.1 - s.0) / 1000).collect();
    eprintln!("wake latency (µs): {latency_us:?}");
    let gaps: Vec<i64> = ticks.windows(2).map(|w| (w[1] - w[0]) as i64).collect();
    eprintln!("vblank spacing (ticks): {gaps:?}");
    ticks
}

/// How far each tick is from the nearest point of the grid through `anchor`, in ticks.
fn phase_errors(ticks: &[u64], anchor: u64) -> Vec<i64> {
    ticks
        .iter()
        .map(|&c| {
            let rel = (c.wrapping_sub(anchor) as i64 as i128) << 32;
            let p = PERIOD_FP as i128;
            let r = rel.rem_euclid(p);
            let r = if r > p / 2 { r - p } else { r };
            (r >> 32) as i64
        })
        .collect()
}

fn push(g: &Guest, at: u64) {
    g.send_display_command(&DisplayCommand::Vsync {
        display_id: 0,
        grid: Some(limina_test::VsyncGrid {
            at,
            period_fp: PERIOD_FP,
            lead: LEAD,
            lead_ns: 4_000_000,
        }),
    })
    .expect("sending the vsync grid");
    std::thread::sleep(Duration::from_secs(1));
}

#[test]
fn the_guest_vblank_falls_on_the_host_grid() {
    const TEST: &str = "the_guest_vblank_falls_on_the_host_grid";
    if !limina_test::require_hvf_or_skip(TEST) {
        return;
    }
    let cfg = match GuestConfig::metal_efi_fedora_from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("SKIPPED {TEST}: {e}");
            return;
        }
    };
    // The supervisor's vsync source must not restate a window's screen grid over ours. The PNG sink
    // has no window for it to measure today; this keeps it so if the harness ever gets one.
    let cfg = cfg
        .with_coexist_display(1280, 800)
        .with_net()
        .with_supervisor_log()
        .with_env("LIMINA_VSYNC_SOURCE", "0");
    let mut g = Guest::boot(&cfg).expect("spawning the limina supervisor");
    g.wait_for_ssh(Duration::from_secs(240))
        .expect("guest sshd never became reachable");
    g.ssh_poll("pgrep -x gnome-shell >/dev/null", Duration::from_secs(180))
        .expect("gnome-shell never appeared");
    g.ssh_exec(&format!(
        "cat > /tmp/vblankgrid.py <<'VBLANKGRID_PY_EOF'\n{VBLANKGRID}\nVBLANKGRID_PY_EOF"
    ))
    .expect("writing vblankgrid.py to the guest");

    // --- A grid: the vblanks go on it, a lead ahead of the host vsync ---
    let at = host_ticks();
    push(&g, at);
    let before = host_ticks();
    let t = read_timing(&g);
    let after = host_ticks();
    eprintln!("timing: {t:?}");
    assert_eq!(t.flags & 1, 1, "scanout 0 has no grid on the page");
    assert_eq!(
        t.anchor,
        at - LEAD,
        "the anchor is not at - lead in guest counter time"
    );
    assert_eq!(t.period, PERIOD_FP);
    assert!(
        (before.saturating_sub(24_000_000)..after).contains(&t.counter),
        "the guest counter {} is not the host's mach time ({before}..{after}): a vtimer offset \
         the page did not account for",
        t.counter
    );
    let ticks = vblank_ticks(&g);
    assert_eq!(ticks.len(), SAMPLES, "vblanks stopped arriving");
    let errors = phase_errors(&ticks, t.anchor);
    eprintln!("phase errors (ticks): {errors:?}");
    assert!(
        errors.iter().all(|e| e.abs() <= TOLERANCE_TICKS),
        "vblanks off the grid: {errors:?}"
    );
    let aimed = read_timing(&g).aimed;
    assert!(
        aimed > t.aimed,
        "the module aimed no ticks while vblanks ran"
    );

    // --- A new phase: they follow it ---
    let shifted = at + (PERIOD_FP >> 32) / 3;
    push(&g, shifted);
    let t = read_timing(&g);
    assert_eq!(t.anchor, shifted - LEAD);
    let errors = phase_errors(&vblank_ticks(&g), t.anchor);
    eprintln!("phase errors after the shift (ticks): {errors:?}");
    eprintln!("timing after the shift: {:?}", read_timing(&g));
    assert!(
        errors.iter().all(|e| e.abs() <= TOLERANCE_TICKS),
        "vblanks did not follow the new grid: {errors:?}"
    );

    // --- Withdrawn: the stock timer takes over and keeps ticking ---
    g.send_display_command(&DisplayCommand::Vsync {
        display_id: 0,
        grid: None,
    })
    .expect("withdrawing the grid");
    std::thread::sleep(Duration::from_secs(1));
    let t = read_timing(&g);
    assert_eq!(t.flags & 1, 0, "the grid is still on the page");
    let aimed = t.aimed;
    assert_eq!(
        vblank_ticks(&g).len(),
        SAMPLES,
        "vblanks stopped without a grid"
    );
    assert_eq!(
        read_timing(&g).aimed,
        aimed,
        "the module kept aiming ticks without a grid"
    );

    let _ = g.shutdown(Duration::from_secs(30));
}
