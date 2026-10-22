// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! How a VM's memory is laid out at each boot, and learning which guests drive a virtio-mem
//! device (docs/design/guest-ram-objects.md §3).
//!
//! A VM with a `min..max` range boots one of three ways:
//! - [`MemLayout::Whole`]: all of `max` is RAM, and the balloon covers `min..max`.
//! - [`MemLayout::Probe`]: as `Whole`, plus a virtio-mem region the guest is never asked to plug.
//!   It only shows whether the guest's driver binds; a managed VM boots this way until it has.
//! - [`MemLayout::Split`]: RAM at the minimum, and a virtio-mem region for the rest, which the
//!   guest plugs as the policy asks. A managed VM boots this way once its guest has bound the
//!   device, and goes back to `Probe` after a boot that runs [`FORGET_AFTER`] without binding it.
//!
//! A resume takes the layout the guest was suspended with, whatever has been learned since: the
//! guest's memory map is part of the snapshot. A flat run keeps no state, so it learns nothing
//! and boots `Whole`.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use limina_launch::connect::Endpoint;
use serde::{Deserialize, Serialize};

use crate::balloon_policy::{self, ReclaimMode};
use crate::vmlib::state;

/// Linux hot-adds memory on arm64 in 128 MiB blocks; a region is a whole number of them.
const UNIT_MIB: u64 = 128;
/// A probe's region: one memory block.
const PROBE_MIB: u64 = UNIT_MIB;
/// The least RAM a split boot starts with. The guest runs on RAM alone until `virtio_mem` loads,
/// which for a stock guest is after the initramfs: unlocking a LUKS2 root there takes up to 1 GiB
/// for argon2id by default, and a 1 GiB guest cannot unlock it.
const BOOT_FLOOR_MIB: u64 = 2048;
/// The device's block, and what a requested size is rounded to.
pub const BLOCK_BYTES: u64 = 2 << 20;
/// How long a boot may run without the guest binding the device before the learner stops
/// waiting, and a `Split` VM forgets it: `virtio_mem` is a module udev loads after the root is
/// mounted, so a boot that ends sooner says nothing.
const FORGET_AFTER: Duration = Duration::from_secs(300);
const POLL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum MemLayout {
    Whole,
    Probe,
    Split { ram_mib: u64, region_mib: u64 },
}

impl MemLayout {
    /// RAM at `min_mib`, but at least [`BOOT_FLOOR_MIB`], and raised so the region is whole
    /// memory blocks; `Whole` when that leaves no region.
    pub fn split(min_mib: u64, max_mib: u64) -> MemLayout {
        let ram_mib = min_mib.max(BOOT_FLOOR_MIB);
        let region_mib = max_mib.saturating_sub(ram_mib) / UNIT_MIB * UNIT_MIB;
        if region_mib == 0 {
            MemLayout::Whole
        } else {
            MemLayout::Split {
                ram_mib: max_mib - region_mib,
                region_mib,
            }
        }
    }

    /// The region the guest plugs as the policy asks, in bytes; `None` when the balloon covers
    /// the range.
    pub fn elastic_bytes(self) -> Option<u64> {
        match self {
            MemLayout::Split { region_mib, .. } => Some(region_mib << 20),
            MemLayout::Whole | MemLayout::Probe => None,
        }
    }
}

/// What the supervisor knows of a VM's memory, from which each spawn takes its layout.
#[derive(Clone, Debug)]
pub struct MemSpec {
    /// RAM when all of it is RAM: the range's max, or `--ram-mib`.
    pub ram_mib: u64,
    /// The `min..max` range, in MiB.
    pub range: Option<(u64, u64)>,
    pub reclaim: ReclaimMode,
    /// Build guest RAM from owned memory blocks (`LIMINA_GUEST_RAM` is not `anon`).
    pub owned_blocks: bool,
    /// `LIMINA_VIRTIO_MEM`: a virtio-mem region of this many MiB beside all of RAM, plugged only
    /// through the control socket's `mem` command. The tests drive the device with it; nothing
    /// is learned and no policy runs it.
    pub fixed_region_mib: Option<u64>,
    /// `LIMINA_VIRTIO_MEM=split`: every boot with a range splits, managed or not, whatever has been
    /// learned. A resume still takes the layout it was suspended with.
    pub force_split: bool,
    /// The managed VM's `state.toml`, which keeps what was learned.
    pub state_file: Option<PathBuf>,
    /// The worker's balloon control socket, which the learner reads.
    pub socket: Option<Endpoint>,
}

impl MemSpec {
    /// The layout for a spawn. `resumed_with` is `Some` on a resume, holding the layout the
    /// `[suspended]` record names, if it names one.
    pub fn layout(&self, resumed_with: Option<Option<MemLayout>>) -> MemLayout {
        let learned = || {
            if self.force_split {
                return Some(true);
            }
            self.state_file
                .as_deref()
                .and_then(state::load)
                .and_then(|s| s.virtio_mem)
        };
        pick(
            self.range.filter(|_| self.fixed_region_mib.is_none()),
            self.state_file.is_some() || self.force_split,
            resumed_with,
            learned,
        )
    }

