// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! `vm.toml` v1 — the serde schema plus the identity helpers (uuid/MAC/timestamp).
//!
//! Field order matters for serialization: TOML requires scalars before tables, so
//! `config_version` leads and every table/array-of-tables follows. Unknown keys are
//! deliberately tolerated on load (an older limina opening a newer VM degrades, per
//! the two-tier posture); an unknown *required* semantic bumps `config_version`,
//! which we reject loudly.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The one config version this limina writes and understands.
pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmConfig {
    pub config_version: u32,
    pub identity: Identity,
    #[serde(default)]
    pub hardware: Hardware,
    /// Ordered — attach order IS device order (M10): the first disk is `vda` (the
    /// boot disk), the second `vdb`, and so on. Relative paths live in the bundle.
    #[serde(default, rename = "disk", skip_serializing_if = "Vec::is_empty")]
    pub disks: Vec<DiskEntry>,
    #[serde(default, rename = "cdrom", skip_serializing_if = "Vec::is_empty")]
    pub cdroms: Vec<CdromEntry>,
    #[serde(default, rename = "network", skip_serializing_if = "Vec::is_empty")]
    pub networks: Vec<NetworkEntry>,
    #[serde(default, rename = "share", skip_serializing_if = "Vec::is_empty")]
    pub shares: Vec<ShareEntry>,
    #[serde(default)]
    pub boot: BootCfg,
    #[serde(default)]
    pub display: DisplayCfg,
    #[serde(default)]
    pub input: InputCfg,
}

impl VmConfig {
    /// Reject definitions written by a newer limina (unknown required semantics).
    /// Unknown *keys* under version 1 are tolerated silently.
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.config_version == CONFIG_VERSION,
            "vm.toml config_version {} is not supported (this limina understands {}); \
             the VM was likely created by a newer limina",
            self.config_version,
            CONFIG_VERSION
        );
        anyhow::ensure!(
            !self.identity.name.is_empty(),
            "vm.toml identity.name is empty"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    /// Mutable display label; the bundle directory name is derived from it at create.
    pub name: String,
    /// The durable key: allocated at create, never changes. Snapshots, network leases
    /// and helper grants key off this, not the name.
    pub uuid: String,
    /// RFC 3339 UTC creation timestamp.
    pub created: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Hardware {
    pub cpus: u8,
    /// Placed after `cpus` so the Range form (a TOML table) serializes legally.
    pub memory: Memory,
}

impl Default for Hardware {
    fn default() -> Self {
        Self {
            cpus: 4,
            memory: Memory::default(),
        }
    }
}

/// Guest memory: a fixed size (`"4096M"`, bare MiB, `"8G"`) or an M6 dynamic range
/// (`{ min = "2G", max = "8G" }` → ballooning between the bounds). Strings are parsed
/// by the same `parse_size_mib` the CLI flags use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Memory {
    Fixed(String),
    Range { min: String, max: String },
}

impl Default for Memory {
    fn default() -> Self {
        Memory::Fixed("4096M".into())
    }
}

