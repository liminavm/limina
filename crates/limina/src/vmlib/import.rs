// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Creating (and deleting) managed VMs — including importing an existing disk image.
//!
//! Import default is **clone-into-bundle** (Parallels-like ownership): on APFS the
//! clone is `clonefile(2)` — instant and space-shared with the source — with a plain
//! copy fallback across volumes/filesystems. The alternative, reference-in-place,
//! records the absolute source path in vm.toml; deletion never touches
//! absolute-path disks either way (design doc §7).

use anyhow::{Context, Result};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::bundle::{VmBundle, BUNDLE_EXT};
use super::runtime;
use super::schema::{
    mac_for_uuid, rfc3339_utc_now, uuid_v4, DiskEntry, DisplayCfg, Hardware, Identity, Memory,
    NetMode, NetworkEntry, VmConfig, CONFIG_VERSION,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportMode {
    /// Clone/copy the disk into the bundle's `disks/` (the default; self-contained).
    CloneIntoBundle,
    /// Reference the source image where it is (absolute path in vm.toml).
    ReferenceInPlace,
}

#[derive(Debug, Clone)]
pub struct CreateOpts {
    pub name: String,
    /// Existing disk image to import; None = a definition with no disks yet.
    pub disk: Option<PathBuf>,
    pub import_mode: ImportMode,
    pub cpus: u8,
    pub memory: Memory,
    /// 0 = auto-allocate the SSH forward port at start.
    pub ssh_port: u16,
    pub window: bool,
}

/// Materialize a new `.liminavm` bundle under `dest_dir`. Fails if the bundle
/// already exists; cleans up the partial bundle on error (a failed import must not
/// leave a half-VM in the library).
pub fn create(opts: &CreateOpts, dest_dir: &Path) -> Result<VmBundle> {
    anyhow::ensure!(!opts.name.is_empty(), "VM name must not be empty");
    anyhow::ensure!(
        !opts.name.contains('/') && !opts.name.starts_with('.'),
        "VM name must not contain '/' or start with '.': {:?}",
        opts.name
    );
    anyhow::ensure!(opts.cpus > 0, "cpus must be > 0");

    let bundle = VmBundle::new(dest_dir.join(format!("{}.{BUNDLE_EXT}", opts.name)));
    anyhow::ensure!(
        !bundle.path.exists(),
        "a VM named {:?} already exists at {}",
        opts.name,
        bundle.path.display()
    );

    let built = (|| -> Result<VmBundle> {
        std::fs::create_dir_all(bundle.disks_dir())
            .with_context(|| format!("creating {}", bundle.disks_dir().display()))?;
        std::fs::create_dir_all(bundle.run_dir())?;
        std::fs::create_dir_all(bundle.logs_dir())?;

        let disks = match &opts.disk {
            Some(src) => vec![import_disk(src, &bundle, opts.import_mode)?],
            None => vec![],
        };

        let uuid = uuid_v4();
        let cfg = VmConfig {
            config_version: CONFIG_VERSION,
            identity: Identity {
                name: opts.name.clone(),
                uuid: uuid.clone(),
                created: rfc3339_utc_now(),
            },
            hardware: Hardware {
                cpus: opts.cpus,
                memory: opts.memory.clone(),
            },
            disks,
            cdroms: vec![],
            // Managed VMs get networking by default — a desktop VM without SSH/net
            // is the exception, not the rule. The MAC is persisted now (identity
            // first), plumbed to the worker later.
            networks: vec![NetworkEntry {
                mode: NetMode::Nat,
                mac: mac_for_uuid(&uuid),
                ssh_port: opts.ssh_port,
            }],
            shares: vec![],
            boot: Default::default(),
            display: DisplayCfg {
                window: opts.window,
                ..Default::default()
            },
            input: Default::default(),
        };
        bundle.save(&cfg)?;
        Ok(bundle.clone())
    })();

    if built.is_err() {
        let _ = std::fs::remove_dir_all(&bundle.path);
    }
    built
}

/// Import the source image per the mode, returning the disk entry to record.
fn import_disk(src: &Path, bundle: &VmBundle, mode: ImportMode) -> Result<DiskEntry> {
    let meta = std::fs::metadata(src)
        .with_context(|| format!("disk image not found: {}", src.display()))?;
    anyhow::ensure!(
        meta.is_file(),
        "disk image is not a regular file: {}",
        src.display()
    );

    match mode {
        ImportMode::ReferenceInPlace => {
            let abs = src
                .canonicalize()
                .with_context(|| format!("resolving {}", src.display()))?;
            Ok(DiskEntry {
                path: abs,
                ro: false,
            })
        }
        ImportMode::CloneIntoBundle => {
            let ext = src
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("raw")
                .to_string();
            let rel = PathBuf::from("disks").join(format!("root.{ext}"));
            let dst = bundle.path.join(&rel);
            clone_or_copy(src, &dst)?;
            Ok(DiskEntry {
                path: rel,
                ro: false,
            })
        }
    }
}

/// APFS `clonefile(2)` when possible (instant, space-shared COW), else a plain copy
/// (cross-volume / non-APFS). The caller decides threading — a real copy of a big
/// image is slow.
fn clone_or_copy(src: &Path, dst: &Path) -> Result<()> {
    let csrc = CString::new(src.as_os_str().as_bytes()).context("disk path contains NUL")?;
    let cdst = CString::new(dst.as_os_str().as_bytes()).context("dest path contains NUL")?;
    let rc = unsafe { libc::clonefile(csrc.as_ptr(), cdst.as_ptr(), 0) };
    if rc == 0 {
        return Ok(());
    }
    // EXDEV (cross-volume), ENOTSUP (non-APFS), EEXIST etc. — fall back to a copy,
    // which surfaces its own error if the destination is genuinely unwritable.
    std::fs::copy(src, dst).with_context(|| {
        format!(
            "copying {} into the bundle (clonefile failed: {})",
            src.display(),
            std::io::Error::last_os_error()
        )
    })?;
    Ok(())
}

/// Delete a stopped VM's bundle. Everything inside the bundle goes — including
/// cloned-in disks — but disks referenced by absolute path are never touched.
pub fn remove(bundle: &VmBundle) -> Result<()> {
    anyhow::ensure!(
        !runtime::status(bundle).is_running(),
        "VM {} is running; stop it before deleting",
        bundle.dir_name()
    );
    std::fs::remove_dir_all(&bundle.path)
        .with_context(|| format!("deleting {}", bundle.path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vmlib::bundle::tests::{basic_opts, scratch_library};

    #[test]
    fn create_builds_a_loadable_bundle_with_net_defaults() {
        let lib = scratch_library("create");
        let bundle = create(&basic_opts("plain"), &lib).unwrap();

        assert!(bundle.disks_dir().is_dir());
        assert!(bundle.logs_dir().is_dir());
        let cfg = bundle.load().unwrap();
        assert_eq!(cfg.identity.name, "plain");
        assert_eq!(cfg.networks.len(), 1, "net on by default");
        assert_eq!(cfg.networks[0].mac, mac_for_uuid(&cfg.identity.uuid));
        assert_eq!(cfg.networks[0].ssh_port, 0);
        assert!(cfg.disks.is_empty());
        assert!(cfg.display.window);

        // Same name again → refused.
        let err = create(&basic_opts("plain"), &lib).unwrap_err().to_string();
        assert!(err.contains("already exists"), "{err}");

        std::fs::remove_dir_all(&lib).ok();
    }

    #[test]
    fn import_clones_into_bundle_and_remove_deletes_it() {
        let lib = scratch_library("clone");
        let src = lib.join("source.raw");
        std::fs::write(&src, b"disk-bytes").unwrap();

        let mut opts = basic_opts("cloned");
        opts.disk = Some(src.clone());
        let bundle = create(&opts, &lib).unwrap();

        let cfg = bundle.load().unwrap();
        assert_eq!(cfg.disks.len(), 1);
        assert_eq!(cfg.disks[0].path, PathBuf::from("disks/root.raw"));
        let cloned = bundle.resolve_path(&cfg.disks[0].path);
        assert_eq!(std::fs::read(&cloned).unwrap(), b"disk-bytes");

        // Deleting the bundle removes the clone but not the source.
        remove(&bundle).unwrap();
        assert!(!bundle.path.exists());
        assert!(src.exists(), "source image must survive");

        std::fs::remove_dir_all(&lib).ok();
    }

    #[test]
    fn import_in_place_references_and_remove_spares_the_disk() {
        let lib = scratch_library("inplace");
        let src = lib.join("shared-base.raw");
        std::fs::write(&src, b"base").unwrap();

        let mut opts = basic_opts("referenced");
        opts.disk = Some(src.clone());
        opts.import_mode = ImportMode::ReferenceInPlace;
        let bundle = create(&opts, &lib).unwrap();

        let cfg = bundle.load().unwrap();
        assert!(cfg.disks[0].path.is_absolute(), "in-place = absolute path");
        assert_eq!(std::fs::read(&cfg.disks[0].path).unwrap(), b"base");
        assert!(
            !bundle.disks_dir().join("root.raw").exists(),
            "nothing copied into the bundle"
        );

        remove(&bundle).unwrap();
        assert!(src.exists(), "referenced disk must survive deletion");

        std::fs::remove_dir_all(&lib).ok();
    }

    #[test]
    fn create_rejects_bad_names_and_missing_disks_without_leaving_debris() {
        let lib = scratch_library("reject");
        assert!(create(&basic_opts(""), &lib).is_err());
        assert!(create(&basic_opts("a/b"), &lib).is_err());
        assert!(create(&basic_opts(".hidden"), &lib).is_err());

        let mut opts = basic_opts("nodisk");
        opts.disk = Some(lib.join("does-not-exist.raw"));
        assert!(create(&opts, &lib).is_err());
        assert!(
            !lib.join("nodisk.liminavm").exists(),
            "failed create must clean up the partial bundle"
        );

        std::fs::remove_dir_all(&lib).ok();
    }

    #[test]
    fn remove_refuses_a_running_vm() {
        let lib = scratch_library("rm-running");
        let bundle = create(&basic_opts("busy"), &lib).unwrap();
        let lock = runtime::acquire(&bundle).unwrap();
        let err = remove(&bundle).unwrap_err().to_string();
        assert!(err.contains("running"), "{err}");
        drop(lock);
        remove(&bundle).unwrap();
        std::fs::remove_dir_all(&lib).ok();
    }
}
