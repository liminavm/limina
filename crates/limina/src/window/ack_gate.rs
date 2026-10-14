// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! When to tell the worker a frame is shown, and when the surface it replaced is free.
//!
//! Each presented frame is acked twice. `shown <id>` completes the guest's held flush fence, and
//! is sent once the surface the frame replaced has left window-server use, or at `cap` past the
//! frame's latch if it has not: on time, that clear is the swap vblank; past it, the replaced
//! surface is off glass and only over-held (spikes/present-pacing/, useprobe2). `free <id>` reports
//! the clear itself, up to `free_cap` after the `shown`, and is informational to the worker.
//!
//! Releases go out in frame order. The `free` tail is tracked apart from them, so a surface the
//! window server over-holds never delays a later frame's release.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// A replaced surface whose window-server use the gate watches.
pub(crate) trait Watched {
    fn is_in_use(&self) -> bool;
}

impl Watched for super::present::SendSurface {
    fn is_in_use(&self) -> bool {
        super::present::SendSurface::is_in_use(self)
    }
}

/// What the gate tells the sender to send.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Ack {
    /// `shown <id>`; `waited` is how long the gate held it, when it gated at all.
    Shown { id: u32, waited: Option<Duration> },
    /// `free <id>`; `cleared` says whether the replaced surface was seen to clear.
    Free { id: u32, cleared: bool },
}

struct Release<S> {
    id: u32,
    replaced: Option<S>,
    gated: bool,
    since: Instant,
}

struct Tail<S> {
    id: u32,
    replaced: S,
    since: Instant,
}

pub(crate) struct AckGate<S> {
    cap: Duration,
    free_cap: Duration,
    releases: VecDeque<Release<S>>,
    tails: Vec<Tail<S>>,
}

impl<S: Watched> AckGate<S> {
    pub(crate) fn new(cap: Duration, free_cap: Duration) -> Self {
        Self {
            cap,
            free_cap,
            releases: VecDeque::new(),
            tails: Vec::new(),
        }
    }

    /// Frame `id` latched at `now`, replacing `replaced`; `gated` is false when the release should
    /// not wait for the replaced surface (the live latch-only A/B).
    pub(crate) fn push(&mut self, id: u32, replaced: Option<S>, gated: bool, now: Instant) {
        self.releases.push_back(Release {
            id,
            replaced,
            gated,
            since: now,
        });
    }

    /// Whether nothing is waiting, so the sender may block until the next frame.
    pub(crate) fn idle(&self) -> bool {
        self.releases.is_empty() && self.tails.is_empty()
    }

    /// Everything due at `now`, in the order to send it.
    pub(crate) fn poll(&mut self, now: Instant) -> Vec<Ack> {
        let mut out = Vec::new();
        self.poll_tails(now, &mut out);
        while let Some(front) = self.releases.front() {
            let held = front.gated && front.replaced.as_ref().is_some_and(|s| s.is_in_use());
            if held && now.duration_since(front.since) < self.cap {
                break;
            }
            let r = self.releases.pop_front().expect("front exists");
            out.push(Ack::Shown {
                id: r.id,
                waited: (r.gated && r.replaced.is_some()).then(|| now.duration_since(r.since)),
            });
            match r.replaced {
                Some(s) if s.is_in_use() => self.tails.push(Tail {
                    id: r.id,
                    replaced: s,
                    since: now,
                }),
                replaced => out.push(Ack::Free {
                    id: r.id,
                    cleared: replaced.is_some(),
                }),
            }
        }
        out
    }

    fn poll_tails(&mut self, now: Instant, out: &mut Vec<Ack>) {
        let free_cap = self.free_cap;
        self.tails.retain(|t| {
            let cleared = !t.replaced.is_in_use();
            if cleared || now.duration_since(t.since) >= free_cap {
                out.push(Ack::Free { id: t.id, cleared });
                false
            } else {
                true
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    #[derive(Clone)]
    struct Fake(Rc<Cell<bool>>);

    impl Fake {
        fn in_use() -> Self {
            Self(Rc::new(Cell::new(true)))
        }
        fn clear(&self) {
            self.0.set(false);
        }
    }

    impl Watched for Fake {
        fn is_in_use(&self) -> bool {
            self.0.get()
        }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn shown_ids(acks: &[Ack]) -> Vec<u32> {
        acks.iter()
            .filter_map(|a| match a {
                Ack::Shown { id, .. } => Some(*id),
                Ack::Free { .. } => None,
            })
            .collect()
    }

    #[test]
    fn a_release_waits_for_the_replaced_surface_to_clear() {
        let t0 = Instant::now();
        let mut gate = AckGate::new(ms(20), ms(50));
        let old = Fake::in_use();
        gate.push(1, Some(old.clone()), true, t0);
        assert!(shown_ids(&gate.poll(t0 + ms(5))).is_empty());
        old.clear();
        assert_eq!(
            gate.poll(t0 + ms(6)),
            vec![
                Ack::Shown {
                    id: 1,
                    waited: Some(ms(6))
                },
                Ack::Free {
                    id: 1,
                    cleared: true
                },
            ]
        );
        assert!(gate.idle());
    }

    #[test]
    fn a_release_goes_out_at_the_cap_and_its_free_report_follows_the_clear() {
        let t0 = Instant::now();
        let mut gate = AckGate::new(ms(20), ms(50));
        let old = Fake::in_use();
        gate.push(1, Some(old.clone()), true, t0);
        assert_eq!(shown_ids(&gate.poll(t0 + ms(20))), vec![1]);
        assert!(!gate.idle(), "the free report is still owed");
        old.clear();
        assert_eq!(
            gate.poll(t0 + ms(25)),
            vec![Ack::Free {
                id: 1,
                cleared: true
            }]
        );
    }

    #[test]
    fn an_over_held_surface_does_not_hold_back_the_next_release() {
        let t0 = Instant::now();
        let mut gate = AckGate::new(ms(20), ms(50));
        // Frame 1's replaced surface never clears: its release goes at the cap.
        gate.push(1, Some(Fake::in_use()), true, t0);
        assert_eq!(shown_ids(&gate.poll(t0 + ms(20))), vec![1]);
        // Frame 2 latches a refresh later, and what it replaced is already free.
        let freed = Fake::in_use();
        freed.clear();
        gate.push(2, Some(freed), true, t0 + ms(37));
        assert_eq!(
            shown_ids(&gate.poll(t0 + ms(37))),
            vec![2],
            "frame 2's release waited for frame 1's over-held surface"
        );
    }

    #[test]
    fn releases_stay_in_frame_order() {
        let t0 = Instant::now();
        let mut gate = AckGate::new(ms(20), ms(50));
        let first = Fake::in_use();
        gate.push(1, Some(first.clone()), true, t0);
        let second = Fake::in_use();
        second.clear();
        gate.push(2, Some(second), true, t0 + ms(1));
        assert!(
            shown_ids(&gate.poll(t0 + ms(2))).is_empty(),
            "frame 2 cannot be released ahead of frame 1"
        );
        first.clear();
        assert_eq!(shown_ids(&gate.poll(t0 + ms(3))), vec![1, 2]);
    }

    #[test]
    fn an_ungated_release_and_a_first_frame_go_at_once() {
        let t0 = Instant::now();
        let mut gate = AckGate::new(ms(20), ms(50));
        gate.push(1, None, true, t0);
        gate.push(2, Some(Fake::in_use()), false, t0);
        let acks = gate.poll(t0);
        assert_eq!(shown_ids(&acks), vec![1, 2]);
        assert!(acks.contains(&Ack::Shown {
            id: 1,
            waited: None
        }));
    }
}
