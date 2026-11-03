// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Debugging a running VM without restarting it.
//!
//! Most of what we chase is intermittent and shows up during ordinary use, on a VM that was started
//! with the default `warn` filter and no traces. Before this crate the only way to see more was to
//! set `RUST_LOG` and a `LIMINA_*_TRACE` variable and relaunch — which ends the state being
//! investigated. So a running process can change both:
//!
//! - [`logger`]: the `RUST_LOG` filter, swappable at runtime. Both host processes install it in
//!   place of `env_logger`'s own, and `RUST_LOG` still sets the starting value.
//! - [`lever`]: named on/off switches for the diagnostic traces. A lever is seeded from its
//!   environment variable, so every recipe that sets one keeps working, and can then be flipped.
//! - [`memstat`] and [`alloc_census`]: what is using a process's memory, read from the inside
//!   (`vmmap -summary` suspends the task it reads, which freezes a guest), and which of our own
//!   call sites allocated what is still live.
//! - [`wire`]: the line protocol the supervisor's debug socket speaks, which `limina debug` and
//!   the Debug menu drive, and which the supervisor speaks to its worker.
//!
//! Nothing set here outlives the process: a relaunch is back to the environment. That is
//! deliberate — a trace left on by accident would otherwise fill a dogfood log for weeks.

pub mod alloc_census;
pub mod lever;
pub mod logger;
pub mod memstat;
pub mod wire;
