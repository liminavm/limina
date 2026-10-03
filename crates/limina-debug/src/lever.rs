// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Named diagnostic switches that can be flipped while the process runs.

use std::sync::atomic::{AtomicU8, Ordering};

const UNSEEDED: u8 = 0;
const OFF: u8 = 1;
const ON: u8 = 2;

/// One diagnostic switch: a `static`, read with [`Lever::on`] at the site it gates.
///
/// Its starting value is its environment variable (set and not `"0"` means on), read on first
/// use, so `LIMINA_EDGE_TRACE=1` keeps meaning what it always meant. After that [`Lever::set`]
/// owns it. A lever is for *diagnostics* — output that costs something to produce and changes
/// nothing else. A switch that changes behaviour is not a lever.
pub struct Lever {
    name: &'static str,
    env: &'static str,
    about: &'static str,
    state: AtomicU8,
}

impl Lever {
    pub const fn new(name: &'static str, env: &'static str, about: &'static str) -> Self {
        Self {
            name,
            env,
            about,
            state: AtomicU8::new(UNSEEDED),
        }
    }

    /// Whether the lever is on. Cheap enough for a per-event check: one relaxed load once seeded.
    pub fn on(&self) -> bool {
        match self.state.load(Ordering::Relaxed) {
            UNSEEDED => self.seed(),
            s => s == ON,
        }
    }

    /// Turn the lever on or off from now on.
    pub fn set(&self, on: bool) {
        self.state
            .store(if on { ON } else { OFF }, Ordering::Relaxed);
    }

    /// The short name the CLI and the menu use (`edge-trace`).
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The environment variable that seeds it (`LIMINA_EDGE_TRACE`).
    pub fn env(&self) -> &'static str {
        self.env
    }

    /// One line on what turning it on prints.
    pub fn about(&self) -> &'static str {
        self.about
    }

    fn seed(&self) -> bool {
        let on = std::env::var_os(self.env).is_some_and(|v| v != "0");
        // A `set` that raced the first read wins: it is the newer statement.
        let _ = self.state.compare_exchange(
            UNSEEDED,
            if on { ON } else { OFF },
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        self.state.load(Ordering::Relaxed) == ON
    }
}

/// The lever called `name` in `levers`.
pub fn find(levers: &[&'static Lever], name: &str) -> Option<&'static Lever> {
    levers.iter().copied().find(|l| l.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lever_is_seeded_from_its_variable_and_then_follows_set() {
        // SAFETY: test-only, and no other test reads this variable.
        unsafe { std::env::set_var("LIMINA_DEBUG_TEST_LEVER_SEEDED", "1") };
        static L: Lever = Lever::new("seeded", "LIMINA_DEBUG_TEST_LEVER_SEEDED", "");
        assert!(L.on());
        L.set(false);
        assert!(!L.on());
        L.set(true);
        assert!(L.on());
    }

    #[test]
    fn zero_and_absence_both_mean_off() {
        // SAFETY: test-only, and no other test reads these variables.
        unsafe { std::env::set_var("LIMINA_DEBUG_TEST_LEVER_ZERO", "0") };
        static ZERO: Lever = Lever::new("zero", "LIMINA_DEBUG_TEST_LEVER_ZERO", "");
        static ABSENT: Lever = Lever::new("absent", "LIMINA_DEBUG_TEST_LEVER_ABSENT", "");
        assert!(!ZERO.on());
        assert!(!ABSENT.on());
    }

    #[test]
    fn a_set_before_the_first_read_is_not_undone_by_the_seed() {
        // SAFETY: test-only, and no other test reads this variable.
        unsafe { std::env::set_var("LIMINA_DEBUG_TEST_LEVER_EARLY", "1") };
        static L: Lever = Lever::new("early", "LIMINA_DEBUG_TEST_LEVER_EARLY", "");
        L.set(false);
        assert!(!L.on());
    }

    #[test]
    fn find_looks_levers_up_by_name() {
        static A: Lever = Lever::new("a-trace", "LIMINA_DEBUG_TEST_A", "");
        static B: Lever = Lever::new("b-trace", "LIMINA_DEBUG_TEST_B", "");
        let all: &[&'static Lever] = &[&A, &B];
        assert_eq!(
            find(all, "b-trace").map(Lever::env),
            Some("LIMINA_DEBUG_TEST_B")
        );
        assert!(find(all, "c-trace").is_none());
    }
}
