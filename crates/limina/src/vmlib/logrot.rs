// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Generational rotation for the per-VM files in `<bundle>/logs/`.
//!
//! Everything a managed VM writes there is per-boot, and the boot that matters is almost
//! always the one that just ended badly. Overwriting it at the next start means the only
//! copy of an incident is whatever a human thought to save by hand before restarting — on
//! 2026-08-31 a dogfood SIGSEGV was diagnosable only because the user did exactly that.

use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStringExt;
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
        shift(&generation(p, n), &generation(p, n + 1), cap);
    }
    shift(p, &generation(p, 1), cap);
}

/// Move `from` to `to`, trimming to the last `cap` bytes if it is larger.
///
/// Every step of the shift goes through here, not just the live file. Capping only the first
/// move left an already-oversized generation to ride along untouched for as many rotations as it
/// took to age out — which is how 7.2 GB of superseded log was still on the dogfood Mac after
/// the cap shipped.
fn shift(from: &Path, to: &Path, cap: u64) {
    if !from.exists() {
        return;
    }
    let oversized = std::fs::metadata(from)
        .map(|m| m.len() > cap)
        .unwrap_or(false);
    if !oversized || keep_tail(from, to, cap).is_err() {
        let _ = std::fs::rename(from, to);
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

/// A live log is cut back in place once it grows past this.
///
/// Rotation happens once per boot, so on its own it bounds nothing inside a long run: a VM up for
/// four days on a base M1 grew its `supervisor.log` to 187 MB, mostly KosmicKrisp's opt-in
/// `[LIMINA]` stats. [`bound_in_place`] keeps the boot and the latest stretch of it.
pub const LIVE_CAP_BYTES: u64 = MAX_GENERATION_BYTES;
/// What a cut-back live log keeps of its start: the boot, which says what the run was.
pub const LIVE_HEAD_BYTES: u64 = 1024 * 1024;
/// What it keeps of its end: the recent history an investigation actually reads.
pub const LIVE_TAIL_BYTES: u64 = 5 * 1024 * 1024;

/// Cut the **append-mode** log at `path` back to its first `head` and last `tail` bytes once it
/// is longer than `cap`, with a marker naming what was dropped. Returns whether it cut. It is
/// read through its path and cut through `writer`.
///
/// Works in place, through a descriptor that shares the open file with every writer — the
/// supervisor's own stderr, which its worker inherits — because a rename would leave every
/// writer appending to the renamed file. That only works in append mode: after the truncate,
/// each writer's next write lands at the new end. A writer with its own offset would leave a
/// hole the size of everything dropped, so the caller must check for `O_APPEND` first. Lines
/// written between the read and the truncate are lost; the marker says the cut happened.
pub fn bound_in_place(
    path: &Path,
    writer: &std::fs::File,
    cap: u64,
    head: u64,
    tail: u64,
) -> std::io::Result<bool> {
    use std::os::unix::fs::FileExt;
    let len = writer.metadata()?.len();
    if len <= cap || head + tail >= len {
        return Ok(false);
    }
    let f = std::fs::File::open(path)?;
    let mut first = vec![0u8; head as usize];
    f.read_exact_at(&mut first, 0)?;
    // End the head on a line boundary, and start the tail on one.
    first.truncate(first.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1));
    let mut last = vec![0u8; tail as usize];
    f.read_exact_at(&mut last, len - tail)?;
    let cut = last.iter().position(|b| *b == b'\n').map_or(0, |i| i + 1);
    let dropped = len - first.len() as u64 - (last.len() - cut) as u64;

    let mut out = first;
    out.extend_from_slice(
        format!("[limina] ---- {dropped} bytes dropped to keep this log under {cap} ----\n")
            .as_bytes(),
    );
    out.extend_from_slice(&last[cut..]);
    writer.set_len(0)?;
    (&mut &*writer).write_all(&out)?;
    Ok(true)
}

/// How often a run checks its own log against [`LIVE_CAP_BYTES`].
const LIVE_CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// The path of the file behind `fd` when it can be bounded in place: a regular file opened for
/// append (see [`bound_in_place`] for why nothing else can be).
fn boundable(fd: std::os::fd::RawFd) -> Option<PathBuf> {
    // SAFETY: fstat/fcntl only read the descriptor's state.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 || (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
        return None;
    }
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || flags & libc::O_APPEND == 0 {
        return None;
    }
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    if unsafe { libc::fcntl(fd, libc::F_GETPATH, buf.as_mut_ptr()) } != 0 {
        return None;
    }
    let len = buf.iter().position(|b| *b == 0)?;
    buf.truncate(len);
    Some(PathBuf::from(std::ffi::OsString::from_vec(buf)))
}

