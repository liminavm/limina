// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Generational rotation for the per-VM files in `<bundle>/logs/`.
//!
//! Everything a managed VM writes there is per-boot, and the boot that matters is almost
//! always the one that just ended badly. Overwriting it at the next start means the only
//! copy of an incident is whatever a human thought to save by hand before restarting — on
//! 2026-08-31 a dogfood SIGSEGV was diagnosable only because the user did exactly that.

use std::io::{Read, Seek, SeekFrom, Write};
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

/// Most a single kept generation may occupy.
///
/// History is worth keeping; a verbatim copy of it is not. A dogfood supervisor log reached
/// 5.7 GB in three hours once (a per-sample GPU-budget line that should have logged per event),
/// and five generations of that would pin ~28 GB. What an investigation actually reads is the
/// end of the file, so an oversized generation keeps its tail and says how much it dropped.
pub const MAX_GENERATION_BYTES: u64 = 10 * 1024 * 1024;

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
    rotate_capped(p, generations, MAX_GENERATION_BYTES)
}

/// [`rotate`], with the per-generation byte cap spelled out (tests pass a small one).
pub fn rotate_capped(p: &Path, generations: u32, cap: u64) {
    for n in (1..generations).rev() {
        let _ = std::fs::rename(generation(p, n), generation(p, n + 1));
    }
    if !p.exists() {
        return;
    }
    let oversized = std::fs::metadata(p).map(|m| m.len() > cap).unwrap_or(false);
    if !oversized || keep_tail(p, &generation(p, 1), cap).is_err() {
        let _ = std::fs::rename(p, generation(p, 1));
    }
}

/// Copy the last `cap` bytes of `src` into `dst` and drop `src`.
///
/// Starts at the first line boundary inside the window so the file never opens mid-line, and
/// leads with a marker naming the bytes dropped — a truncated log that does not admit it is how
/// someone later concludes a run began at the wrong moment.
fn keep_tail(src: &Path, dst: &Path, cap: u64) -> std::io::Result<()> {
    let mut f = std::fs::File::open(src)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(cap);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity(cap as usize);
    f.read_to_end(&mut buf)?;
    let cut = buf.iter().position(|b| *b == b'\n').map_or(0, |i| i + 1);
    let dropped = start + cut as u64;

    let mut out = std::fs::File::create(dst)?;
    writeln!(
        out,
        "[limina] ---- {dropped} earlier bytes dropped on rotation (cap {cap}) ----"
    )?;
    out.write_all(&buf[cut..])?;
    out.sync_all()?;
    drop(out);
    std::fs::remove_file(src)?;
    Ok(())
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

    /// An oversized log keeps its END, because that is the part an investigation reads, and it
    /// says how much it dropped rather than pretending the run began there.
    #[test]
    fn an_oversized_generation_keeps_its_tail() {
        let dir = std::env::temp_dir().join(format!("limina-logrot-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let live = dir.join("supervisor.log");

        let mut big = String::new();
        for i in 0..2000 {
            big.push_str(&format!("line {i} padding padding padding padding\n"));
        }
        std::fs::write(&live, &big).unwrap();
        let cap = 1024u64;
        rotate_capped(&live, GENERATIONS, cap);

        let kept = std::fs::read_to_string(dir.join("supervisor.1.log")).unwrap();
        assert!(!live.exists(), "the live file is consumed by the rotation");
        assert!(
            kept.lines()
                .next()
                .unwrap()
                .contains("earlier bytes dropped"),
            "the truncation must announce itself: {:?}",
            kept.lines().next()
        );
        assert!(
            kept.contains("line 1999 "),
            "the END of the log is what must survive"
        );
        assert!(!kept.contains("line 0 "), "the head is what gets dropped");
        // Marker aside, the kept body stays within the cap and starts on a line boundary.
        let body = &kept[kept.find('\n').unwrap() + 1..];
        assert!(body.len() as u64 <= cap, "body {} > cap {cap}", body.len());
        assert!(
            body.starts_with("line "),
            "must not open mid-line: {:?}",
            &body[..20]
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