    /// The worker's flags for `layout`.
    pub fn worker_args(&self, layout: MemLayout) -> Vec<String> {
        let (ram_mib, region_mib, requested_mib) = match layout {
            MemLayout::Whole => (self.ram_mib, self.fixed_region_mib, None),
            MemLayout::Probe => (self.ram_mib, Some(PROBE_MIB), None),
            MemLayout::Split {
                ram_mib,
                region_mib,
            } => (ram_mib, Some(region_mib), Some(region_mib)),
        };
        let mut args = vec!["--ram-mib".to_string(), ram_mib.to_string()];
        if self.owned_blocks {
            // In a split layout RAM ends at the minimum, so there is no elastic range to cut
            // fine: virtio-mem gives that back.
            let min = match layout {
                MemLayout::Split { .. } => None,
                MemLayout::Whole | MemLayout::Probe => self.range.map(|(min, _)| min),
            };
            let spans = balloon_policy::guest_ram_blocks(self.reclaim, min)
                .iter()
                .map(|(offset, block)| format!("{offset}:{block}"))
                .collect::<Vec<_>>()
                .join(",");
            args.extend(["--guest-ram-blocks".into(), spans]);
        }
        if let Some(region) = region_mib {
            args.extend(["--virtio-mem-mib".into(), region.to_string()]);
        }
        if let Some(requested) = requested_mib {
            args.extend(["--virtio-mem-requested-mib".into(), requested.to_string()]);
        }
        args
    }
}

fn pick(
    range: Option<(u64, u64)>,
    managed: bool,
    resumed_with: Option<Option<MemLayout>>,
    learned: impl FnOnce() -> Option<bool>,
) -> MemLayout {
    match (range, resumed_with) {
        (None, _) => MemLayout::Whole,
        // A record from before the layout was kept was written by a worker that knew only one.
        (Some(_), Some(saved)) => saved.unwrap_or(MemLayout::Whole),
        (Some(_), None) if !managed => MemLayout::Whole,
        (Some((min, max)), None) => match learned() {
            Some(true) => MemLayout::split(min, max),
            _ => match MemLayout::split(min, max) {
                MemLayout::Whole => MemLayout::Whole,
                _ => MemLayout::Probe,
            },
        },
    }
}

static SPAWNED: Mutex<MemLayout> = Mutex::new(MemLayout::Whole);
static SPAWNS: AtomicU64 = AtomicU64::new(0);

/// The layout the running worker was spawned with.
pub fn spawned() -> MemLayout {
    *SPAWNED.lock().unwrap_or_else(|p| p.into_inner())
}

/// How many workers this supervisor has spawned: the policy starts over when it moves.
pub fn spawns() -> u64 {
    SPAWNS.load(Ordering::SeqCst)
}

/// Record a spawn's layout, and watch whether its guest binds the device when that teaches the
/// VM something.
pub fn note_spawn(spec: &MemSpec, layout: MemLayout) {
    *SPAWNED.lock().unwrap_or_else(|p| p.into_inner()) = layout;
    let spawn = SPAWNS.fetch_add(1, Ordering::SeqCst) + 1;
    log::info!("memory: {layout:?}");
    if matches!(layout, MemLayout::Whole) {
        return;
    }
    let (Some(socket), Some(state_file)) = (spec.socket.clone(), spec.state_file.clone()) else {
        return;
    };
    if let Err(e) = std::thread::Builder::new()
        .name("virtio-mem-learn".into())
        .spawn(move || learn(socket, state_file, layout, spawn))
    {
        log::warn!("virtio-mem: cannot watch whether the guest binds the device: {e}");
    }
}

/// Wait for the guest to bind the device, then record what that teaches: a `Probe` VM learns
/// that its later boots can split, and a `Split` VM that waits [`FORGET_AFTER`] in vain forgets
/// it. Ends with the boot it watches.
fn learn(socket: Endpoint, state_file: PathBuf, layout: MemLayout, spawn: u64) {
    let started = Instant::now();
    while spawns() == spawn {
        std::thread::sleep(POLL);
        if bound(&socket) == Some(true) {
            if layout == MemLayout::Probe {
                match state::set_virtio_mem(&state_file, Some(true)) {
                    Ok(()) => log::info!(
                        "virtio-mem: the guest drives the device; from its next boot this VM \
                         takes RAM at its minimum and plugs the rest"
                    ),
                    Err(e) => log::warn!("virtio-mem: cannot record that the guest drives it: {e}"),
                }
            }
            return;
        }
        if started.elapsed() >= FORGET_AFTER {
            if matches!(layout, MemLayout::Split { .. }) && spawns() == spawn {
                log::warn!(
                    "virtio-mem: the guest has not bound the device in {}s; from its next boot \
                     this VM takes all its RAM up front again",
                    FORGET_AFTER.as_secs()
                );
                if let Err(e) = state::set_virtio_mem(&state_file, None) {
                    log::warn!("virtio-mem: cannot forget it: {e}");
                }
            }
            return;
        }
    }
}

