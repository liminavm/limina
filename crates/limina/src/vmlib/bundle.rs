// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! `.liminavm` bundle access + the VM library.
//!
//! A bundle is self-contained and relocatable (Finder-copyable): `vm.toml` plus
//! `disks/`, `run/` (lock + pidfile, transient), and `logs/`. The library is just a
//! directory of bundles — `limina ls` and the control center both read it directly;
//! there is no daemon or registry.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use super::schema::VmConfig;

pub const BUNDLE_EXT: &str = "liminavm";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmBundle {
    pub path: PathBuf,
}

impl VmBundle {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The VM's display-ish name: the bundle directory name minus `.liminavm`.
    /// (The authoritative label is `identity.name` in vm.toml; this is the fallback
    /// for bundles whose config can't be read.)
    pub fn dir_name(&self) -> String {
        self.path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string()
    }

    pub fn vm_toml(&self) -> PathBuf {
        self.path.join("vm.toml")
    }

    pub fn disks_dir(&self) -> PathBuf {
        self.path.join("disks")
    }

    pub fn run_dir(&self) -> PathBuf {
        self.path.join("run")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.path.join("logs")
    }

    pub fn load(&self) -> Result<VmConfig> {
        let text = std::fs::read_to_string(self.vm_toml())
            .with_context(|| format!("reading {}", self.vm_toml().display()))?;
        let cfg: VmConfig = toml::from_str(&text)
            .with_context(|| format!("parsing {}", self.vm_toml().display()))?;
        cfg.validate()
            .with_context(|| format!("validating {}", self.vm_toml().display()))?;
        Ok(cfg)
    }

    /// Atomic save: write `vm.toml.tmp`, then rename over `vm.toml`. This is why the
    /// running-VM flock lives on `run/lock` and not here — the rename swaps inodes.
    pub fn save(&self, cfg: &VmConfig) -> Result<()> {
        cfg.validate()?;
        let text = toml::to_string_pretty(cfg).context("serializing vm.toml")?;
        let tmp = self.path.join("vm.toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, self.vm_toml())
            .with_context(|| format!("renaming into {}", self.vm_toml().display()))?;
        Ok(())
    }

    /// Resolve a (possibly bundle-relative) disk/cdrom path against the bundle root.
    pub fn resolve_path(&self, p: &Path) -> PathBuf {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.path.join(p)
        }
    }
}

/// The default VM library. `$LIMINA_VM_LIBRARY` overrides it (tests, portable setups);
/// otherwise `~/Library/Application Support/limina/VMs`.
pub fn library_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LIMINA_VM_LIBRARY") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join("Library/Application Support/limina/VMs")
}

/// Resolve a VM spec to a bundle: a path if it looks like one (contains a separator or
/// ends in `.liminavm`), else a name looked up in the library (case-insensitive on
/// miss, matching the macOS filesystem's own posture).
pub fn resolve(spec: &str) -> Result<VmBundle> {
    let looks_like_path = spec.contains('/') || spec.ends_with(&format!(".{BUNDLE_EXT}"));
    if looks_like_path {
        let b = VmBundle::new(spec);
        anyhow::ensure!(
            b.vm_toml().is_file(),
            "no VM definition at {} (missing vm.toml)",
            b.path.display()
        );
        return Ok(b);
    }
    // Scan the library rather than probing the joined path directly: the returned
    // bundle then always carries the on-disk casing (the library filesystem is
    // case-insensitive APFS), so downstream consumers (ls, the control center) get
    // consistent path keys. Exact-case match wins over a case-insensitive one.
    let all = list()?;
    if let Some(b) = all.iter().find(|b| b.dir_name() == spec) {
        return Ok(b.clone());
    }
    if let Some(b) = all.iter().find(|b| b.dir_name().eq_ignore_ascii_case(spec)) {
        return Ok(b.clone());
    }
    let lib = library_dir();
    let names: Vec<String> = all.iter().map(|b| b.dir_name()).collect();
    anyhow::bail!(
        "no VM named {spec:?} in {} (available: {})",
        lib.display(),
        if names.is_empty() {
            "none".to_string()
        } else {
            names.join(", ")
        }
    )
}

/// All bundles in the library, sorted by name. A missing library dir is an empty
/// library, not an error (nothing has been created yet).
pub fn list() -> Result<Vec<VmBundle>> {
    let lib = library_dir();
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&lib) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("reading library {}", lib.display())),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() && path.extension().and_then(|e| e.to_str()) == Some(BUNDLE_EXT) {
            out.push(VmBundle::new(path));
        }
    }
    out.sort_by_key(|b| b.dir_name().to_ascii_lowercase());
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::vmlib::import::{create, CreateOpts, ImportMode};
    use crate::vmlib::schema::Memory;

    /// Serialize the LIMINA_VM_LIBRARY-dependent tests (env vars are process-global).
    pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    pub(crate) fn scratch_library(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "limina-vmlib-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub(crate) fn basic_opts(name: &str) -> CreateOpts {
        CreateOpts {
            name: name.into(),
            disk: None,
            import_mode: ImportMode::CloneIntoBundle,
            blank_size: None,
            cdrom: None,
            cpus: 4,
            memory: Memory::default(),
            ssh_port: 0,
            window: true,
        }
    }

    #[test]
    fn resolve_finds_by_path_name_and_case() {
        let _guard = ENV_LOCK.lock().unwrap();
        let lib = scratch_library("resolve");
        std::env::set_var("LIMINA_VM_LIBRARY", &lib);

        let bundle = create(&basic_opts("Fedora"), &lib).unwrap();

        // By explicit path.
        let by_path = resolve(bundle.path.to_str().unwrap()).unwrap();
        assert_eq!(by_path, bundle);
        // By library name, exact and case-insensitive.
        assert_eq!(resolve("Fedora").unwrap(), bundle);
        assert_eq!(resolve("fedora").unwrap(), bundle);
        // Unknown names list what's available.
        let err = resolve("nope").unwrap_err().to_string();
        assert!(err.contains("Fedora"), "error should list VMs: {err}");
        // list() enumerates it.
        let all = list().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].dir_name(), "Fedora");

        std::env::remove_var("LIMINA_VM_LIBRARY");
        std::fs::remove_dir_all(&lib).ok();
    }

    #[test]
    fn save_is_atomic_and_reloadable() {
        let _guard = ENV_LOCK.lock().unwrap();
        let lib = scratch_library("save");
        let bundle = create(&basic_opts("editme"), &lib).unwrap();

        let mut cfg = bundle.load().unwrap();
        cfg.hardware.cpus = 7;
        cfg.networks[0].ssh_port = 2299;
        bundle.save(&cfg).unwrap();

        let back = bundle.load().unwrap();
        assert_eq!(back.hardware.cpus, 7);
        assert_eq!(back.networks[0].ssh_port, 2299);
        assert!(
            !bundle.path.join("vm.toml.tmp").exists(),
            "tmp file cleaned"
        );

        std::fs::remove_dir_all(&lib).ok();
    }

    #[test]
    fn resolve_path_joins_relative_only() {
        let b = VmBundle::new("/lib/Fedora.liminavm");
        assert_eq!(
            b.resolve_path(Path::new("disks/root.raw")),
            PathBuf::from("/lib/Fedora.liminavm/disks/root.raw")
        );
        assert_eq!(
            b.resolve_path(Path::new("/elsewhere/base.raw")),
            PathBuf::from("/elsewhere/base.raw")
        );
    }
}
