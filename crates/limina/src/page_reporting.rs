// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Whether the balloon offers the guest free page reporting (`VIRTIO_BALLOON_F_REPORTING`).
//!
//! Free page reporting hands the host the guest's free memory as the guest frees it, including
//! the free pages inside plugged virtio-mem blocks, which nothing else returns while they stay
//! plugged. But a Linux guest without upstream commit 0b45f6927a14 ("mm/page_reporting: use
//! system_freezable_wq to fix UAF during suspend") crashes on suspend-to-idle once it has
//! negotiated it: `virtballoon_freeze` frees the reporting virtqueue while the reporting work,
//! on a workqueue that does not freeze, still uses it.
//!
//! So a managed VM learns it. Each boot asks the guest's `qemu-guest-agent` which kernel it runs,
//! and [`kernel_has_fix`] decides; the answer goes to `state.toml` and the next cold boot follows
//! it. A guest with no agent never answers and never gets reporting. A resume follows what its
//! `[suspended]` record says, since the guest negotiated the feature before it was saved.
//! `--balloon-free-page-reporting` turns it on whatever has been learned.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::qga::client::Qga;
use crate::vmlib::state;

/// How often the learner asks the guest agent while it has not answered.
const ASK_EVERY: Duration = Duration::from_secs(15);

/// How long one question may take.
const ASK_WITHIN: Duration = Duration::from_secs(5);

/// The first release of each stable series that carries the fix; a series not listed never got
/// it (6.19 and 7.0 among them). Mainline took it in 7.2-rc6.
const FIXED_IN: &[((u32, u32), u32)] = &[
    ((7, 1), 8),
    ((6, 18), 44),
    ((6, 12), 103),
    ((6, 6), 151),
    ((6, 1), 183),
    ((5, 15), 216),
    ((5, 10), 265),
];

