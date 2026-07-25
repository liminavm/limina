// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! vmlib — managed VM definitions (`.liminavm` bundles).
//!
//! Implements Phase 1 of `docs/design/vm-definitions.md`: a VM definition is a
//! directory `<name>.liminavm/` holding a single `vm.toml` plus everything the VM
//! owns (`disks/`, `run/`, `logs/`). The library lives at
//! `~/Library/Application Support/Limina/VMs/` and is enumerated by both the CLI
//! (`limina ls`) and the control-center UI — one store, no daemon.
//!
//! The definition layer is pure supervisor-side policy: resolving a definition
//! produces the same `Cli` the flag invocation produces today (see
//! `cli_from_definition` in main.rs), so the worker and the supervisor internals
//! don't know definitions exist.
//!
//! One deliberate deviation from the design doc: the running-VM `flock` is taken
//! on a stable sentinel `run/lock`, NOT on `vm.toml` — config saves are atomic
//! (tmp + rename), and a rename swaps the inode out from under an flock, which
//! would make a running VM look stopped.

pub mod bundle;
pub mod import;
pub mod runtime;
pub mod schema;
pub mod state;
