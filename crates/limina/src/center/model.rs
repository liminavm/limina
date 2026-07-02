// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The control center's pure-Rust view model: one snapshot of the VM library.
//!
//! Kept AppKit-free so it is unit-testable; the controller diffs consecutive
//! snapshots and only rebuilds the row views when something actually changed.

use crate::vmlib::{bundle, runtime, schema};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmRow {
    pub bundle: bundle::VmBundle,
    /// identity.name when the config loads; the directory name for broken bundles.
    pub name: String,
    pub running: bool,
    /// Supervisor pid when running (0 = unknown-but-running).
    pub pid: i32,
    /// "4 vCPU · 4096M · ssh auto" — or the load error for broken bundles.
    pub summary: String,
    /// vm.toml failed to load/validate: show the row (with the error) but offer no
    /// lifecycle actions except Delete.
    pub broken: bool,
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
    bundles
        .into_iter()
        .map(|b| {
            let (running, pid) = match runtime::status(&b) {
                runtime::VmStatus::Running { pid } => (true, pid),
                runtime::VmStatus::Stopped => (false, 0),
            };
            match b.load() {
                Ok(cfg) => VmRow {
                    name: cfg.identity.name.clone(),
                    summary: summarize(&cfg),
                    running,
                    pid,
                    broken: false,
                    bundle: b,
                },
                Err(e) => VmRow {
                    name: b.dir_name(),
                    summary: format!("broken: {e:#}"),
                    running,
                    pid,
                    broken: true,
                    bundle: b,
                },
            }
        })
        .collect()
}

fn summarize(cfg: &schema::VmConfig) -> String {
    let mem = match &cfg.hardware.memory {
        schema::Memory::Fixed(s) => s.clone(),
        schema::Memory::Range { min, max } => format!("{min}..{max}"),
    };
    let ssh = match cfg.networks.first() {
        Some(n) if n.ssh_port != 0 => format!(" · ssh {}", n.ssh_port),
        Some(_) => " · ssh auto".to_string(),
        None => String::new(),
    };
    format!("{} vCPU · {mem}{ssh}", cfg.hardware.cpus)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vmlib::import::{create, CreateOpts, ImportMode};
    use crate::vmlib::schema::Memory;

    #[test]
    fn snapshot_lists_vms_and_tolerates_broken_bundles() {
        let _guard = crate::vmlib::bundle::tests::ENV_LOCK.lock().unwrap();
        let lib = crate::vmlib::bundle::tests::scratch_library("model");
        std::env::set_var("LIMINA_VM_LIBRARY", &lib);

        create(
            &CreateOpts {
                name: "Alpha".into(),
                disk: None,
                import_mode: ImportMode::CloneIntoBundle,
                cpus: 2,
                memory: Memory::Fixed("2G".into()),
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
        assert_eq!(alpha.summary, "2 vCPU · 2G · ssh 2299");
        let trash = rows.iter().find(|r| r.name == "Trash").unwrap();
        assert!(trash.broken);
        assert!(trash.summary.starts_with("broken:"), "{}", trash.summary);

        std::env::remove_var("LIMINA_VM_LIBRARY");
        std::fs::remove_dir_all(&lib).ok();
    }
}
