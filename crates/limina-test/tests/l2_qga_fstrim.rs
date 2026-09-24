// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L2 — **a guest trim gives host disk back.**
//!
//! A raw disk image only ever grows. Deleting a file inside the guest frees blocks in the
//! guest's filesystem and nothing else; the host file keeps every block it ever allocated
//! unless the guest also *discards* the range, which travels as virtio-blk
//! `VIRTIO_BLK_T_DISCARD` and lands in our imago fork's punch-hole. The supervisor drives that
//! on a long cadence through the stock `qemu-guest-agent` (`crates/limina/src/qga/trim.rs`),
//! because only the guest knows which blocks are free.
//!
//! # Why the root is remounted `nodiscard` first
//!
//! Fedora mounts its btrfs root `discard=async` and enables `fstrim.timer`, so freed extents
//! come back **on their own** within seconds (measured: `spikes/qga-fstrim/RESULTS.md`). A
//! shrink measured on a stock mount would therefore prove nothing about our code — the guest
//! would have done it unprompted. Remounting `nodiscard` takes that away, which makes the
//! middle assertion (freed but *not* returned) a real part of the oracle rather than a
//! formality: it pins that the shrink at the end came from our trim.
//!
//! The root is also the only honest vehicle here. `qemu-ga` is SELinux-confined and cannot
//! open a filesystem at an unlabelled mount point — a scratch `ext4` at `/mnt/…` answers
//! `failed to open: Permission denied` while the guest's real filesystems trim fine beside
//! it. Test on the filesystem production trims, with the labels production has.
//!
//! # Why the desktop is quieted first
//!
//! The host can only hand back blocks that are **still free when the trim runs**. A seated
//! F44 guest stages a full offline system update within minutes of login — `/var/lib/dnf`
//! grew 4708 → 5707 MiB inside this test's own trim window — and btrfs puts that gigabyte
//! straight back into the block group the deleted payload just vacated. The host then cannot
//! return those blocks, and from the host side that is indistinguishable from a discard path
//! that does not work. It made this test a coin flip: six measured runs recovered between
//! 1007 and 1083 MiB against a 1024 MiB bar, passing and failing on tens of MiB.
//!
//! [`quiesce_desktop`](limina_test::quiesce_desktop) stops it, and oracle 4 below asserts
//! that it worked — so if the update machinery changes shape again, the test says *that*
//! instead of blaming the discard path. `spikes/qga-fstrim/RESULTS.md` has the measurements.
//!
//! Oracles, in order:
//! 1. Writing into the guest's filesystem grows the host image.
//! 2. Deleting it inside the guest does **not** shrink the host image.
//! 3. The supervisor's periodic trim fires and says what it did.
//! 4. The guest did not spend the freed space again while we waited.
//! 5. …and now the host image has shrunk back.
//!
//! Host allocation is read as `st_blocks`, never `fstrim -v`'s own number: the agent reports
//! the size of the ranges it *walked*, which was 25.7 GiB in a run that recovered 958 MiB.
//! The guest's own consumption is read with `df`, which is a different kind of number — what
//! the filesystem holds, not what a trim claims to have walked.

use limina_test::{Guest, GuestConfig};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, Instant};

/// How much to write and then delete. Comfortably past any noise the filesystem's own
/// metadata makes, and small enough to write in seconds.
const PAYLOAD_MIB: u64 = 2048;

/// The trim cadence for this run. It also sets the settle delay
/// (`trim::SETTLE.min(interval)`), which is what this test really buys with it: the settle is
/// a guaranteed quiet window from the moment the port is attached, and the whole setup —
/// boot, remount, write, delete — has to finish inside it for oracle 2 to mean anything.
/// Measured at ~120 s on an idle host, so this leaves room without dragging the suite.
const TRIM_SECS: &str = "200";

/// Where the payload lives in the guest.
const PAYLOAD: &str = "/var/tmp/trim-payload";

