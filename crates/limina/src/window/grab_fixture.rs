// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Replay recorded `LIMINA_EDGE_TRACE` gestures through the pure grab policy.
//!
//! A dogfood round's trace is a list of timestamped samples, and the policy deliberately takes
//! `now` as data ([`grab_policy`]), so a recorded gesture can be re-run headlessly with an
//! asserted verdict: change a constant and the fixture says *which* gesture changed its mind,
//! with no rig time spent. The samples come from the trace lines the adapters already emit —
//! `[EDGE]` (one free-pointer motion event: position, fit, and the state flags after the step)
//! and `[GRAB]` (a press charging, the grab being taken, a release firing).
//!
//! Trace timestamps are stamped at *print* time, a few ms after the step they describe, so a
//! replay reproduces verdict timing to within an event or two, not to the millisecond: the
//! assertions here are behavioral (the grab fires at the recorded moment ± a few events, the
//! release goes out the recorded edge to the recorded point), plus the exact per-line flags
//! that are pure functions of the recorded values (`deep`, `latched`).

use std::time::{Duration, Instant};

use super::fit;
use super::grab_policy::{capture_tier, free_step, press_step, Free, GrabState, Press, Release};

/// One trace line this replay understands, in file order.
#[derive(Debug)]
enum Ev {
    /// `[EDGE]` — one uncaptured motion event, as `uncaptured_edges` sampled it.
    Edge {
        t: f64,
        cur: (f64, f64),
        fit: fit::FitRect,
        deep: bool,
        latched: bool,
    },
    /// `[GRAB] … grabbing:` — the free path took the pointer on the event just replayed.
    Grabbing { t: f64 },
    /// `[GRAB] … press <Edge> …` — one captured motion event charging an edge press.
    Press {
        t: f64,
        edge: fit::Edge,
        pos: (f64, f64),
        delta: (f64, f64),
        charge: f64,
        hold: f64,
    },
    /// `[GRAB] … releasing <Edge> … to view (x, y)` — the press earned its release.
    Releasing { t: f64, target: (f64, f64) },
}

/// The float starting at `s` (an optional sign, digits, an optional fraction).
fn num(s: &str) -> f64 {
    let end = s
        .char_indices()
        .find(|(i, c)| !(c.is_ascii_digit() || *c == '.' || (*i == 0 && *c == '-')))
        .map_or(s.len(), |(i, _)| i);
    s[..end].parse().expect("malformed number in trace line")
}

/// The text following `key` on `line`, up to the end of the line.
fn after<'a>(line: &'a str, key: &str) -> &'a str {
    let i = line.find(key).expect("missing key in trace line");
    &line[i + key.len()..]
}

/// A `(x,y)` or `(x, y)` pair starting at `s` (which must begin with the `(`).
fn pair(s: &str) -> (f64, f64) {
    let s = s.strip_prefix('(').expect("expected a ( pair");
    let comma = s.find(',').expect("expected , in pair");
    (num(s), num(s[comma + 1..].trim_start()))
}

/// A `(x,y wxh)` fit rect starting at `s`.
fn fit_rect(s: &str) -> fit::FitRect {
    let s = s.strip_prefix('(').expect("expected a ( fit");
    let comma = s.find(',').expect("expected , in fit");
    let space = s.find(' ').expect("expected space in fit");
    let x_mark = s[space..].find('x').expect("expected x in fit") + space;
    fit::FitRect {
        x: num(s),
        y: num(&s[comma + 1..]),
        w: num(&s[space + 1..]),
        h: num(&s[x_mark + 1..]),
    }
}

fn edge_of(s: &str) -> fit::Edge {
    match s.split_whitespace().next() {
        Some("Left") => fit::Edge::Left,
        Some("Right") => fit::Edge::Right,
        Some("Top") => fit::Edge::Top,
        Some("Bottom") => fit::Edge::Bottom,
        other => panic!("unknown edge {other:?} in trace line"),
    }
}

