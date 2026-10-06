// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The control center's pure-Rust view model: one snapshot of the VM library.
//!
//! Kept AppKit-free so it is unit-testable; the controller diffs consecutive
//! snapshots and only rebuilds the row views when something actually changed.

use super::live;
use crate::vmlib::{bundle, preflight, runtime, schema};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmRow {
    pub bundle: bundle::VmBundle,
    /// The optional identity override, falling back to the bundle directory name.
    pub name: String,
    pub running: bool,
    /// Supervisor pid when running (0 = unknown-but-running).
    pub pid: i32,
    /// "8 vCPU · 4G..12G" — or the load error for broken bundles.
    pub summary: String,
    /// "root.raw (40 GB) · data.raw (ro)" — the disks/cdroms line ("" = none).
    pub disks: String,
    /// The SSH command ("ssh -p 4444 127.0.0.1") when running with networking, or
    /// the configured port ("port 4444" / "auto") when stopped. None = no network.
    pub ssh: Option<String>,
    /// vm.toml failed to load/validate: show the row (with the error) but offer no
    /// lifecycle actions except Delete.
    pub broken: bool,
    /// Why Start cannot work right now — pre-flight's first blocker, already phrased for a
    /// person. `None` means nothing known is wrong. Only computed while stopped: a running
    /// VM holds its own disk locks, and "why can't it start" is not a question about it.
    pub blocked: Option<String>,
    /// Stopped with a suspended session waiting: Start resumes it. Read from the snapshot file,
    /// which IS the resume-pending record (`supervisor::take_pending_resume`); `state.toml`'s
    /// `[suspended]` is status only and can be stale.
    pub suspended: bool,
}

/// Snapshot the whole library. A missing library is an empty list; an unreadable
/// bundle is a `broken` row, never an error (the center must always come up).
pub fn snapshot() -> Vec<VmRow> {
    let bundles = match bundle::list() {
        Ok(b) => b,
        Err(e) => {
            log::warn!("control center: cannot read the VM library: {e:#}");
            return Vec::new();
        }
    };
    let rows: Vec<VmRow> = bundles
        .into_iter()
        .map(|b| {
            let (running, pid) = match runtime::status(&b) {
                runtime::VmStatus::Running { pid } => (true, pid),
                runtime::VmStatus::Stopped => (false, 0),
            };
            let live_port = if running {
                live::info(pid).and_then(|i| i.ssh_port)
            } else {
                None
            };
            let suspended = !running && b.snapshot_bin().exists();
            match b.load() {
                Ok(cfg) => VmRow {
                    name: b.display_name(&cfg),
                    summary: summarize(&cfg),
                    disks: disks_line(&b, &cfg),
                    ssh: ssh_line(&cfg, running, live_port),
                    blocked: (!running).then(|| blocking_reason(&b, &cfg)).flatten(),
                    running,
                    pid,
                    broken: false,
                    suspended,
                    bundle: b,
                },
                Err(e) => VmRow {
                    name: b.dir_name(),
                    summary: format!("broken: {e:#}"),
                    disks: String::new(),
                    ssh: None,
                    blocked: Some(format!("{e:#}")),
                    running,
                    pid,
                    broken: true,
                    suspended,
                    bundle: b,
                },
            }
        })
        .collect();
    let running: Vec<i32> = rows.iter().filter(|r| r.running).map(|r| r.pid).collect();
    live::retain(&running);
    rows
}

/// Pre-flight's verdict for the row, at the depth a 1 s refresh can afford: stat-only, no
/// opening every disk read-write and no port probes (those wait for the click).
fn blocking_reason(bundle: &bundle::VmBundle, cfg: &schema::VmConfig) -> Option<String> {
    preflight::check(bundle, cfg, preflight::Depth::Cheap)
        .first_blocker()
        .map(|f| f.to_string())
}

fn summarize(cfg: &schema::VmConfig) -> String {
    let mem = cfg.hardware.memory.0.clone();
    format!("{} vCPU · {mem}", cfg.hardware.cpus)
}

/// One line describing the attached storage: in-bundle disks by file name with their
/// size, external disks by path, cdroms marked `iso:`.
fn disks_line(bundle: &bundle::VmBundle, cfg: &schema::VmConfig) -> String {
    let mut parts: Vec<String> = Vec::new();
    for d in &cfg.disks {
        let resolved = bundle.resolve_path(&d.path);
        let shown = if d.path.is_absolute() {
            d.path.display().to_string()
        } else {
            d.path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| d.path.display().to_string())
        };
        let size = std::fs::metadata(&resolved)
            .map(|m| format!(" ({})", human_size(m.len())))
            .unwrap_or_else(|_| " (missing)".into());
        let ro = if d.ro { " ro" } else { "" };
        parts.push(format!("{shown}{size}{ro}"));
    }
    for c in &cfg.cdroms {
        let shown = c
            .path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| c.path.display().to_string());
        parts.push(format!("iso: {shown}"));
    }
    parts.join(" · ")
}