/// Whether a kernel that calls itself `release` (`uname -r`) carries the fix. `None` when the
/// release does not start with a version.
pub fn kernel_has_fix(release: &str) -> Option<bool> {
    let (version, rest) = release
        .split_once(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or((release, ""));
    let mut parts = version.split('.').filter(|p| !p.is_empty());
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    let patch: u32 = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    if (major, minor) > (7, 2) {
        return Some(true);
    }
    if (major, minor) == (7, 2) {
        return Some(patch > 0 || release_candidate(rest).is_none_or(|rc| rc >= 6));
    }
    Some(
        FIXED_IN
            .iter()
            .find(|&&(series, _)| series == (major, minor))
            .is_some_and(|&(_, first)| patch >= first),
    )
}

/// The release candidate a release's suffix names (`-rc3`, Fedora's `-0.rc3.30.fc45`), if any.
fn release_candidate(suffix: &str) -> Option<u32> {
    suffix.match_indices("rc").find_map(|(at, _)| {
        let digits: String = suffix[at + 2..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok()
    })
}

/// Whether this spawn offers reporting. A cold boot takes `forced` or what was learned; a resume
/// takes what the snapshot was saved with, and a record that does not say predates the setting,
/// when only `forced` turned it on.
pub fn for_spawn(
    forced: bool,
    learned: Option<bool>,
    resumed: bool,
    record: Option<&state::Suspended>,
) -> bool {
    match (resumed, record) {
        (true, Some(rec)) => rec.page_reporting.unwrap_or(forced),
        _ => forced || learned == Some(true),
    }
}

/// What the managed VM's `state.toml` has learned, if there is one.
pub fn learned(state_file: Option<&Path>) -> Option<bool> {
    state::load(state_file?)?.page_reporting
}

static SPAWNED: AtomicBool = AtomicBool::new(false);
static STATE_FILE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Record what the worker just spawned offers, and where to keep what its guest teaches.
pub fn note_spawn(on: bool, state_file: Option<&Path>) {
    SPAWNED.store(on, Ordering::SeqCst);
    *STATE_FILE.lock().unwrap_or_else(|p| p.into_inner()) = state_file.map(Path::to_path_buf);
}

/// Whether the running worker offers reporting, for the `[suspended]` record a suspend writes.
pub fn spawned() -> bool {
    SPAWNED.load(Ordering::SeqCst)
}

/// Ask the guest behind a freshly attached agent port which kernel it runs, and record whether
/// that kernel takes reporting. Ends once answered, or when a later spawn replaces the port.
pub fn learn(qga: Arc<Qga>) {
    let Some(state_file) = STATE_FILE.lock().unwrap_or_else(|p| p.into_inner()).clone() else {
        return;
    };
    let spawn = crate::elastic_mem::spawns();
    let spawned = std::thread::Builder::new()
        .name("page-reporting-learn".into())
        .spawn(move || {
            while crate::elastic_mem::spawns() == spawn {
                std::thread::sleep(ASK_EVERY);
                let Ok(os) = qga.call("guest-get-osinfo", None, ASK_WITHIN) else {
                    continue;
                };
                let Some(release) = os.get("kernel-release").and_then(|v| v.as_str()) else {
                    return;
                };
                record(&state_file, release);
                return;
            }
        });
    if let Err(e) = spawned {
        log::warn!("page reporting: cannot ask the guest which kernel it runs: {e}");
    }
}

fn record(state_file: &Path, release: &str) {
    let Some(fixed) = kernel_has_fix(release) else {
        log::info!("page reporting: cannot read a version in kernel release {release:?}");
        return;
    };
    if learned(Some(state_file)) == Some(fixed) {
        return;
    }
    match state::set_page_reporting(state_file, Some(fixed)) {
        Ok(()) if fixed => log::warn!(
            "page reporting: the guest runs {release}, which reports free pages safely; its next \
             boot is offered free page reporting"
        ),
        Ok(()) => log::warn!(
            "page reporting: the guest runs {release}, which crashes on suspend-to-idle with free \
             page reporting; its next boot is not offered it"
        ),
        Err(e) => log::warn!("page reporting: cannot record what {release} takes: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernels_carry_the_fix_from_their_series_backport() {
        for (release, want) in [
            ("7.1.13-limina16k.2", true),
            ("7.1.13-200.fc44.aarch64", true),
            ("7.1.8", true),
            ("7.1.7-100.fc44.aarch64", false),
            ("7.0.12-200.fc44.aarch64", false),
            ("6.19.10-300.fc44.aarch64", false),
            ("6.18.44", true),
            ("6.18.43", false),
            ("6.12.103-rt", true),
            ("6.6.150", false),
            ("6.1.183", true),
            ("5.15.216", true),
            ("5.10.264", false),
            ("6.17.9", false),
            ("4.19.300", false),
            ("7.2.0-0.rc5.20260720git.43.fc45.aarch64", false),
            ("7.2.0-0.rc6.30.fc45.aarch64", true),
            ("7.2.0-rc6", true),
            ("7.2.0-rc3", false),
            ("7.2.0-100.fc45.aarch64", true),
            ("7.2.8-200.fc45.aarch64", true),
            ("7.3.0-0.rc1.1.fc45.aarch64", true),
            ("8.0.1", true),
            ("7.2", true),
        ] {
            assert_eq!(kernel_has_fix(release), Some(want), "{release}");
        }
        assert_eq!(kernel_has_fix("?"), None);
        assert_eq!(kernel_has_fix("linux"), None);
        assert_eq!(kernel_has_fix("7"), None);
    }

    fn record(page_reporting: Option<bool>) -> state::Suspended {
        state::Suspended {
            snapshot: "snap".into(),
            ipa_granule: None,
            nested_virt: None,
            gpu_metal: None,
            machine: None,
            mem_layout: None,
            page_reporting,
        }
    }

    #[test]
    fn a_cold_boot_follows_what_was_learned_and_a_resume_its_record() {
        assert!(!for_spawn(false, None, false, None));
        assert!(!for_spawn(false, Some(false), false, None));
        assert!(for_spawn(false, Some(true), false, None));
        assert!(for_spawn(true, Some(false), false, None));
        // A resume takes the record, whatever has been learned since.
        assert!(!for_spawn(
            false,
            Some(true),
            true,
            Some(&record(Some(false)))
        ));
        assert!(for_spawn(
            false,
            Some(false),
            true,
            Some(&record(Some(true)))
        ));
        // A record from before the setting: only the flag could have turned it on.
        assert!(!for_spawn(false, Some(true), true, Some(&record(None))));
        assert!(for_spawn(true, None, true, Some(&record(None))));
        // A flat run resumes with no record and takes what it was started with.
        assert!(for_spawn(true, None, true, None));
    }

    #[test]
    fn learning_writes_the_state_only_when_it_changes() {
        let dir =
            std::env::temp_dir().join(format!("limina-page-reporting-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.toml");
        let _ = std::fs::remove_file(&path);

        super::record(&path, "6.19.10-300.fc44.aarch64");
        assert_eq!(learned(Some(&path)), Some(false));
        super::record(&path, "7.1.13-200.fc44.aarch64");
        assert_eq!(learned(Some(&path)), Some(true));
        super::record(&path, "not a version");
        assert_eq!(learned(Some(&path)), Some(true));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
