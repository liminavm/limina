// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Pre-flight: decide, before spawning anything, whether a managed VM can start.
//!
//! Implements Layer 1 of `docs/design/vm-start-preflight.md`. Pure supervisor-side policy —
//! no AppKit, no process spawning — so every check is unit-testable without HVF.
//!
//! Four invariants make the result trustworthy rather than one more thing to distrust:
//!
//! 1. **Read-only.** Nothing here creates, touches, or leaves a lock held. A pre-flight that
//!    "helpfully" repaired state would re-create the shadow-library hazard that
//!    `vm-definitions.md` §8.4 exists to prevent.
//! 2. **Conservative bias.** A check that cannot *prove* a failure emits a [`Severity::Warning`],
//!    never a [`Severity::Blocker`]. Pre-flight must never be the reason a VM that would have
//!    booted does not — so an inconclusive probe (an unopenable file, an unknown errno) yields
//!    nothing at all.
//! 3. **Advisory; the start path stays authoritative.** There is an irreducible gap between
//!    checking and spawning, so `run_vm` keeps every check it already had. This front-runs
//!    them, it does not replace them.
//! 4. **One implementation per check.** [`validate_disk_path`] and [`canonical_key`] live here
//!    and the start path calls them, so the pre-spawn refusal and the supervisor's own error
//!    are the same string rather than two that drift.
//!
//! Deliberately *not* checked, because degrading is the designed behaviour and a Blocker here
//! would fight the two-tier guarantee: GPU/venus availability, control-plane bind failures,
//! free host disk space (every image is sparse), configured memory versus host RAM, and the
//! worker's codesigning — the `hv_vm_create` call is the honest oracle for that one, so it is
//! Layer 2's job (`center::spawn`) to recognise the error rather than predict it.

use std::fmt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::vmlib::{bundle::VmBundle, runtime, schema::VmConfig};

/// How much work a caller is willing to pay for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// Stat-only. Cheap enough for the control center's 1 s refresh across the whole library.
    Cheap,
    /// Everything, including probes that open files or bind sockets. Click and CLI paths only.
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// The VM will start, but something worth saying is true of how.
    Warning,
    /// Starting cannot succeed; refuse before spawning.
    Blocker,
}

/// The stable identity of a finding. Tests assert on this, never on the prose, so messages
/// stay free to improve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    BundleUnavailable,
    LockUnreadable,
    AlreadyRunning,
    DiskMissing,
    DiskUnreadable,
    DiskNotAFile,
    DiskInUse,
    CdromMissing,
    ImageAttachedTwice,
    ShareMissing,
    ShareNotADirectory,
    ShareIsSymlink,
    CpusZero,
    CpusExceedHost,
    MemoryUnparsable,
    SshPortTooLow,
    SshPortInUse,
    FirmwareMissing,
    FirmwareUnresolvable,
    FirmwareDebugFallback,
    GvproxyMissing,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub code: Code,
    pub severity: Severity,
    /// The resolved absolute path, or the config key — never the relative spelling.
    pub subject: String,
    pub message: String,
    /// What to actually do about it.
    pub remedy: Option<String>,
}

impl Finding {
    fn blocker(code: Code, subject: impl Into<String>, message: impl Into<String>) -> Self {
        Finding {
            code,
            severity: Severity::Blocker,
            subject: subject.into(),
            message: message.into(),
            remedy: None,
        }
    }

    fn warning(code: Code, subject: impl Into<String>, message: impl Into<String>) -> Self {
        Finding {
            code,
            severity: Severity::Warning,
            subject: subject.into(),
            message: message.into(),
            remedy: None,
        }
    }

    fn with_remedy(mut self, remedy: impl Into<String>) -> Self {
        self.remedy = Some(remedy.into());
        self
    }
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(r) = &self.remedy {
            write!(f, " ({r})")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub findings: Vec<Finding>,
}

impl Report {
    pub fn blockers(&self) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Blocker)
    }

    pub fn is_startable(&self) -> bool {
        self.blockers().next().is_none()
    }

    /// The first blocker, for the one-line "why is this row's play button disabled" surface.
    pub fn first_blocker(&self) -> Option<&Finding> {
        self.blockers().next()
    }

    /// Turn blockers into the error a caller returns instead of spawning. Every blocker is
    /// listed — fixing one only to be told about the next is its own kind of silence.
    pub fn ensure_startable(&self) -> Result<()> {
        let blockers: Vec<String> = self.blockers().map(|f| f.to_string()).collect();
        if blockers.is_empty() {
            return Ok(());
        }
        Err(anyhow::anyhow!(blockers.join("\n")))
    }
}