/// The SSH line — the actual copyable command whenever the port is knowable.
/// For a running VM the truth is what its supervisor reports (`live_port`: the port
/// auto-allocates from 2222 when not pinned, and can move while it runs), falling
/// back to the pinned port until the first report. A stopped VM with a pinned port
/// shows the same command (it's what to use after Start); auto-port + stopped is
/// the one unknowable case.
fn ssh_line(cfg: &schema::VmConfig, running: bool, live_port: Option<u16>) -> Option<String> {
    let net = cfg.networks.first()?;
    let pinned = (net.ssh_port != 0).then_some(net.ssh_port);
    let port = if running {
        live_port.or(pinned)
    } else {
        pinned
    };
    match (port, running) {
        (Some(p), _) => Some(format!("ssh -p {p} 127.0.0.1")),
        (None, false) => Some("ssh: port assigned at start".into()),
        // Running, but its supervisor has not reported yet (or cannot: a build from
        // before the runtime socket).
        (None, true) => Some("ssh: port not yet known".into()),
    }
}

fn human_size(bytes: u64) -> String {
    const G: u64 = 1024 * 1024 * 1024;
    const M: u64 = 1024 * 1024;
    if bytes >= 10 * G {
        format!("{} GB", bytes / G)
    } else if bytes >= G {
        format!("{:.1} GB", bytes as f64 / G as f64)
    } else {
        format!("{} MB", bytes.div_ceil(M).max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vmlib::import::{CreateOpts, ImportMode, create};
    use crate::vmlib::schema::Memory;

    #[test]
    fn snapshot_lists_vms_and_tolerates_broken_bundles() {
        let _guard = crate::vmlib::bundle::tests::env_lock();
        let lib = crate::vmlib::bundle::tests::scratch_library("model");
        unsafe { std::env::set_var("LIMINA_VM_LIBRARY", &lib) };

        let src = lib.join("img.raw");
        std::fs::write(&src, vec![0u8; 2 * 1024 * 1024]).unwrap();
        create(
            &CreateOpts {
                name: "Alpha".into(),
                disk: Some(src),
                import_mode: ImportMode::CloneIntoBundle,
                blank_size: None,
                cdrom: None,
                cpus: 2,
                memory: Memory("2G".into()),
                ssh_port: 2299,
                window: true,
            },
            &lib,
        )
        .unwrap();
        // A "bundle" whose vm.toml is garbage must surface as a broken row.
        let bad = lib.join("Trash.liminavm");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("vm.toml"), "not [valid").unwrap();

        let rows = snapshot();
        assert_eq!(rows.len(), 2);
        let alpha = rows.iter().find(|r| r.name == "Alpha").unwrap();
        assert!(!alpha.broken);
        assert!(!alpha.running);
        assert_eq!(alpha.summary, "2 vCPU · 2G");
        assert_eq!(alpha.disks, "root.raw (2 MB)");
        assert_eq!(alpha.ssh.as_deref(), Some("ssh -p 2299 127.0.0.1"));
        let trash = rows.iter().find(|r| r.name == "Trash").unwrap();
        assert!(trash.broken);
        assert!(trash.summary.starts_with("broken:"), "{}", trash.summary);

        unsafe { std::env::remove_var("LIMINA_VM_LIBRARY") };
        std::fs::remove_dir_all(&lib).ok();
    }

    /// The row must carry *why* Start is unavailable, not merely render "(missing)" in the
    /// disks line and leave the button live.
    #[test]
    fn a_vm_whose_disk_is_gone_reports_why_it_cannot_start() {
        let _guard = crate::vmlib::bundle::tests::env_lock();
        let lib = crate::vmlib::bundle::tests::scratch_library("blocked");
        unsafe { std::env::set_var("LIMINA_VM_LIBRARY", &lib) };

        let src = lib.join("img.raw");
        std::fs::write(&src, vec![0u8; 2 * 1024 * 1024]).unwrap();
        let mut opts = crate::vmlib::bundle::tests::basic_opts("Alpha");
        opts.disk = Some(src);
        let bundle = create(&opts, &lib).unwrap();

        // The row reports the first blocker, and whether this host has gvproxy or a built
        // GOP firmware is not what this test is about: satisfy both with stand-ins so the
        // only thing that can block is the definition itself.
        let gvproxy = lib.join("gvproxy");
        std::fs::write(&gvproxy, b"").unwrap();
        unsafe { std::env::set_var("LIMINA_GVPROXY_BIN", &gvproxy) };
        let firmware = lib.join("firmware.fd");
        std::fs::write(&firmware, b"").unwrap();
        let mut cfg = bundle.load().unwrap();
        cfg.boot.firmware = Some(firmware);
        bundle.save(&cfg).unwrap();

        // Healthy: nothing to report about starting.
        let row = snapshot().into_iter().find(|r| r.name == "Alpha").unwrap();
        assert_eq!(row.blocked, None, "{:?}", row.disks);

        std::fs::remove_file(bundle.resolve_path(&cfg.disks[0].path)).unwrap();

        let row = snapshot().into_iter().find(|r| r.name == "Alpha").unwrap();
        assert!(!row.broken, "a missing disk is not a broken definition");
        assert_eq!(row.disks, "root.raw (missing)");
        let why = row
            .blocked
            .expect("the row must say why Start is unavailable");
        assert!(why.contains("not found"), "{why}");

        unsafe { std::env::remove_var("LIMINA_GVPROXY_BIN") };
        unsafe { std::env::remove_var("LIMINA_VM_LIBRARY") };
        std::fs::remove_dir_all(&lib).ok();
    }

    /// The row offers Resume and Discard exactly while a snapshot is waiting.
    #[test]
    fn a_stopped_vm_with_a_snapshot_reads_as_suspended() {
        let _guard = crate::vmlib::bundle::tests::env_lock();
        let lib = crate::vmlib::bundle::tests::scratch_library("suspended-row");
        unsafe { std::env::set_var("LIMINA_VM_LIBRARY", &lib) };
        let bundle = create(&crate::vmlib::bundle::tests::basic_opts("Alpha"), &lib).unwrap();

        let row = snapshot().into_iter().find(|r| r.name == "Alpha").unwrap();
        assert!(!row.suspended);

        std::fs::create_dir_all(bundle.run_dir()).unwrap();
        std::fs::write(bundle.snapshot_bin(), b"snapshot").unwrap();
        let row = snapshot().into_iter().find(|r| r.name == "Alpha").unwrap();
        assert!(row.suspended);

        bundle.discard_suspend().unwrap();
        let row = snapshot().into_iter().find(|r| r.name == "Alpha").unwrap();
        assert!(!row.suspended);

        unsafe { std::env::remove_var("LIMINA_VM_LIBRARY") };
        std::fs::remove_dir_all(&lib).ok();
    }

    #[test]
    fn ssh_line_prefers_the_supervisors_port_when_running() {
        let _guard = crate::vmlib::bundle::tests::env_lock();
        let lib = crate::vmlib::bundle::tests::scratch_library("sshline");
        let mut opts = crate::vmlib::bundle::tests::basic_opts("Net");
        opts.ssh_port = 0;
        let cfg = create(&opts, &lib).unwrap().load().unwrap();

        // Stopped, auto port: no command to show yet.
        assert_eq!(
            ssh_line(&cfg, false, None).as_deref(),
            Some("ssh: port assigned at start")
        );
        // Running, before the supervisor reports.
        assert_eq!(
            ssh_line(&cfg, true, None).as_deref(),
            Some("ssh: port not yet known")
        );
        // Running: the supervisor's port wins.
        assert_eq!(
            ssh_line(&cfg, true, Some(2223)).as_deref(),
            Some("ssh -p 2223 127.0.0.1")
        );

        // Pinned: shown while stopped, and until the supervisor reports a port it moved to.
        let mut pinned = cfg.clone();
        pinned.networks[0].ssh_port = 2299;
        assert_eq!(
            ssh_line(&pinned, false, None).as_deref(),
            Some("ssh -p 2299 127.0.0.1")
        );
        assert_eq!(
            ssh_line(&pinned, true, None).as_deref(),
            Some("ssh -p 2299 127.0.0.1")
        );
        assert_eq!(
            ssh_line(&pinned, true, Some(2300)).as_deref(),
            Some("ssh -p 2300 127.0.0.1")
        );

        std::fs::remove_dir_all(&lib).ok();
    }

    #[test]
    fn human_sizes_read_naturally() {
        assert_eq!(human_size(40 * 1024 * 1024 * 1024), "40 GB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024 / 2), "1.5 GB");
        assert_eq!(human_size(2 * 1024 * 1024), "2 MB");
    }
}
