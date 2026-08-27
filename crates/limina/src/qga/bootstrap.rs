// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Delivering the enhanced tier into a guest that has none of it, over the stock agent's
//! port.
//!
//! The two-tier guarantee makes the basic tier the **bootstrap substrate**: a fresh install
//! starts stock, and everything of ours arrives afterwards — so whatever delivers it cannot
//! depend on the things it delivers. Until now the only delivery path was SSH, which needs a
//! network, a running `sshd`, an account and a key already in it. `qemu-guest-agent` needs
//! none of those: it is installed on every Fedora desktop variant, it is root, and
//! `guest-file-write` + `guest-exec` are the whole mechanism.
//!
//! # A kit, not a payload
//!
//! What travels here is small — an agent binary, a unit file, a script; hundreds of KiB. It
//! is deliberately **not** the enhanced-tier payload, which is half a gigabyte of RPMs and
//! would be absurd as base64 inside JSON lines. The kit's job is to install enough for the
//! guest to fetch the rest the ordinary way (virtiofs, the network) once `limina-agent` is
//! up. If a future kit starts to look large, that is the signal to bootstrap a *fetcher*,
//! not to make this channel wider.
//!
//! # The shape of a kit
//!
//! A directory. Every regular file in it is staged in the guest under [`STAGE`], except:
//!
//! - [`INSTALLER`] — required. Run as root once the files are in place; a kit without one
//!   would deliver bytes and change nothing.
//! - `authorized_keys.<user>` — the keys in it are installed for `<user>` through the
//!   agent's own `guest-ssh-add-authorized-keys`, which is the only way to get the
//!   ownership, mode and SELinux label `sshd` insists on.
//!
//! # Where it works
//!
//! Wherever the agent is unconfined — an AppArmor guest, or an SELinux one whose
//! `virt_qemu_ga_t` domain has been made permissive. On a stock **SELinux-Enforcing**
//! Fedora it does not: `guest-exec` runs its children as `virt_qemu_ga_t`, which may not
//! write `bin_t` (so `install` into `/usr/local/bin` is denied), may not reach systemd's
//! D-Bus (so `systemd-run` is denied), may not write `/etc/qemu-ga/fsfreeze-hook.d/`, and
//! may not even call `getenforce`; `guest-ssh-add-authorized-keys` is separately gated
//! behind the `virt_qemu_ga_manage_ssh` boolean. Every lever that would widen the domain
//! needs privileges the domain lacks, so nothing here can lift its own confinement. **On
//! such a guest the delivery path is SSH**, and a failed deploy says so and costs nothing.
//!
//! # When it fires
//!
//! Only when the guest does **not** already run `limina-agent`: a bootstrap is for a guest
//! that has none, and reinstalling under a working one is a way to break it. Since a guest's
//! own agent takes some seconds to boot and connect, the deploy waits [`DEFAULT_GRACE`]
//! after the port is attached and then looks. Everything after that is best-effort — a
//! bootstrap that fails costs the enhanced tier, never the VM.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use super::client::Qga;

/// The control-plane name an enhanced guest's **system** agent introduces itself by. Its
/// presence is what says a guest needs no bootstrap.
///
/// The trailing slash is load-bearing: peers announce themselves as `<name>/<version>`, and
/// the enhanced tier also connects a `limina-agent-session` peer — a per-user helper that
/// shares the prefix and cannot stand in for the system agent. Without the separator, a
/// guest whose system agent is gone but whose session helper is still connected reads as
/// needing nothing.
pub const AGENT: &str = "limina-agent/";

/// Where a kit is staged in the guest. `/var/tmp` rather than `/tmp`: a long boot can meet a
/// `systemd-tmpfiles` sweep, and the files must outlive it until the installer has run.
pub const STAGE: &str = "/var/tmp/limina-bootstrap";

/// The one file a kit must have. Run with `/bin/sh`, so it needs no execute bit — which also
/// keeps it clear of the SELinux label a file the agent created would not have.
pub const INSTALLER: &str = "install.sh";

/// `authorized_keys.<user>` — keys to install for that account.
pub const KEYS_PREFIX: &str = "authorized_keys.";

/// How long to let the guest bring up its own `limina-agent` before deciding it has none.
/// Generous: the cost of waiting is a slower bootstrap, the cost of being hasty is
/// reinstalling under a healthy agent.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(120);

/// How long the installer gets. It runs `systemctl enable --now`, which waits on the unit.
pub const INSTALL_TIMEOUT: Duration = Duration::from_secs(300);