/// Run a guest command, tolerating a non-zero exit and a transient ssh failure.
fn ssh_soft(guest: &Guest, cmd: &str) -> String {
    let wrapped = format!("{{ {cmd} ; }} 2>&1 || true");
    let mut last_err = String::new();
    for _ in 0..4 {
        match guest.ssh_exec_timeout(&wrapped, Duration::from_secs(180)) {
            Ok(out) => return out.trim().to_string(),
            Err(e) => last_err = e.to_string(),
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    panic!("ssh `{cmd}` kept failing: {last_err}");
}

/// Blocks actually allocated to the host file, in MiB. `len()` is the *apparent* size and is
/// useless here — the file is sparse and its apparent size never changes.
fn allocated_mib(path: &Path) -> u64 {
    let md = std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("stat-ing the data disk {}: {e}", path.display()));
    md.blocks() * 512 / (1024 * 1024)
}

/// What the guest itself is using on the filesystem the payload lives on, in MiB.
///
/// Read at the delete and again after the trim, this is what separates "the discard path is
/// broken" from "the guest spent the payload's space again before we asked for it back" —
/// two situations the host file's allocation alone cannot tell apart.
fn guest_used_mib(guest: &Guest) -> u64 {
    let out = ssh_soft(guest, "df -B1048576 --output=used / | tail -1");
    out.trim()
        .parse()
        .unwrap_or_else(|e| panic!("reading the guest's used space from {out:?}: {e}"))
}

/// How many trims the supervisor has completed so far.
fn trims_logged(guest: &Guest) -> usize {
    guest
        .supervisor_log()
        .lines()
        .filter(|l| l.contains("qga: trimmed"))
        .count()
}

/// The supervisor's `qga:` lines, for a failure message.
fn qga_lines(guest: &Guest) -> String {
    let log = guest.supervisor_log();
    let lines: Vec<&str> = log.lines().filter(|l| l.contains("qga:")).collect();
    if lines.is_empty() {
        "(the supervisor logged nothing about the guest agent)".to_string()
    } else {
        lines.join("\n")
    }
}

#[test]
fn a_periodic_guest_trim_returns_free_blocks_to_the_host_image() {
    let name = "a_periodic_guest_trim_returns_free_blocks_to_the_host_image";
    if !limina_test::require_hvf_or_skip(name) {
        return;
    }
    let cfg = match GuestConfig::seated_efi_fedora_from_env() {
        Ok(cfg) => cfg
            // The image boots to gdm; without a display device gdm dies and systemd restarts
            // it forever, which is IO noise next to a disk-allocation assertion.
            .with_coexist_display(1280, 800)
            .with_net()
            .with_supervisor_log()
            .with_env("LIMINA_QGA_TRIM_SECS", TRIM_SECS)
            .with_env("LIMINA_QGA_TRACE", "1")
            // The trim's own "not now, because…" lines are debug. Without them a failure at
            // oracle 3 cannot say whether the tick never ran, the agent refused, or the idle
            // gate deferred — and that is a re-run this test should never need.
            .with_env("RUST_LOG", "limina=info,limina::control=debug"),
        Err(e) => {
            eprintln!("SKIPPED {name}: {e:#}");
            return;
        }
    };

    let mut guest = Guest::boot(&cfg).expect("spawning the limina supervisor");
    let banner = guest
        .wait_for_ssh(Duration::from_secs(300))
        .expect("guest sshd never became reachable through gvproxy");
    eprintln!("guest SSH up: {banner}");

    // `with_net` boots a writable COW clone of the shared image; that clone is what the
    // guest's writes and discards actually land in.
    let disk = guest.scratch_dir().join("disk.raw");
    assert!(
        disk.exists(),
        "the harness did not put the boot disk at {} — the net-boot clone's name changed and \
         this test's host-side oracle is reading nothing",
        disk.display()
    );

    // Before ANY measurement: a fresh desktop's update machinery would otherwise download a
    // gigabyte into the very space this test is about to free. See the module doc.
    limina_test::quiesce_desktop(&guest);

    let rpm = ssh_soft(&guest, "rpm -q qemu-guest-agent");
    if !rpm.contains("qemu-guest-agent-") {
        eprintln!(
            "SKIPPED {name}: this image has no qemu-guest-agent ({rpm}). Fedora's comps make \
             it mandatory in every desktop variant, so an image without it is a property of \
             the image, not a failure."
        );
        guest.shutdown(Duration::from_secs(60)).ok();
        return;
    }

    // Take away the guest's own continuous discard, so nothing returns a block unprompted.
    let setup = ssh_soft(
        &guest,
        "sudo mount -o remount,nodiscard / && findmnt -no FSTYPE,OPTIONS /",
    );
    // btrfs reports `nodiscard` as the *absence* of a `discard=` option, not as a word.
    assert!(
        setup.contains("btrfs") && !setup.contains("discard="),
        "could not take discard=async off the guest's root, so the control below would not \
         hold and a shrink would prove nothing. findmnt said: {setup}"
    );
    eprintln!("guest root: {setup}");
    let floor = allocated_mib(&disk);

    // --- Oracle 1: writing grows the host image ---
    ssh_soft(
        &guest,
        &format!(
            "sudo dd if=/dev/urandom of={PAYLOAD} bs=1M count={PAYLOAD_MIB} status=none; sync"
        ),
    );
    let filled = allocated_mib(&disk);
    eprintln!("host allocation: floor {floor} MiB → filled {filled} MiB");
    assert!(
        filled >= floor + PAYLOAD_MIB / 2,
        "writing {PAYLOAD_MIB} MiB in the guest grew the host image by only {} MiB \
         (floor {floor}, filled {filled}) — the guest's writes are not reaching the backing \
         file, so nothing below this can mean anything",
        filled.saturating_sub(floor)
    );

    // --- Oracle 2: deleting does NOT shrink it ---
    //
    // This is the control. Without it, a shrink at the end could just be the guest's own
    // discard machinery, and the test would pass with our trim ripped out.
    ssh_soft(&guest, &format!("sudo rm -f {PAYLOAD}; sync"));
    // Long enough for a discard to have happened if one were going to, and long enough for
    // the PSI avg10 the idle gate reads to decay after the write.
    std::thread::sleep(Duration::from_secs(30));
    let deleted = allocated_mib(&disk);
    // The control only holds inside the settle window, before the first periodic trim. If
    // setup overran it, say exactly that instead of blaming the discard path — an earlier
    // draft measured a 13722 MiB "delete" that was really our own trim landing mid-window.
    assert_eq!(
        trims_logged(&guest),
        0,
        "setup overran the {TRIM_SECS}s settle window and a trim already ran, so this \
         control cannot be measured. Raise TRIM_SECS."
    );
    assert!(
        deleted >= floor + PAYLOAD_MIB / 2,
        "the host image shrank to {deleted} MiB on the delete alone (floor {floor}). This \
         filesystem was mounted nodiscard precisely so it would not — if that stopped being \
         true, oracle 4 below no longer proves the trim did anything and this test needs a \
         new vehicle"
    );
    eprintln!("host allocation after delete: {deleted} MiB (still held, as intended)");
    let used_at_delete = guest_used_mib(&guest);

    // --- Oracle 3: the supervisor's periodic trim runs ---
    let before = trims_logged(&guest);
    // Three cadences: the idle gate legitimately defers, because the trim raises the guest's
    // own IO pressure and the tick right after one is usually refused.
    let deadline = Instant::now() + Duration::from_secs(3 * TRIM_SECS.parse::<u64>().unwrap());
    while trims_logged(&guest) == before && Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(5));
    }
    assert!(
        trims_logged(&guest) > before,
        "the supervisor never ran a trim within {:?} of a {TRIM_SECS}s cadence. Either the \
         tick is not wired (crates/limina/src/control.rs, qga_trim_tick), the agent does not \
         offer guest-fstrim, or the idle gate is refusing. qga log lines:\n{}",
        deadline.elapsed(),
        qga_lines(&guest)
    );

    // The punch-hole is issued as the agent walks the free extents, so the shrink can trail
    // the log line slightly.
    let mut trimmed = allocated_mib(&disk);
    let settle = Instant::now() + Duration::from_secs(60);
    while trimmed > floor + PAYLOAD_MIB / 2 && Instant::now() < settle {
        std::thread::sleep(Duration::from_secs(5));
        trimmed = allocated_mib(&disk);
    }
    eprintln!("host allocation after the trim: {trimmed} MiB");

    // --- Oracle 4: the guest did not spend the freed space again ---
    //
    // The control for oracle 5, and the reason this test stopped being a coin flip. Without
    // it, a guest that re-used the payload's extents reads as a broken discard path.
    let respent = guest_used_mib(&guest).saturating_sub(used_at_delete);
    eprintln!("guest re-used {respent} MiB of its own while we waited for the trim");
    assert!(
        respent < PAYLOAD_MIB / 4,
        "the guest allocated {respent} MiB of its own between the delete and the trim, so the \
         payload's space was taken back before it could be returned and this run cannot \
         measure the discard path at all. Something the guest does in the background is no \
         longer being stopped by limina_test::quiesce_desktop — as of 2026-09-24 that was \
         gnome-software staging an offline dnf5 update, ~1 GiB into /var/lib/dnf. Find the \
         new one with `du -xsm /var/* /var/lib/*` either side of the wait; \
         spikes/qga-fstrim/RESULTS.md has the method."
    );

    // --- Oracle 5: …and the blocks came back ---
    assert!(
        trimmed < deleted - PAYLOAD_MIB / 2,
        "the trim ran but the host image gave back only {} MiB of the {PAYLOAD_MIB} MiB \
         payload (holds {trimmed} MiB, was {deleted} before the trim, floor {floor}), and \
         the guest re-used only {respent} MiB meanwhile — so the space really was free and \
         really was not returned. Check virtio-blk's VIRTIO_BLK_T_DISCARD arm and imago's \
         punch-hole; `spikes/qga-fstrim/second-trim-probe.sh` reproduces the path without \
         the supervisor. qga log lines:\n{}",
        deleted.saturating_sub(trimmed),
        qga_lines(&guest)
    );

    guest
        .shutdown(Duration::from_secs(90))
        .expect("the guest did not power off");
}
