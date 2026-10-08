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

    /// The bundle directory name minus `.liminavm`. This remains the name used to resolve a VM
    /// from the CLI even when the definition carries a display-name override.
    pub fn dir_name(&self) -> String {
        self.path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string()
    }

    /// The name shown to people: a non-empty definition override, or the bundle directory name.
    pub fn display_name(&self, cfg: &VmConfig) -> String {
        cfg.identity
            .name
            .as_deref()
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| self.dir_name())
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

    /// The TPM's state (its NV: seeds, keys, NV indexes, lockout), when `[hardware] tpm` is on.
    /// Beside the disks because it is as much the VM's identity as they are: a copy of the
    /// bundle keeps the TPM, and every secret sealed to it.
    pub fn tpm_state(&self) -> PathBuf {
        self.path.join("tpm.state")
    }

    /// The firmware's UEFI variables (boot entries; later Secure Boot keys), mapped into the
    /// guest as its variable store. Beside the disks for the same reason as `tpm_state`: the boot
    /// entries name partitions on them.
    pub fn efi_vars(&self) -> PathBuf {
        self.path.join("efi.vars")
    }

    /// Mutable machine state (window placement etc.) — see `vmlib::state`.
    pub fn state_toml(&self) -> PathBuf {
        self.path.join("state.toml")
    }

    /// The VM's suspend snapshot file (M9.2). Written by the worker when the guest is suspended
    /// (`limina suspend`) and reloaded on the next start when `state.toml` records `[suspended]`.
    /// Lives under `run/` (per-boot state, not user content) so it travels with the bundle but is
    /// clearly disposable.
    pub fn snapshot_bin(&self) -> PathBuf {
        self.run_dir().join("snapshot.bin")
    }

    /// The restore splash the window shows until a resumed guest's first frame, written beside
    /// [`Self::snapshot_bin`] at suspend.
    pub fn splash_png(&self) -> PathBuf {
        self.run_dir().join("splash.png")
    }

    /// Throw away a suspended session so the next start cold-boots: the snapshot, its splash and
    /// the `[suspended]` record. Returns whether there was a session to discard. The caller must
    /// know the VM is stopped — a running VM's armed snapshot path is its own business.
    pub fn discard_suspend(&self) -> Result<bool> {
        let snapshot = self.snapshot_bin();
        let had = snapshot.exists();
        if had {
            std::fs::remove_file(&snapshot).with_context(|| {
                format!("discarding the suspended session {}", snapshot.display())
            })?;
        }
        // The splash and the record are only meaningful beside a snapshot; clear any leftovers.
        let _ = std::fs::remove_file(self.splash_png());
        if super::state::load(&self.state_toml()).is_some_and(|s| s.suspended.is_some()) {
            super::state::set_suspended(&self.state_toml(), None)
                .with_context(|| format!("clearing {}'s suspended record", self.dir_name()))?;
        }
        Ok(had)
    }

    /// Throw away this VM's TPM: delete `tpm.state` so the next start creates a fresh TPM (a new
    /// seed, a new identity). Every secret sealed to the old TPM — disk-encryption keys bound to
    /// its PCRs, keys it generated — becomes unrecoverable. Returns whether there was state to
    /// remove. The caller must know the VM is stopped and not suspended: a suspended guest carries
    /// its TPM inside the snapshot, and the worker refuses to resume a VM whose TPM has gone.
    pub fn reset_tpm(&self) -> Result<bool> {
        let state = self.tpm_state();
        if !state.exists() {
            return Ok(false);
        }
        std::fs::remove_file(&state)
            .with_context(|| format!("resetting the TPM {}", state.display()))?;
        Ok(true)
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

/// `~/Library/Application Support/Limina`: the default library's parent, and where the host
/// config lives — outside the library, so it survives the library moving.
pub fn app_support_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join("Library/Application Support/Limina")
}

/// The host-wide settings file (`docs/design/vm-definitions.md` §8.1). `$LIMINA_CONFIG`
/// overrides its location (tests).
pub fn config_path() -> PathBuf {
    match std::env::var_os("LIMINA_CONFIG") {
        Some(p) => PathBuf::from(p),
        None => app_support_dir().join("config.toml"),
    }
}