/// Validate a disk/cdrom backing path: it must exist and be a regular file or a block device.
/// Distinguishes "not found" (likely a typo, or wanted `:create`) from "permission denied".
///
/// The start path calls this too (`build_disk_args`), so the refusal the user sees before the
/// spawn and the one the supervisor would have produced are the same text.
pub fn validate_disk_path(path: &Path) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;
    match std::fs::metadata(path) {
        Ok(m) => {
            anyhow::ensure!(
                m.is_file() || m.file_type().is_block_device(),
                "--disk path is not a regular file or block device: {path:?}"
            );
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(anyhow::anyhow!(
            "--disk path not found: {path:?} (pass :create=SIZE to make a new disk)"
        )),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(anyhow::anyhow!(
            "--disk path not accessible (permission denied): {path:?}"
        )),
        Err(e) => Err(e).map_err(|e| anyhow::anyhow!("stat --disk path {path:?}: {e}")),
    }
}

/// The identity of a backing image for duplicate detection: its canonical path when that
/// resolves, else the path as given. Shared with `build_disk_args` so both agree on what
/// "the same image twice" means.
pub fn canonical_key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Run the checks a caller is willing to pay for. Never fails: an inconclusive probe produces
/// no finding rather than a guess (invariant 2).
pub fn check(bundle: &VmBundle, cfg: &VmConfig, depth: Depth) -> Report {
    let mut f = Vec::new();
    check_bundle(bundle, &mut f);
    check_hardware(cfg, &mut f);
    check_images(bundle, cfg, depth, &mut f);
    check_shares(cfg, &mut f);
    check_network(cfg, depth, &mut f);
    check_firmware(bundle, cfg, &mut f);
    if depth == Depth::Full {
        check_running(bundle, &mut f);
    }
    Report { findings: f }
}

fn check_bundle(bundle: &VmBundle, out: &mut Vec<Finding>) {
    if !bundle.path.is_dir() {
        out.push(
            Finding::blocker(
                Code::BundleUnavailable,
                bundle.path.display().to_string(),
                format!(
                    "the VM bundle is not available at {}",
                    bundle.path.display()
                ),
            )
            .with_remedy("the volume holding it may not be mounted"),
        );
        return;
    }
    // `runtime::status` reads an unopenable lock as Stopped, so a bundle we cannot read
    // presents as startable and then fails at acquire. Say so instead.
    let lock = bundle.run_dir().join("lock");
    if let Err(e) = std::fs::File::open(&lock) {
        if e.kind() != std::io::ErrorKind::NotFound {
            out.push(Finding::blocker(
                Code::LockUnreadable,
                lock.display().to_string(),
                format!("cannot read the VM's run lock at {}: {e}", lock.display()),
            ));
        }
    }
}

fn check_running(bundle: &VmBundle, out: &mut Vec<Finding>) {
    if let runtime::VmStatus::Running { pid } = runtime::status(bundle) {
        out.push(Finding::blocker(
            Code::AlreadyRunning,
            bundle.dir_name(),
            format!(
                "{} is already running (supervisor pid {pid})",
                bundle.dir_name()
            ),
        ));
    }
}

fn check_hardware(cfg: &VmConfig, out: &mut Vec<Finding>) {
    if cfg.hardware.cpus == 0 {
        out.push(Finding::blocker(
            Code::CpusZero,
            "hardware.cpus",
            "hardware.cpus is 0; a VM needs at least one vCPU",
        ));
    } else if let Ok(host) = std::thread::available_parallelism() {
        // A Warning, not a Blocker: oversubscribing is legal, merely unwise.
        if usize::from(cfg.hardware.cpus) > host.get() {
            out.push(Finding::warning(
                Code::CpusExceedHost,
                "hardware.cpus",
                format!(
                    "hardware.cpus is {} but the host has {} logical CPUs",
                    cfg.hardware.cpus,
                    host.get()
                ),
            ));
        }
    }
    if let Err(e) = cfg.hardware.memory.max_mib() {
        out.push(Finding::blocker(
            Code::MemoryUnparsable,
            "hardware.memory",
            format!("hardware.memory is not a usable size: {e:#}"),
        ));
    }
}