/// The kit directory to deploy (`LIMINA_QGA_DEPLOY`), if one was named.
///
/// Env-only on purpose: the mechanism is settled, the product surface that fronts it — a
/// control-center "install guest tools" — is not, and a CLI flag would commit to one.
pub fn dir_from_env() -> Option<PathBuf> {
    std::env::var_os("LIMINA_QGA_DEPLOY")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// `LIMINA_QGA_DEPLOY_AFTER` overrides the grace, in seconds.
pub fn grace_from_env() -> Duration {
    parse_grace(std::env::var("LIMINA_QGA_DEPLOY_AFTER").ok().as_deref())
}

/// The pure half of [`grace_from_env`]. A typo keeps the default rather than collapsing the
/// window to zero, which would deploy over a healthy agent every single boot.
fn parse_grace(raw: Option<&str>) -> Duration {
    raw.and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_GRACE)
}

/// How long to wait before the next `guest-exec-status` poll. Tight at first, because most
/// commands a kit runs are instant, then backing off so a five-minute installer does not
/// cost a thousand round trips.
pub fn poll_delay(attempt: u32) -> Duration {
    Duration::from_millis(match attempt {
        0..=4 => 100,
        5..=20 => 500,
        _ => 2000,
    })
}

/// What a filename in the kit directory means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// The installer script.
    Installer,
    /// Keys for this account.
    Keys(String),
    /// An ordinary file, staged under [`STAGE`].
    File,
}

/// Classify one kit filename.
pub fn classify(name: &str) -> Entry {
    if name == INSTALLER {
        Entry::Installer
    } else if let Some(user) = name.strip_prefix(KEYS_PREFIX).filter(|u| !u.is_empty()) {
        Entry::Keys(user.to_string())
    } else {
        Entry::File
    }
}

/// A loaded kit: everything read off the host, before a single byte has been sent.
pub struct Kit {
    /// Staged files, `(name, bytes)`, sorted so a deploy is reproducible.
    pub files: Vec<(String, Vec<u8>)>,
    /// `(user, keys)` from the `authorized_keys.*` entries.
    pub keys: Vec<(String, Vec<String>)>,
}

impl Kit {
    /// Total bytes that will travel the port.
    pub fn bytes(&self) -> usize {
        self.files.iter().map(|(_, b)| b.len()).sum()
    }
}

/// Read a kit directory. Fails rather than deploying a kit that cannot do anything: a
/// missing installer, or a directory that is not one, is a misconfiguration to say out loud.
pub fn load(dir: &Path) -> Result<Kit> {
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("reading the bootstrap kit at {}", dir.display()))?;
    let mut files = Vec::new();
    let mut keys = Vec::new();
    for entry in entries {
        let entry = entry.context("listing the bootstrap kit")?;
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let bytes = std::fs::read(entry.path())
            .with_context(|| format!("reading {}", entry.path().display()))?;
        match classify(&name) {
            Entry::Keys(user) => keys.push((
                user,
                String::from_utf8_lossy(&bytes)
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && !l.starts_with('#'))
                    .map(str::to_string)
                    .collect(),
            )),
            _ => files.push((name, bytes)),
        }
    }
    if !files.iter().any(|(n, _)| n == INSTALLER) {
        bail!(
            "the bootstrap kit at {} has no {INSTALLER}, so deploying it would change nothing",
            dir.display()
        );
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    keys.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(Kit { files, keys })
}

/// Push a kit into the guest and run its installer.
///
/// The staging directory is removed afterwards **whatever happened**: it is the supervisor's
/// scratch, not something the guest asked for, and leaving a half-delivered binary behind is
/// how a later boot picks up something nobody meant to install.
pub fn deploy(qga: &Qga, kit: &Kit) -> Result<()> {
    let mkdir = qga
        .run("/bin/mkdir", &["-p", STAGE], Duration::from_secs(30))
        .context("making the staging directory")?;
    if !mkdir.ok() {
        bail!("could not create {STAGE} in the guest: {}", mkdir.said());
    }

    let result = push_and_install(qga, kit);
    // Best-effort: if the guest is wedged enough to refuse this, it is also past caring, and
    // the real error is the one being returned.
    if let Err(e) = qga.run("/bin/rm", &["-rf", STAGE], Duration::from_secs(30)) {
        log::debug!("qga: could not clear {STAGE} ({e:#})");
    }
    result?;
    install_keys(qga, kit);
    Ok(())
}