/// The VM library: `$LIMINA_VM_LIBRARY` (tests, portable setups), else `[library] path` from
/// [`config_path`], else `~/Library/Application Support/Limina/VMs`.
///
/// The config is read on every call, not cached: the control center runs for days and has to
/// see a change without a restart, and every caller is about to do directory I/O anyway.
pub fn library_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LIMINA_VM_LIBRARY") {
        return PathBuf::from(dir);
    }
    configured_library(&config_path()).unwrap_or_else(|| app_support_dir().join("VMs"))
}

/// `[library] path` from the config at `path`. Missing file or key = `None`. A file that does
/// not parse, or a relative path, is a warning and `None`: the control center must still open
/// so the user can fix it.
fn configured_library(path: &Path) -> Option<PathBuf> {
    #[derive(serde::Deserialize, Default)]
    struct Config {
        #[serde(default)]
        library: Library,
    }
    #[derive(serde::Deserialize, Default)]
    struct Library {
        path: Option<PathBuf>,
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn_config(format!(
                "cannot read {}: {e}; using the default VM library",
                path.display()
            ));
            return None;
        }
    };
    let lib = match toml::from_str::<Config>(&text) {
        Ok(c) => c.library.path?,
        Err(e) => {
            warn_config(format!(
                "{} is malformed ({e}); using the default VM library",
                path.display()
            ));
            return None;
        }
    };
    if lib.as_os_str().is_empty() {
        return None;
    }
    if !lib.is_absolute() {
        warn_config(format!(
            "{}: [library] path {} is not absolute; using the default VM library",
            path.display(),
            lib.display()
        ));
        return None;
    }
    Some(lib)
}

/// Say what is wrong with the config once, not on every one of the control center's refreshes.
fn warn_config(msg: String) {
    static LAST: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|p| p.into_inner());
    if last.as_deref() != Some(msg.as_str()) {
        log::warn!("{msg}");
        *last = Some(msg);
    }
}

/// The volume root (`/Volumes/<name>`) `dir` lives on, when that volume is not mounted.
///
/// Only paths under `/Volumes` are judged: that is where removable and network volumes mount,
/// and anything else lives on a volume that is always there. `is_mount_point` is the probe
/// ([`is_mount_point`] in production) so the rule is testable without a disk to unplug.
pub fn unmounted_volume(dir: &Path, is_mount_point: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    use std::path::Component;
    let mut parts = dir.components();
    match (parts.next(), parts.next(), parts.next()) {
        (Some(Component::RootDir), Some(Component::Normal(v)), Some(Component::Normal(name)))
            if v == "Volumes" =>
        {
            let root = Path::new("/Volumes").join(name);
            (!is_mount_point(&root)).then_some(root)
        }
        _ => None,
    }
}

/// Is `p` a mount point: an existing directory on a different device than its parent?
pub fn is_mount_point(p: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (Ok(me), Some(Ok(parent))) = (std::fs::metadata(p), p.parent().map(std::fs::metadata))
    else {
        return false;
    };
    me.is_dir() && me.dev() != parent.dev()
}