/// Every replayable line of `log` whose timestamp falls within `[from, to]`, in file order.
fn parse(log: &str, from: f64, to: f64) -> Vec<Ev> {
    let mut out = Vec::new();
    for line in log.lines() {
        let (is_edge, is_grab) = (line.starts_with("[EDGE] "), line.starts_with("[GRAB] "));
        if !is_edge && !is_grab {
            continue;
        }
        let t = num(after(line, "t="));
        if !(from..=to).contains(&t) {
            continue;
        }
        if is_edge {
            out.push(Ev::Edge {
                t,
                cur: pair(after(line, "cur=")),
                fit: fit_rect(after(line, "fit=")),
                deep: after(line, "deep=").starts_with("true"),
                latched: after(line, "latched=").starts_with("true"),
            });
        } else if line.contains(" grabbing:") {
            out.push(Ev::Grabbing { t });
        } else if line.contains(" press ") {
            out.push(Ev::Press {
                t,
                edge: edge_of(after(line, " press ")),
                pos: pair(after(line, " at ")),
                delta: pair(after(line, " d=")),
                charge: num(after(line, "charge=")),
                hold: num(after(after(line, "charge="), "/")),
            });
        } else if line.contains(" releasing ") {
            out.push(Ev::Releasing {
                t,
                target: pair(after(line, "to view ")),
            });
        }
    }
    out
}