fn check_images(bundle: &VmBundle, cfg: &VmConfig, depth: Depth, out: &mut Vec<Finding>) {
    let mut seen: Vec<(PathBuf, String)> = Vec::new();
    let entries = cfg
        .disks
        .iter()
        .map(|d| (bundle.resolve_path(&d.path), d.ro, true))
        .chain(
            cfg.cdroms
                .iter()
                .map(|c| (bundle.resolve_path(&c.path), true, false)),
        );

    for (path, ro, is_disk) in entries {
        let shown = path.display().to_string();
        match validate_disk_path(&path) {
            Ok(()) => {}
            Err(e) => {
                let msg = format!("{e:#}");
                let code = if msg.contains("not found") {
                    if is_disk {
                        Code::DiskMissing
                    } else {
                        Code::CdromMissing
                    }
                } else if msg.contains("permission denied") {
                    Code::DiskUnreadable
                } else {
                    Code::DiskNotAFile
                };
                out.push(Finding::blocker(
                    code,
                    shown.clone(),
                    format!(
                        "{} {shown} is not usable: {msg}",
                        if is_disk { "disk" } else { "cdrom" }
                    ),
                ));
                continue;
            }
        }

        let key = canonical_key(&path);
        if let Some((_, first)) = seen.iter().find(|(k, _)| *k == key) {
            out.push(Finding::blocker(
                Code::ImageAttachedTwice,
                shown.clone(),
                format!("{shown} is attached twice (also as {first})"),
            ));
        } else {
            seen.push((key, shown.clone()));
        }

        // Opening every image read-write on the center's refresh would be wasteful, and a
        // running VM holds its own locks — so this is a click/CLI-time check only.
        if depth == Depth::Full && is_disk && !ro && disk_is_locked_elsewhere(&path) {
            out.push(
                Finding::blocker(
                    Code::DiskInUse,
                    shown.clone(),
                    format!("{shown} is already attached read-write to another running VM"),
                )
                .with_remedy("attach it :ro to share it, or stop the other VM"),
            );
        }
    }
}

/// Probe the same advisory lock the worker takes on every writable disk
/// (`limina-vmm`'s `lock_writable_disks`) and release it immediately. Because the lock is on
/// the backing file rather than on a bundle, this sees flat `limina --disk` runs too.
///
/// Conservative: anything other than a definitive `EWOULDBLOCK` reports "not locked", so an
/// unopenable file never becomes a spurious Blocker — `validate_disk_path` has already spoken
/// about that case.
fn disk_is_locked_elsewhere(path: &Path) -> bool {
    let Ok(f) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    else {
        return false;
    };
    // SAFETY: `f` owns a valid fd for the duration of the calls.
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
        return false;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK)
}

fn check_shares(cfg: &VmConfig, out: &mut Vec<Finding>) {
    for s in &cfg.shares {
        // Absoluteness is enforced at load (`VmConfig::validate`), so anything reaching here
        // is absolute and the remaining questions are about the directory itself.
        let shown = s.path.display().to_string();
        match std::fs::symlink_metadata(&s.path) {
            Err(e) => out.push(Finding::blocker(
                Code::ShareMissing,
                shown.clone(),
                format!("share {shown} is not accessible: {e}"),
            )),
            Ok(m) if m.file_type().is_symlink() => out.push(
                Finding::blocker(
                    Code::ShareIsSymlink,
                    shown.clone(),
                    format!("share {shown} is a symlink"),
                )
                .with_remedy("point the share at the real directory"),
            ),
            Ok(m) if !m.is_dir() => out.push(Finding::blocker(
                Code::ShareNotADirectory,
                shown.clone(),
                format!("share {shown} is not a directory"),
            )),
            Ok(_) => {}
        }
    }
}

fn check_network(cfg: &VmConfig, depth: Depth, out: &mut Vec<Finding>) {
    let Some(net) = cfg.networks.first() else {
        return;
    };
    if net.ssh_port != 0 && net.ssh_port < crate::gateway::SSH_PORT_MIN {
        out.push(Finding::blocker(
            Code::SshPortTooLow,
            "network.ssh_port",
            format!(
                "network.ssh_port {} is below {} (gvproxy's floor)",
                net.ssh_port,
                crate::gateway::SSH_PORT_MIN
            ),
        ));
    }
    let gvproxy = crate::gateway::gvproxy_bin();
    if !gvproxy.is_file() {
        out.push(
            Finding::blocker(
                Code::GvproxyMissing,
                gvproxy.display().to_string(),
                format!(
                    "networking needs gvproxy, not found at {}",
                    gvproxy.display()
                ),
            )
            .with_remedy("set LIMINA_GVPROXY_BIN, or install gvproxy"),
        );
    }
    // A bind probe races anything else on the host, so it can only ever be a Warning.
    if depth == Depth::Full && net.ssh_port != 0 && !crate::gateway::port_is_free(net.ssh_port) {
        out.push(Finding::warning(
            Code::SshPortInUse,
            "network.ssh_port",
            format!("host port {} is already in use", net.ssh_port),
        ));
    }
}

