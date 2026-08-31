// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Generational rotation for the per-VM files in `<bundle>/logs/`.
//!
//! Everything a managed VM writes there is per-boot, and the boot that matters is almost
//! always the one that just ended badly. Overwriting it at the next start means the only
//! copy of an incident is whatever a human thought to save by hand before restarting — on
//! 2026-08-31 a dogfood SIGSEGV was diagnosable only because the user did exactly that.

use std::path::{Path, PathBuf};

/// How many previous boots to keep beside the live file.
///
/// This was one for the balloon trace, and at that depth every field measurement is a race
/// against the next deploy: a bundle push rotates the live file to `.1` and destroys
/// whatever was there, so the run you are still analysing dies the moment the fix for it
/// ships. Both windows of the 2026-08-14 allowance-shortfall A/B were one deploy from gone
/// when they were rescued (`spikes/hv-ledger-gap/postdeploy-2026-08-14/`). Five boots is
/// still bounded — a long dogfood day is a few MB per boot — and deep enough that copying a
/// window out is never urgent.
pub const GENERATIONS: u32 = 5;

/// `foo.log` → `foo.<n>.log`. Always derived from the base path, never from the previous
/// generation, so `.1` → `.2` cannot compound into `foo.1.2.log`.
fn generation(p: &Path, n: u32) -> PathBuf {
    match p.extension().and_then(|e| e.to_str()) {
        Some(ext) => p.with_extension(format!("{n}.{ext}")),
        None => p.with_extension(n.to_string()),
    }
}

/// Shift `p` → `p.1` → … → `p.<generations>`, dropping the oldest.
///
/// Best-effort by design: a rename that fails costs history, never the file itself, so
/// every error is ignored and the caller still opens `p` fresh.
pub fn rotate(p: &Path, generations: u32) {
    for n in (1..generations).rev() {
        let _ = std::fs::rename(generation(p, n), generation(p, n + 1));
    }
    if p.exists() {
        let _ = std::fs::rename(p, generation(p, 1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deploy must not destroy the run being analysed. Five VM starts survive, oldest
    /// dropped — at one generation the second boot after a measurement already took the
    /// window with it, which nearly cost the 2026-08-14 A/D baseline twice.
    #[test]
    fn five_boots_survive_the_deploys_after_them() {
        let dir = std::env::temp_dir().join(format!("limina-logrot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let live = dir.join("supervisor.log");

        // Seven boots: each writes its own generation, then the next start rotates it back.
        for boot in 0..7 {
            rotate(&live, GENERATIONS);
            std::fs::write(&live, format!("boot{boot}")).unwrap();
        }

        // Boot 6 is live; 5..=2 sit behind it, oldest-first, and boot 1 has aged out.
        assert_eq!(std::fs::read_to_string(&live).unwrap(), "boot6");
        for (n, boot) in (1..=4).zip((2..=5).rev()) {
            let path = dir.join(format!("supervisor.{n}.log"));
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                format!("boot{boot}"),
                "generation .{n} should hold boot{boot}"
            );
        }
        assert!(
            !dir.join("supervisor.5.log").exists()
                || std::fs::read_to_string(dir.join("supervisor.5.log")).unwrap() == "boot1",
            "the oldest kept generation is boot1; anything older is dropped"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The suffix goes before the extension, and a rotated name never accumulates one.
    #[test]
    fn a_generation_is_numbered_before_the_extension() {
        assert_eq!(
            generation(Path::new("/l/supervisor.log"), 2)
                .to_str()
                .unwrap(),
            "/l/supervisor.2.log"
        );
        assert_eq!(
            generation(Path::new("/l/balloon-trace.jsonl"), 1)
                .to_str()
                .unwrap(),
            "/l/balloon-trace.1.jsonl"
        );
        assert_eq!(
            generation(Path::new("/l/console"), 3).to_str().unwrap(),
            "/l/console.3"
        );
    }
}