/// Keep this run's log bounded while it runs, when stderr is an append-mode file — the
/// control center's `logs/supervisor.log`, which the worker writes through the same open file.
/// A terminal, a pipe or a file opened without append is left alone.
pub fn bound_stderr_for_this_run() {
    let Some(path) = boundable(libc::STDERR_FILENO) else {
        return;
    };
    // SAFETY: dup of our own stderr; the new descriptor is owned by the File.
    let fd = unsafe { libc::dup(libc::STDERR_FILENO) };
    if fd < 0 {
        return;
    }
    let writer = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd) };
    let spawned = std::thread::Builder::new()
        .name("log-bound".into())
        .spawn(move || {
            let mut warned = false;
            loop {
                std::thread::sleep(LIVE_CHECK_EVERY);
                match bound_in_place(
                    &path,
                    &writer,
                    LIVE_CAP_BYTES,
                    LIVE_HEAD_BYTES,
                    LIVE_TAIL_BYTES,
                ) {
                    Ok(true) => log::info!(
                        "log: cut {} back under {LIVE_CAP_BYTES} bytes",
                        path.display()
                    ),
                    Ok(false) => {}
                    Err(e) if !warned => {
                        warned = true;
                        log::warn!("log: cannot keep {} bounded: {e}", path.display());
                    }
                    Err(_) => {}
                }
            }
        });
    if let Err(e) = spawned {
        log::warn!("log: no size bound for this run ({e})");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only an append-mode regular file can be cut back under live writers.
    #[test]
    fn only_an_append_mode_file_is_bounded() {
        use std::os::fd::AsRawFd;
        let dir = std::env::temp_dir().join(format!("limina-boundable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("supervisor.log");
        let append = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        assert_eq!(
            boundable(append.as_raw_fd()).map(|p| p.canonicalize().unwrap()),
            Some(path.canonicalize().unwrap())
        );
        let plain = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        assert_eq!(
            boundable(plain.as_raw_fd()),
            None,
            "a writer with its own offset"
        );
        let (r, _w) = std::os::unix::net::UnixStream::pair().unwrap();
        assert_eq!(boundable(r.as_raw_fd()), None, "not a file");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A long run's log stays bounded, keeps its boot and its latest lines, and the next write —
    /// from any holder of the shared append-mode descriptor — lands right after them rather than
    /// at the old length.
    #[test]
    fn a_live_log_is_cut_back_in_place_and_keeps_appending() {
        let dir = std::env::temp_dir().join(format!("limina-livebound-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("supervisor.log");
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        // A second descriptor on the same open file, as the worker holds.
        let worker = f.try_clone().unwrap();
        (&f).write_all(b"boot line\n").unwrap();
        for i in 0..400 {
            (&worker)
                .write_all(format!("[LIMINA] stats line {i:04}\n").as_bytes())
                .unwrap();
        }
        assert!(
            !bound_in_place(&path, &f, 1_000_000, 100, 100).unwrap(),
            "under the cap: untouched"
        );

        assert!(bound_in_place(&path, &f, 2_000, 100, 300).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("boot line\n"), "{text}");
        assert!(text.contains("bytes dropped"), "{text}");
        assert!(text.ends_with("[LIMINA] stats line 0399\n"), "{text}");
        assert!(text.len() < 500, "{} bytes", text.len());
        // Every kept line is whole.
        for line in text.lines() {
            assert!(
                line == "boot line"
                    || line.starts_with("[limina] ----")
                    || line.starts_with("[LIMINA] stats line "),
                "torn line {line:?}"
            );
        }

        (&worker).write_all(b"after the cut\n").unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(after.len(), text.len() + "after the cut\n".len(), "no hole");
        assert!(after.ends_with("0399\nafter the cut\n"));
        std::fs::remove_dir_all(&dir).ok();
    }

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

    /// An oversized file that is ALREADY a generation gets trimmed too. Capping only the live
    /// file let one ride along untouched until it aged out, which left 7.2 GB on the dogfood
    /// Mac after the cap had shipped.
    #[test]
    fn an_older_generation_is_capped_when_it_shifts() {
        let dir = std::env::temp_dir().join(format!("limina-logrot-shift-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let live = dir.join("supervisor.log");
        let cap = 1024u64;

        // A fat .1 already on disk, as if written before the cap existed.
        let mut fat = String::new();
        for i in 0..2000 {
            fat.push_str(&format!("old {i} padding padding padding padding\n"));
        }
        std::fs::write(dir.join("supervisor.1.log"), &fat).unwrap();
        std::fs::write(&live, "new run\n").unwrap();

        rotate_capped(&live, GENERATIONS, cap);

        let shifted = std::fs::read_to_string(dir.join("supervisor.2.log")).unwrap();
        assert!(
            (shifted.len() as u64) < fat.len() as u64,
            "the fat generation must be trimmed as it shifts, got {} bytes",
            shifted.len()
        );
        assert!(shifted.contains("old 1999 "), "its tail is what survives");
        assert!(
            shifted
                .lines()
                .next()
                .unwrap()
                .contains("earlier bytes dropped"),
            "and it says so"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("supervisor.1.log")).unwrap(),
            "new run\n"
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