/// Refuse to put a VM into `dir` when the volume it lives on is not mounted, rather than create
/// the directory wherever the path happens to land and grow a second library there.
pub fn ensure_volume_mounted(dir: &Path) -> Result<()> {
    if let Some(root) = unmounted_volume(dir, is_mount_point) {
        anyhow::bail!(
            "the VM library {} is on {}, which is not mounted; connect that volume (or change \
             [library] path in {}) and try again",
            dir.display(),
            root.display(),
            config_path().display()
        );
    }
    Ok(())
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
    use crate::vmlib::import::{CreateOpts, ImportMode, create};
    use crate::vmlib::schema::Memory;

    /// Serialize the LIMINA_VM_LIBRARY-dependent tests (env vars are process-global).
    pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Serializes the tests that set `LIMINA_VM_LIBRARY`. A test that panics while holding
    /// the lock poisons it; taking the guard back keeps that one failure from turning every
    /// later test into a `PoisonError`.
    pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

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

    /// `library_dir()` precedence (design §8.1): env > `[library] path` in config.toml >
    /// default, re-read on every call, and a broken config falls through to the default.
    #[test]
    fn the_library_comes_from_env_then_config_then_the_default() {
        let _g = env_lock();
        let dir = scratch_library("config");
        let config = dir.join("config.toml");
        let saved_lib = std::env::var_os("LIMINA_VM_LIBRARY");
        unsafe {
            std::env::remove_var("LIMINA_VM_LIBRARY");
            std::env::set_var("LIMINA_CONFIG", &config);
        }
        let default = app_support_dir().join("VMs");

        assert_eq!(library_dir(), default, "no config file");
        std::fs::write(&config, "[library]\npath = \"/Volumes/Ext/Limina VMs\"\n").unwrap();
        assert_eq!(library_dir(), PathBuf::from("/Volumes/Ext/Limina VMs"));
        // Re-read per call: a long-running center sees the change.
        std::fs::write(&config, "[library]\npath = \"/Users/x/VMs\"\n").unwrap();
        assert_eq!(library_dir(), PathBuf::from("/Users/x/VMs"));
        std::fs::write(&config, "# nothing about the library\n").unwrap();
        assert_eq!(library_dir(), default, "no [library] key");
        std::fs::write(&config, "[library\npath = ").unwrap();
        assert_eq!(library_dir(), default, "malformed: warn and fall through");
        std::fs::write(&config, "[library]\npath = \"relative/VMs\"\n").unwrap();
        assert_eq!(library_dir(), default, "relative: warn and fall through");

        std::fs::write(&config, "[library]\npath = \"/Users/x/VMs\"\n").unwrap();
        unsafe { std::env::set_var("LIMINA_VM_LIBRARY", &dir) };
        assert_eq!(library_dir(), dir, "the env var outranks the config");

        unsafe {
            std::env::remove_var("LIMINA_CONFIG");
            match saved_lib {
                Some(v) => std::env::set_var("LIMINA_VM_LIBRARY", v),
                None => std::env::remove_var("LIMINA_VM_LIBRARY"),
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Only `/Volumes/<name>` paths are judged, by their volume root.
    #[test]
    fn an_unmounted_volume_is_named_by_its_root() {
        let mounted = |p: &Path| p == Path::new("/Volumes/Ext");
        assert_eq!(
            unmounted_volume(Path::new("/Volumes/Ext/Limina VMs"), mounted),
            None
        );
        assert_eq!(
            unmounted_volume(Path::new("/Volumes/Gone/Limina VMs"), mounted),
            Some(PathBuf::from("/Volumes/Gone"))
        );
        assert_eq!(
            unmounted_volume(Path::new("/Volumes/Gone"), mounted),
            Some(PathBuf::from("/Volumes/Gone"))
        );
        assert_eq!(unmounted_volume(Path::new("/Users/x/VMs"), |_| false), None);
        assert_eq!(unmounted_volume(Path::new("/Volumes"), |_| false), None);
        // The real probe: a name nothing is mounted at is not a mount point.
        assert!(!is_mount_point(Path::new("/Volumes/limina-no-such-volume")));
    }

    /// Creating a VM in a library whose volume is not mounted refuses, naming the volume,
    /// rather than putting a directory wherever the path happens to land.
    #[test]
    fn creation_refuses_a_library_on_an_unmounted_volume() {
        let lib = PathBuf::from(format!(
            "/Volumes/limina-unplugged-{}/VMs",
            std::process::id()
        ));
        let err = create(&basic_opts("Shadow"), &lib).unwrap_err().to_string();
        assert!(err.contains("not mounted"), "{err}");
        assert!(err.contains("limina-unplugged"), "{err}");
        assert!(!lib.exists());
    }

    #[test]
    fn resolve_finds_by_path_name_and_case() {
        let _guard = env_lock();
        let lib = scratch_library("resolve");
        unsafe { std::env::set_var("LIMINA_VM_LIBRARY", &lib) };

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

        unsafe { std::env::remove_var("LIMINA_VM_LIBRARY") };
        std::fs::remove_dir_all(&lib).ok();
    }

    #[test]
    fn save_is_atomic_and_reloadable() {
        let _guard = env_lock();
        let lib = scratch_library("save");
        let bundle = create(&basic_opts("editme"), &lib).unwrap();

        let mut cfg = bundle.load().unwrap();
        cfg.hardware.cpus = 7;
        cfg.hardware.cpu_reclaim = crate::vcpu_policy::CpuReclaim::Moderate;
        cfg.networks[0].ssh_port = 2299;
        cfg.power.on_host_sleep = crate::vmlib::schema::OnHostSleep::Ignore;
        bundle.save(&cfg).unwrap();

        let back = bundle.load().unwrap();
        assert_eq!(back.hardware.cpus, 7);
        // A setting the Control Center offers but that does not survive a save is worse than
        // no setting at all: the sheet would show the old value back with no explanation.
        assert_eq!(
            back.hardware.cpu_reclaim,
            crate::vcpu_policy::CpuReclaim::Moderate,
            "[hardware] cpu_reclaim must round-trip"
        );
        assert_eq!(back.networks[0].ssh_port, 2299);
        assert_eq!(
            back.power.on_host_sleep,
            crate::vmlib::schema::OnHostSleep::Ignore,
            "[power] on_host_sleep must round-trip"
        );
        assert!(
            !bundle.path.join("vm.toml.tmp").exists(),
            "tmp file cleaned"
        );

        std::fs::remove_dir_all(&lib).ok();
    }

    #[test]
    fn display_name_prefers_the_override_and_falls_back_to_the_bundle() {
        let _guard = ENV_LOCK.lock().unwrap();
        let lib = scratch_library("display-name");
        let bundle = create(&basic_opts("Fedora"), &lib).unwrap();

        let mut cfg = bundle.load().unwrap();
        assert_eq!(bundle.display_name(&cfg), "Fedora");

        cfg.identity.name = Some("Workstation".into());
        assert_eq!(bundle.display_name(&cfg), "Workstation");

        cfg.identity.name = Some(String::new());
        assert_eq!(bundle.display_name(&cfg), "Fedora");

        std::fs::remove_dir_all(&lib).ok();
    }

    #[test]
    fn discard_suspend_removes_the_session_and_keeps_the_rest_of_the_state() {
        use crate::vmlib::state;
        let lib = scratch_library("discard");
        let b = VmBundle::new(lib.join("Debian.liminavm"));
        std::fs::create_dir_all(b.run_dir()).unwrap();
        std::fs::write(b.snapshot_bin(), b"snapshot").unwrap();
        std::fs::write(b.splash_png(), b"png").unwrap();
        let window = state::WindowState {
            frame: [10.0, 20.0, 800.0, 600.0],
            content: (800, 600),
            fullscreen: false,
            fullscreen_display: None,
        };
        state::set_window(&b.state_toml(), Some(window)).unwrap();
        state::set_suspended(
            &b.state_toml(),
            Some(state::Suspended {
                snapshot: b.snapshot_bin(),
                ipa_granule: None,
            }),
        )
        .unwrap();

        assert!(
            b.discard_suspend().unwrap(),
            "a suspended session was there to discard"
        );
        assert!(!b.snapshot_bin().exists());
        assert!(!b.splash_png().exists());
        let st = state::load(&b.state_toml()).unwrap();
        assert_eq!(st.suspended, None);
        assert_eq!(st.window, Some(window), "the window placement must survive");

        assert!(!b.discard_suspend().unwrap(), "nothing left to discard");
        std::fs::remove_dir_all(&lib).ok();
    }

    #[test]
    fn reset_tpm_removes_the_state_and_is_idempotent() {
        let lib = scratch_library("reset-tpm");
        let b = VmBundle::new(lib.join("Fedora.liminavm"));
        std::fs::create_dir_all(&b.path).unwrap();

        assert!(!b.reset_tpm().unwrap(), "no TPM state to reset yet");
        std::fs::write(b.tpm_state(), b"tpm nv").unwrap();
        assert!(b.reset_tpm().unwrap(), "state was there to reset");
        assert!(!b.tpm_state().exists());
        assert!(!b.reset_tpm().unwrap(), "nothing left to reset");
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