/// The hidden-seam dogfood episode of 2026-08-20 (boot 8): a two-panel rig, external primary
/// fullscreen, the internal display's window swiped off its Space — so the seam to it clamps
/// like an outer edge and the way through is the resistance-and-release. The user reported the
/// seam as "wobble, no way to cross"; this replay pins what the *policy* did with the recorded
/// gesture, so the open diagnosis starts from a verdict instead of a theory.
///
/// The recorded cycle, all four grab verbs in one gesture: a latched (Ctrl-Opt) release ends by
/// leaving the guest; coming back deep serves the dwell and the grab is taken; a sustained
/// left-edge press charges past the side hold and RELEASES out onto the internal panel
/// (`x = fit.x − RELEASE_OFFSET`); returning deep re-takes the grab a second later. The policy
/// let the pointer through — whatever read as a wall on the rig happened *after* the verdict,
/// in execution (warp/visibility), which is Move C's territory, not this state machine's.
#[test]
fn the_hidden_seam_trace_replays_to_a_release_through_the_seam() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../spikes/m15-secondary-input/hidden-seam-boot8-worker.log"
    );
    let log = std::fs::read_to_string(path).expect("the committed hidden-seam trace");
    // From the first free event after the Ctrl-Opt release (the state seeded below) to the
    // re-grab that closes the cycle.
    let events = parse(&log, 613_411.0, 615_896.0);
    assert!(
        events.len() > 100,
        "the episode should be dense: {events:?}"
    );

    let base = Instant::now();
    let t0 = 613_411.0;
    let at = |t: f64| base + Duration::from_secs_f64((t - t0) / 1000.0);
    // The window opens mid-latch: a Ctrl-Opt release preceded it (`pointer capture: OFF`,
    // t=611757), and the first [EDGE] lines print `latched=true`.
    let mut st = GrabState::default();
    st.release_by_user(true);

    // The recorded arrangement, as the release rule saw it: the internal panel lies beyond the
    // primary's LEFT edge (the recorded release warped to cg(−8, 1285), onto it); every other
    // edge of the union has nothing beyond it. Only Left presses occur in the episode.
    let reachable = |p: (f64, f64)| p.0 < 0.0;
    // The press lines don't carry the fit; it is the owning (external, primary) window's,
    // recorded on the grabbing lines.
    let press_fit = fit::FitRect {
        x: 0.0,
        y: 0.0,
        w: 2560.0,
        h: 1440.0,
    };
    // The session ran the default hold; the recorded press lines show its side timing
    // (hold 0.18 = 0.30 × the side factor, decay 0.90), which the replay must re-derive.
    let hold = crate::vmlib::schema::EdgeHold::Standard.seconds();

    let mut captured = false;
    let mut grabs: Vec<f64> = Vec::new();
    let mut releases: Vec<(f64, fit::Edge, Release)> = Vec::new();
    for ev in &events {
        match ev {
            Ev::Edge {
                t,
                cur,
                fit,
                deep,
                latched,
            } => {
                if captured {
                    // The replay grabbed an event or two before the recorded run did (print
                    // jitter); in its world this straggler free event never happens.
                    continue;
                }
                // `deep` is a pure function of the recorded values — an exact pin.
                assert_eq!(fit::deep_inside(*cur, *fit), *deep, "deep at t={t}");
                let s = Free {
                    now: at(*t),
                    pos: *cur,
                    fit: *fit,
                    // Not in the trace: the grab engaged in the episode, so both held.
                    fullscreen_and_key: true,
                    space_visible: true,
                    grab_enabled: true,
                    buttons_down: false,
                    click: false,
                    // The recording has no menu open; the trace would carry no click anyway.
                    menu_open: false,
                };
                // The recording is a pointer over guest content throughout — the episode is
                // about the grab, not about macOS chrome.
                // No screenshot UI in the recording, as in every recorded episode so far.
                let out = free_step(&mut st, &s, || true, || false);
                // The latch's whole lifecycle — holding inside, ending by leaving — replays
                // exactly ([EDGE] prints it after the step).
                assert_eq!(st.user_released(), *latched, "latched at t={t}");
                if out.grab {
                    grabs.push(*t);
                    captured = true;
                }
            }
            Ev::Grabbing { t } => {
                assert!(captured, "the recorded grab at t={t} did not replay");
                let took = grabs.last().expect("a grab must have been recorded");
                assert!((t - took).abs() < 40.0, "grab at t={took}, recorded {t}");
            }
            Ev::Press {
                t,
                edge,
                pos,
                delta,
                charge,
                hold: rec_hold,
            } => {
                assert!(captured, "press lines only occur captured (t={t})");
                if releases.len() > grabs.len() - 1 {
                    continue; // replay released early; skip its world's non-events
                }
                let s = Press {
                    now: at(*t),
                    pos: *pos,
                    delta: *delta,
                    fit: press_fit,
                    buttons_down: false,
                    hold,
                    side: fit::SideTuning::default(),
                    fullscreen: true,
                };
                let tier = capture_tier(captured, &st);
                let out = press_step(&mut st, tier, &s, reachable);
                let p = out.pressing.expect("the recorded press must replay");
                assert_eq!(p.edge, *edge, "edge at t={t}");
                assert!((p.hold - rec_hold).abs() < 1e-9, "hold at t={t}");
                // Charge telescopes over the printed timestamps, so it tracks the recorded
                // value to print jitter, not to a random walk.
                assert!(
                    (p.charge - charge).abs() < 0.02,
                    "charge {:.3} vs recorded {charge:.3} at t={t}",
                    p.charge
                );
                if let Some((edge, rel)) = out.release {
                    releases.push((*t, edge, rel));
                    // The adapter's contract: `release_grab` stops the policy's hold.
                    st.stop_holding();
                    captured = false;
                }
            }
            Ev::Releasing { t, target } => {
                let (rt, edge, rel) = releases.last().expect("the recorded release must replay");
                assert!((t - rt).abs() < 50.0, "release at t={rt}, recorded {t}");
                assert_eq!(*edge, fit::Edge::Left);
                let Release::Out(p) = rel else {
                    panic!("a side release goes OUT, not in place: {rel:?}");
                };
                // x is exact (fit.x − RELEASE_OFFSET); y inherits the trace's one-decimal
                // rounding of the press position.
                assert_eq!(p.0, press_fit.x - fit::RELEASE_OFFSET);
                assert!(
                    (p.1 - target.1).abs() < 0.06,
                    "release y {} vs {target:?}",
                    p.1
                );
            }
        }
    }
    // The whole cycle: the grab taken on return, released through the seam, re-taken after.
    assert_eq!(grabs.len(), 2, "two grabs: the approach and the return");
    assert_eq!(releases.len(), 1, "one release, out the left edge");
    assert!(
        st.holding(),
        "the episode ends with the policy holding again"
    );
}