impl Memory {
    /// Parse the CLI's memory string: `MIN..MAX` → a dynamic range, anything else a
    /// fixed size. Both forms are validated by the same parsers the flags use.
    pub fn parse(s: &str) -> Result<Self> {
        if let Some((min, max)) = s.split_once("..") {
            crate::parse_memory_range(s)?; // bounds + ordering validation
            Ok(Memory::Range {
                min: min.trim().to_string(),
                max: max.trim().to_string(),
            })
        } else {
            let mib = crate::parse_size_mib(s)?;
            anyhow::ensure!(mib > 0, "memory must be > 0: {s:?}");
            Ok(Memory::Fixed(s.trim().to_string()))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskEntry {
    /// Relative = inside the bundle (e.g. `disks/root.raw`); absolute paths may point
    /// anywhere (e.g. a shared base image) and are never deleted with the bundle.
    pub path: PathBuf,
    #[serde(default)]
    pub ro: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CdromEntry {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetMode {
    #[default]
    Nat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkEntry {
    #[serde(default)]
    pub mode: NetMode,
    /// Allocated at create (locally-administered, derived from the uuid) and stored so
    /// per-VM identity survives restarts. Not yet plumbed to the worker — persisted
    /// first per the multi-vm-networking design.
    pub mac: String,
    /// Host port for the inbound SSH forward; 0 = auto-allocate from 2222 up.
    #[serde(default)]
    pub ssh_port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareEntry {
    /// Share name (guest mounts under /media/NAME); defaults to the dir basename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub path: PathBuf,
    #[serde(default)]
    pub ro: bool,
}

/// Boot source. Not in the original design sketch, but headless starts require an
/// explicit firmware (only windowed boots auto-resolve the GOP firmware), so the
/// definition can pin one. None = the CLI's existing resolution rules.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BootCfg {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firmware: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DisplayCfg {
    pub window: bool,
    pub gpu: GpuMode,
}

impl Default for DisplayCfg {
    fn default() -> Self {
        Self {
            window: true,
            gpu: GpuMode::Auto,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GpuMode {
    /// The coexist device: software-2D scanout + venus 3D (the shipped default).
    #[default]
    Auto,
    /// Software-2D only (`--gpu-software-2d`): the capture oracle / GPU-less hosts.
    Software2d,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct InputCfg {
    pub swap_cmd_opt: bool,
}

impl Default for InputCfg {
    fn default() -> Self {
        Self { swap_cmd_opt: true }
    }
}

// --- identity helpers -------------------------------------------------------------

/// A random v4 UUID from `getentropy` (no uuid crate; 16 bytes + version/variant bits).
pub fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    // getentropy can only fail for >256-byte buffers or a bad pointer; treat failure
    // as unrecoverable rather than silently issuing a predictable id.
    let rc = unsafe { libc::getentropy(b.as_mut_ptr().cast(), b.len()) };
    assert_eq!(
        rc,
        0,
        "getentropy failed: {}",
        std::io::Error::last_os_error()
    );
    b[6] = (b[6] & 0x0F) | 0x40; // version 4
    b[8] = (b[8] & 0x3F) | 0x80; // RFC 4122 variant
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// Deterministic locally-administered unicast MAC derived from the VM uuid, so the
/// same VM always presents the same MAC (stable DHCP leases) without a registry.
pub fn mac_for_uuid(uuid: &str) -> String {
    // FNV-1a 64 over the uuid string, folded onto 6 bytes.
    let mut h: u64 = 0xcbf29ce484222325;
    for byte in uuid.as_bytes() {
        h ^= u64::from(*byte);
        h = h.wrapping_mul(0x100000001b3);
    }
    let mut m = [
        (h >> 40) as u8,
        (h >> 32) as u8,
        (h >> 24) as u8,
        (h >> 16) as u8,
        (h >> 8) as u8,
        h as u8,
    ];
    m[0] = (m[0] & 0xFE) | 0x02; // unicast + locally administered
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        m[0], m[1], m[2], m[3], m[4], m[5]
    )
}

/// RFC 3339 UTC "now" (`2026-07-02T12:34:56Z`) without a chrono/time dependency.
pub fn rfc3339_utc_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs() as i64;
    rfc3339_from_unix(secs)
}

/// Civil-from-days (Howard Hinnant's algorithm) — exact for the proleptic Gregorian
/// calendar, which is all RFC 3339 needs.
fn rfc3339_from_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, m, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };

    format!("{year:04}-{month:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Parse a `Memory` into what the CLI consumes: `(ram_mib, Some("MIN..MAX") for ranges)`.
/// For a range, `ram_mib` is MAX (what libkrun allocates), matching `--memory` semantics.
pub fn memory_to_cli(memory: &Memory) -> Result<(usize, Option<String>)> {
    match memory {
        Memory::Fixed(s) => {
            let mib = crate::parse_size_mib(s).context("vm.toml hardware.memory")?;
            anyhow::ensure!(mib > 0, "vm.toml hardware.memory must be > 0");
            Ok((mib, None))
        }
        Memory::Range { min, max } => {
            let range = format!("{min}..{max}");
            let (_, max_mib) =
                crate::parse_memory_range(&range).context("vm.toml hardware.memory range")?;
            Ok((max_mib, Some(range)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> VmConfig {
        VmConfig {
            config_version: CONFIG_VERSION,
            identity: Identity {
                name: "Fedora".into(),
                uuid: uuid_v4(),
                created: rfc3339_utc_now(),
            },
            hardware: Hardware::default(),
            disks: vec![DiskEntry {
                path: "disks/root.raw".into(),
                ro: false,
            }],
            cdroms: vec![],
            networks: vec![NetworkEntry {
                mode: NetMode::Nat,
                mac: mac_for_uuid("x"),
                ssh_port: 0,
            }],
            shares: vec![],
            boot: BootCfg::default(),
            display: DisplayCfg::default(),
            input: InputCfg::default(),
        }
    }

    #[test]
    fn toml_round_trip_preserves_the_config() {
        let cfg = minimal();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: VmConfig = toml::from_str(&text).unwrap();
        back.validate().unwrap();
        assert_eq!(back.identity.uuid, cfg.identity.uuid);
        assert_eq!(back.hardware.cpus, 4);
        assert_eq!(back.hardware.memory, Memory::Fixed("4096M".into()));
        assert_eq!(back.disks.len(), 1);
        assert_eq!(back.disks[0].path, PathBuf::from("disks/root.raw"));
        assert_eq!(back.networks[0].ssh_port, 0);
        assert!(back.display.window);
        assert!(back.input.swap_cmd_opt);
    }

    #[test]
    fn memory_parses_both_forms() {
        // Fixed, with and without a suffix.
        let m: Memory = toml::from_str::<Hardware>("memory = \"8G\"")
            .unwrap()
            .memory;
        assert_eq!(m, Memory::Fixed("8G".into()));
        assert_eq!(memory_to_cli(&m).unwrap(), (8192, None));
        // Range table.
        let m: Memory = toml::from_str::<Hardware>("memory = { min = \"2G\", max = \"8G\" }")
            .unwrap()
            .memory;
        assert_eq!(
            memory_to_cli(&m).unwrap(),
            (8192, Some("2G..8G".to_string()))
        );
        // A bad range (min > max) is rejected by the shared parser.
        let bad = Memory::Range {
            min: "8G".into(),
            max: "2G".into(),
        };
        assert!(memory_to_cli(&bad).is_err());
    }

    #[test]
    fn unknown_keys_are_tolerated_but_newer_versions_are_rejected() {
        let mut cfg = minimal();
        let mut text = toml::to_string_pretty(&cfg).unwrap();
        // An unknown scalar prepended before the tables parses fine (forward compat).
        text = format!("some_future_knob = true\n{text}");
        let back: VmConfig = toml::from_str(&text).unwrap();
        back.validate().unwrap();
        // config_version = 2 parses but fails validation with a "newer limina" message.
        cfg.config_version = 2;
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: VmConfig = toml::from_str(&text).unwrap();
        let err = back.validate().unwrap_err().to_string();
        assert!(err.contains("newer limina"), "unexpected error: {err}");
    }

    #[test]
    fn uuid_v4_has_version_and_variant_bits() {
        let u = uuid_v4();
        assert_eq!(u.len(), 36);
        let bytes: Vec<&str> = u.split('-').collect();
        assert_eq!(
            bytes.iter().map(|s| s.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(u.as_bytes()[14] == b'4', "version nibble must be 4: {u}");
        assert!(
            matches!(u.as_bytes()[19], b'8' | b'9' | b'a' | b'b'),
            "variant nibble must be 8/9/a/b: {u}"
        );
        assert_ne!(uuid_v4(), uuid_v4(), "uuids must not repeat");
    }

    #[test]
    fn mac_is_stable_locally_administered_unicast() {
        let a = mac_for_uuid("3f2c1e9a-0000-4000-8000-000000000001");
        let b = mac_for_uuid("3f2c1e9a-0000-4000-8000-000000000001");
        assert_eq!(a, b, "same uuid → same MAC");
        assert_ne!(
            a,
            mac_for_uuid("different"),
            "different uuid → different MAC"
        );
        let first = u8::from_str_radix(&a[0..2], 16).unwrap();
        assert_eq!(first & 0x01, 0, "unicast bit clear: {a}");
        assert_eq!(first & 0x02, 0x02, "locally-administered bit set: {a}");
    }

    #[test]
    fn rfc3339_formats_known_instants() {
        assert_eq!(rfc3339_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_from_unix(951_782_400), "2000-02-29T00:00:00Z"); // leap day
        assert_eq!(rfc3339_from_unix(1_782_950_399), "2026-07-01T23:59:59Z");
    }
}