/// Whether the worker says the guest has bound the device; `None` when it cannot be asked.
fn bound(socket: &Endpoint) -> Option<bool> {
    let mut conn = socket.connect().ok()?;
    conn.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
    writeln!(conn, "stats").and_then(|()| conn.flush()).ok()?;
    let mut line = String::new();
    BufReader::new(&conn).read_line(&mut line).ok()?;
    Some(line.split_whitespace().any(|t| t == "mem_bound=1"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPLIT: MemLayout = MemLayout::Split {
        ram_mib: 2048,
        region_mib: 6144,
    };

    #[test]
    fn a_split_keeps_max_and_rounds_ram_up_to_whole_blocks() {
        assert_eq!(MemLayout::split(2048, 8192), SPLIT);
        assert_eq!(
            MemLayout::split(2000, 8192),
            MemLayout::Split {
                ram_mib: 2048,
                region_mib: 6144
            }
        );
        assert_eq!(MemLayout::split(4000, 4096), MemLayout::Whole);
        // Early boot runs on RAM alone, so it never starts below the floor.
        assert_eq!(MemLayout::split(1024, 8192), SPLIT);
        assert_eq!(MemLayout::split(1024, 2048), MemLayout::Whole);
        assert_eq!(SPLIT.elastic_bytes(), Some(6144 << 20));
        assert_eq!(MemLayout::Probe.elastic_bytes(), None);
    }

    #[test]
    fn a_managed_vm_probes_until_it_learns_then_splits() {
        let range = Some((2048, 8192));
        assert_eq!(pick(range, true, None, || None), MemLayout::Probe);
        assert_eq!(pick(range, true, None, || Some(false)), MemLayout::Probe);
        assert_eq!(pick(range, true, None, || Some(true)), SPLIT);
        assert_eq!(pick(range, false, None, || Some(true)), MemLayout::Whole);
        assert_eq!(pick(None, true, None, || Some(true)), MemLayout::Whole);
        assert_eq!(
            pick(Some((4000, 4096)), true, None, || None),
            MemLayout::Whole
        );
    }

    #[test]
    fn a_resume_takes_the_layout_it_was_suspended_with() {
        let range = Some((2048, 8192));
        let learned = || panic!("a resume must not consult what was learned");
        assert_eq!(
            pick(range, true, Some(Some(MemLayout::Probe)), learned),
            MemLayout::Probe
        );
        assert_eq!(pick(range, true, Some(Some(SPLIT)), learned), SPLIT);
        assert_eq!(pick(range, true, Some(None), learned), MemLayout::Whole);
    }

    fn spec(layout_range: Option<(u64, u64)>, owned_blocks: bool) -> MemSpec {
        MemSpec {
            ram_mib: 8192,
            range: layout_range,
            reclaim: ReclaimMode::Aggressive,
            owned_blocks,
            fixed_region_mib: None,
            force_split: false,
            state_file: None,
            socket: None,
        }
    }

    #[test]
    fn worker_args_follow_the_layout() {
        let s = spec(Some((2048, 8192)), true);
        assert_eq!(
            s.worker_args(MemLayout::Whole),
            [
                "--ram-mib",
                "8192",
                "--guest-ram-blocks",
                "0:2097152,2147483648:16384"
            ]
        );
        assert_eq!(
            s.worker_args(MemLayout::Probe),
            [
                "--ram-mib",
                "8192",
                "--guest-ram-blocks",
                "0:2097152,2147483648:16384",
                "--virtio-mem-mib",
                "128"
            ]
        );
        assert_eq!(
            s.worker_args(SPLIT),
            [
                "--ram-mib",
                "2048",
                "--guest-ram-blocks",
                "0:2097152",
                "--virtio-mem-mib",
                "6144",
                "--virtio-mem-requested-mib",
                "6144"
            ]
        );
        assert_eq!(
            spec(Some((2048, 8192)), false).worker_args(MemLayout::Whole),
            ["--ram-mib", "8192"]
        );
    }

    #[test]
    fn a_forced_split_needs_no_state_but_still_resumes_as_saved() {
        let s = MemSpec {
            force_split: true,
            ..spec(Some((2048, 8192)), true)
        };
        assert_eq!(s.layout(None), SPLIT);
        assert_eq!(s.layout(Some(Some(MemLayout::Whole))), MemLayout::Whole);
        let tiny = MemSpec {
            force_split: true,
            ..spec(Some((1024, 2048)), true)
        };
        assert_eq!(tiny.layout(None), MemLayout::Whole);
    }

    #[test]
    fn a_fixed_region_rides_a_whole_boot() {
        let s = MemSpec {
            fixed_region_mib: Some(2048),
            ..spec(Some((2048, 8192)), false)
        };
        assert_eq!(s.layout(None), MemLayout::Whole);
        assert_eq!(
            s.worker_args(MemLayout::Whole),
            ["--ram-mib", "8192", "--virtio-mem-mib", "2048"]
        );
    }
}