fn check_firmware(bundle: &VmBundle, cfg: &VmConfig, out: &mut Vec<Finding>) {
    if let Some(fw) = &cfg.boot.firmware {
        // An explicitly configured firmware bypasses resolution entirely, so nothing else
        // ever checks that it is there.
        let path = bundle.resolve_path(fw);
        if !path.is_file() {
            out.push(Finding::blocker(
                Code::FirmwareMissing,
                path.display().to_string(),
                format!("boot.firmware {} does not exist", path.display()),
            ));
        }
        return;
    }
    if !cfg.display.window {
        out.push(
            Finding::blocker(
                Code::FirmwareUnresolvable,
                "boot.firmware",
                "a headless VM needs an explicit boot.firmware",
            )
            .with_remedy("only windowed boots resolve one automatically"),
        );
        return;
    }
    match crate::resolve_windowed_firmware_for_preflight() {
        None => out.push(Finding::blocker(
            Code::FirmwareUnresolvable,
            "boot.firmware",
            "no EFI firmware could be found for a windowed boot",
        )),
        Some((path, false)) => out.push(
            Finding::warning(
                Code::FirmwareDebugFallback,
                path.display().to_string(),
                format!(
                    "falling back to krunkit's DEBUG firmware at {}",
                    path.display()
                ),
            )
            .with_remedy("its live ASSERTs can wedge a cold boot; build the GOP firmware"),
        ),
        Some((_, true)) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vmlib::bundle::tests::{basic_opts, env_lock, scratch_library};
    use crate::vmlib::import::create;
    use crate::vmlib::schema::ShareEntry;

    /// A bundle whose disk exists, so each test can remove exactly the one thing it is about.
    fn startable(tag: &str) -> (PathBuf, VmBundle, VmConfig) {
        let lib = scratch_library(tag);
        std::env::set_var("LIMINA_VM_LIBRARY", &lib);
        let src = lib.join("seed.raw");
        std::fs::write(&src, vec![0u8; 1024]).unwrap();
        let mut opts = basic_opts("Probe");
        opts.disk = Some(src);
        let bundle = create(&opts, &lib).unwrap();
        let cfg = bundle.load().unwrap();
        (lib, bundle, cfg)
    }

    fn codes(r: &Report) -> Vec<Code> {
        r.findings.iter().map(|f| f.code).collect()
    }

    /// Blockers that come from the definition itself. Whether this host has gvproxy or a
    /// built GOP firmware is not what these tests are about, and asserting on plain
    /// `is_startable()` would make them pass or fail by what happens to be installed.
    fn config_blockers(r: &Report) -> Vec<Code> {
        r.blockers()
            .map(|f| f.code)
            .filter(|c| {
                !matches!(
                    c,
                    Code::GvproxyMissing | Code::FirmwareUnresolvable | Code::FirmwareMissing
                )
            })
            .collect()
    }

    #[test]
    fn a_healthy_bundle_reports_nothing_blocking() {
        let _g = env_lock();
        let (lib, bundle, cfg) = startable("pf-ok");
        let r = check(&bundle, &cfg, Depth::Cheap);
        assert!(config_blockers(&r).is_empty(), "{:?}", codes(&r));
        std::env::remove_var("LIMINA_VM_LIBRARY");
        std::fs::remove_dir_all(&lib).ok();
    }

    /// The originating bug: a bundle copied without its disk. The row must be able to say so
    /// before anything is spawned.
    #[test]
    fn a_missing_disk_blocks_and_names_the_resolved_path() {
        let _g = env_lock();
        let (lib, bundle, cfg) = startable("pf-nodisk");
        let disk = bundle.resolve_path(&cfg.disks[0].path);
        std::fs::remove_file(&disk).unwrap();

        let r = check(&bundle, &cfg, Depth::Cheap);
        assert!(!r.is_startable());
        assert!(codes(&r).contains(&Code::DiskMissing), "{:?}", codes(&r));
        let msg = format!("{:#}", r.ensure_startable().unwrap_err());
        // The absolute path, not the "disks/root.raw" the config spells.
        assert!(msg.contains(disk.to_str().unwrap()), "{msg}");
        assert!(msg.contains(":create=SIZE"), "{msg}");

        std::env::remove_var("LIMINA_VM_LIBRARY");
        std::fs::remove_dir_all(&lib).ok();
    }

    #[test]
    fn a_share_that_is_gone_or_symlinked_blocks() {
        let _g = env_lock();
        let (lib, bundle, mut cfg) = startable("pf-share");

        cfg.shares = vec![ShareEntry {
            name: None,
            path: lib.join("not-there"),
            ro: false,
        }];
        assert!(codes(&check(&bundle, &cfg, Depth::Cheap)).contains(&Code::ShareMissing));

        let target = lib.join("target");
        std::fs::create_dir_all(&target).unwrap();
        let link = lib.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        cfg.shares[0].path = link;
        assert!(codes(&check(&bundle, &cfg, Depth::Cheap)).contains(&Code::ShareIsSymlink));

        cfg.shares[0].path = target;
        assert!(config_blockers(&check(&bundle, &cfg, Depth::Cheap)).is_empty());

        std::env::remove_var("LIMINA_VM_LIBRARY");
        std::fs::remove_dir_all(&lib).ok();
    }

    #[test]
    fn zero_cpus_blocks_but_oversubscription_only_warns() {
        let _g = env_lock();
        let (lib, bundle, mut cfg) = startable("pf-cpus");

        cfg.hardware.cpus = 0;
        assert!(codes(&check(&bundle, &cfg, Depth::Cheap)).contains(&Code::CpusZero));

        // Oversubscribing is legal, merely unwise: it must never block a start.
        cfg.hardware.cpus = 255;
        let r = check(&bundle, &cfg, Depth::Cheap);
        assert!(config_blockers(&r).is_empty(), "{:?}", codes(&r));
        assert!(codes(&r).contains(&Code::CpusExceedHost));

        std::env::remove_var("LIMINA_VM_LIBRARY");
        std::fs::remove_dir_all(&lib).ok();
    }

    #[test]
    fn the_same_image_attached_twice_blocks() {
        let _g = env_lock();
        let (lib, bundle, mut cfg) = startable("pf-dup");
        let dup = cfg.disks[0].clone();
        cfg.disks.push(dup);
        assert!(codes(&check(&bundle, &cfg, Depth::Cheap)).contains(&Code::ImageAttachedTwice));
        std::env::remove_var("LIMINA_VM_LIBRARY");
        std::fs::remove_dir_all(&lib).ok();
    }

    /// A disk held read-write by another VM. The lock is the worker's own, on the backing
    /// file, so this sees flat `limina --disk` runs too -- and it is Full-depth only, since
    /// opening every image read-write on the center's 1 s refresh would be wasteful.
    #[test]
    fn a_disk_locked_by_another_vm_blocks_at_full_depth_only() {
        let _g = env_lock();
        let (lib, bundle, cfg) = startable("pf-lock");
        let disk = bundle.resolve_path(&cfg.disks[0].path);

        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&disk)
            .unwrap();
        // SAFETY: `held` owns a valid fd for the duration of the call.
        assert_eq!(
            unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );

        assert!(codes(&check(&bundle, &cfg, Depth::Full)).contains(&Code::DiskInUse));
        assert!(!codes(&check(&bundle, &cfg, Depth::Cheap)).contains(&Code::DiskInUse));

        drop(held);
        assert!(!codes(&check(&bundle, &cfg, Depth::Full)).contains(&Code::DiskInUse));

        std::env::remove_var("LIMINA_VM_LIBRARY");
        std::fs::remove_dir_all(&lib).ok();
    }

    /// Invariant 1: pre-flight never writes. Running it against a bundle missing `run/` and
    /// `logs/` must not create them -- `vm-definitions.md` §8.4 is the cautionary tale.
    #[test]
    fn checking_creates_nothing() {
        let _g = env_lock();
        let (lib, bundle, cfg) = startable("pf-readonly");
        std::fs::remove_dir_all(bundle.run_dir()).ok();
        std::fs::remove_dir_all(bundle.logs_dir()).ok();

        check(&bundle, &cfg, Depth::Full);

        assert!(!bundle.run_dir().exists(), "pre-flight created run/");
        assert!(!bundle.logs_dir().exists(), "pre-flight created logs/");

        std::env::remove_var("LIMINA_VM_LIBRARY");
        std::fs::remove_dir_all(&lib).ok();
    }

    #[test]
    fn every_blocker_is_listed_not_just_the_first() {
        let _g = env_lock();
        let (lib, bundle, mut cfg) = startable("pf-many");
        std::fs::remove_file(bundle.resolve_path(&cfg.disks[0].path)).unwrap();
        cfg.hardware.cpus = 0;

        let err = format!(
            "{:#}",
            check(&bundle, &cfg, Depth::Cheap)
                .ensure_startable()
                .unwrap_err()
        );
        assert!(err.contains("vCPU"), "{err}");
        assert!(err.contains("not found"), "{err}");

        std::env::remove_var("LIMINA_VM_LIBRARY");
        std::fs::remove_dir_all(&lib).ok();
    }
}