/// Install the kit's `authorized_keys.*` entries — **after** the kit itself, and never fatal.
///
/// On an SELinux-Enforcing Fedora guest this verb is off by default and fails with
/// `failed to create directory '/home/…/.ssh': File exists` — an error whose text names a
/// filesystem it never reached. `qemu-ga` runs as `virt_qemu_ga_t`, which may not touch
/// `user_home_t` unless the `virt_qemu_ga_manage_ssh` boolean is set; qemu's own guard is a
/// `g_file_test(…, IS_DIR)` before an unconditional `g_mkdir` (`qga/commands-posix-ssh.c`),
/// and a domain that cannot see the directory takes the branch that then collides with it.
/// Either way it is a guest policy decision, not a broken bootstrap: the agent binary is what
/// the kit exists to deliver, and it is already installed by here.
fn install_keys(qga: &Qga, kit: &Kit) {
    for (user, keys) in &kit.keys {
        if keys.is_empty() {
            continue;
        }
        match qga.add_authorized_keys(user, keys, false) {
            Ok(()) => log::info!("qga: installed {} ssh key(s) for {user}", keys.len()),
            Err(e) => log::warn!("qga: could not install ssh key(s) for {user} ({e:#})"),
        }
    }
}

fn push_and_install(qga: &Qga, kit: &Kit) -> Result<()> {
    for (name, bytes) in &kit.files {
        let path = format!("{STAGE}/{name}");
        qga.write_file(&path, bytes)
            .with_context(|| format!("delivering {name}"))?;
        log::debug!("qga: delivered {path} ({} bytes)", bytes.len());
    }

    let install = qga
        .run(
            "/bin/sh",
            &[&format!("{STAGE}/{INSTALLER}")],
            INSTALL_TIMEOUT,
        )
        .context("running the kit's installer")?;
    if !install.ok() {
        // Line by line, each carrying the `qga:` prefix: a failing installer's own output is
        // the only account of what went wrong inside the guest, and folding it into one
        // multi-line message loses every line after the first to any per-line log filter.
        for line in install
            .stderr
            .lines()
            .chain(install.stdout.lines())
            .filter(|l| !l.trim().is_empty())
        {
            log::warn!("qga: {INSTALLER}: {line}");
        }
        bail!(
            "the kit's {INSTALLER} exited {}: {}",
            install
                .exitcode
                .map(|c| c.to_string())
                .unwrap_or_else(|| format!("on signal {:?}", install.signal)),
            install.said()
        );
    }
    log::info!(
        "qga: bootstrap kit installed ({} file(s), {:.0} KiB){}",
        kit.files.len(),
        kit.bytes() as f64 / 1024.0,
        match install.said().as_str() {
            "" => String::new(),
            said => format!("; {INSTALLER} said: {said}"),
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kit_filename_says_what_it_is() {
        assert_eq!(classify("install.sh"), Entry::Installer);
        assert_eq!(classify("limina-agent"), Entry::File);
        assert_eq!(
            classify("authorized_keys.claude"),
            Entry::Keys("claude".into())
        );
        // No user named, so it is not a key file — staging it is the safe reading, because
        // the alternative is installing keys for an account we had to guess.
        assert_eq!(classify("authorized_keys."), Entry::File);
    }

    /// A typo in the knob must not collapse the window: a zero grace deploys over a healthy
    /// enhanced guest on every single boot, which is the one outcome this feature must never
    /// have.
    #[test]
    fn a_typo_in_the_grace_keeps_the_default() {
        assert_eq!(parse_grace(None), DEFAULT_GRACE);
        assert_eq!(parse_grace(Some("two minutes")), DEFAULT_GRACE);
        assert_eq!(parse_grace(Some("30")), Duration::from_secs(30));
    }

    #[test]
    fn the_status_poll_starts_tight_and_backs_off() {
        assert!(poll_delay(0) < poll_delay(10));
        assert!(poll_delay(10) < poll_delay(100));
        // A five-minute installer must not cost thousands of round trips.
        assert!(poll_delay(100) >= Duration::from_secs(1));
    }

    #[test]
    fn a_kit_without_an_installer_is_refused() {
        let dir = std::env::temp_dir().join(format!("limina-kit-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("limina-agent"), b"binary").unwrap();
        assert!(load(&dir).is_err());

        std::fs::write(dir.join(INSTALLER), b"#!/bin/sh\ntrue\n").unwrap();
        std::fs::write(
            dir.join("authorized_keys.claude"),
            "# a comment\nssh-ed25519 AAAA one\n\nssh-ed25519 BBBB two\n",
        )
        .unwrap();
        let kit = load(&dir).unwrap();
        assert_eq!(
            kit.files
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>(),
            vec![INSTALLER, "limina-agent"]
        );
        // Comments and blank lines are not keys.
        assert_eq!(
            kit.keys,
            vec![(
                "claude".to_string(),
                vec![
                    "ssh-ed25519 AAAA one".to_string(),
                    "ssh-ed25519 BBBB two".to_string(),
                ]
            )]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
