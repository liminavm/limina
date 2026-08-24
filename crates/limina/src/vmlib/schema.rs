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
    #[serde(default)]
    pub power: PowerCfg,
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
    /// How hard the balloon reclaims idle guest memory: disabled / light / moderate
    /// (default) / aggressive. See `--reclaim`.
    pub reclaim: crate::balloon_policy::ReclaimMode,
    /// The maximum; managed VMs always boot dynamic with a [`DYNAMIC_MIN_MIB`] floor.
    pub memory: Memory,
    /// Mirror the host battery into the guest (virtio-i2c SBS battery; default true).
    /// Even when true, nothing attaches on a battery-less host. See `--no-battery`.
    pub battery: bool,
    /// Attach the native virtio-snd audio device driving host CoreAudio (default true).
    /// The guest's stock virtio_snd driver binds it with no guest components. See `--no-snd`.
    #[serde(default = "default_true")]
    pub snd: bool,
    /// Let the guest capture the host microphone via the virtio-snd input stream.
    /// Opt-in, default false for privacy (unlike playback). No effect if `snd = false`.
    /// See `--mic`.
    #[serde(default)]
    pub mic: bool,
    /// Attach the emulated xHCI USB controller (platform `generic-xhci`; default true).
    /// A stock guest binds it with its own xhci-plat driver (no guest components); it also
    /// carries the FIDO authenticator gadget. Harmless on a guest that ignores it. See `--no-usb`.
    #[serde(default = "default_true")]
    pub usb: bool,
    /// Present an impersonated Touch-ID-backed fingerprint reader to the guest (default true).
    /// Implies the USB controller. Stock libfprint/fprintd bind it with no guest components; it is
    /// inert until the guest wires `pam_fprintd` in, and only advertised on a Mac with a usable
    /// Touch ID sensor (else silently absent — stock-degrade). See `--no-fingerprint`.
    #[serde(default = "default_true")]
    pub fingerprint: bool,
    /// Offer the guest a Touch-ID-backed FIDO2/WebAuthn authenticator (default true). Unlike the
    /// reader above this is **transport-independent**: it covers both the stock-tier USB gadget and
    /// the enhanced-tier uhid device the agent creates, so setting it false leaves the guest with
    /// no passkey surface at all — which is the point, since this VM's passkeys live in the host
    /// keychain. Only advertised where a Secure Enclave can back it (else silently absent —
    /// stock-degrade). See `--no-fido`.
    #[serde(default = "default_true")]
    pub fido: bool,
    /// Advertise `VIRTIO_BALLOON_F_DEFLATE_ON_OOM` to the guest (default false; M6
    /// addendum). The bit makes Linux keep ballooned pages inside `MemTotal`, so an inflated
    /// dynamic VM reads as nearly out of memory and systemd-oomd fires; without it accounting
    /// stays truthful and the supervisor's PSI policy owns the release path. Escape hatch only.
    /// See `--balloon-deflate-on-oom`.
    #[serde(default)]
    pub balloon_deflate_on_oom: bool,
}

fn default_true() -> bool {
    true
}

/// `[power]` — host power-event policy (docs/design/host-sleep-s2idle.md).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PowerCfg {
    /// What to do with the guest when the HOST goes to sleep: `s2idle` (default) suspends
    /// the guest first and wakes it on host wake — its own thaw restores the wall clock on
    /// every tier; `ignore` leaves it running with a frozen counter (pre-M9.5 behavior).
    pub on_host_sleep: OnHostSleep,
}

/// The `[power] on_host_sleep` policy values.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnHostSleep {
    #[default]
    S2idle,
    Ignore,
}

impl OnHostSleep {
    /// The value the `--on-host-sleep` flag (supervisor and worker) expects.
    pub fn as_flag(&self) -> &'static str {
        match self {
            OnHostSleep::S2idle => "s2idle",
            OnHostSleep::Ignore => "ignore",
        }
    }
}

impl Default for Hardware {
    fn default() -> Self {
        Self {
            cpus: 4,
            reclaim: crate::balloon_policy::ReclaimMode::Moderate,
            memory: Memory::default(),
            battery: true,
            snd: true,
            mic: false,
            usb: true,
            fingerprint: true,
            fido: true,
            balloon_deflate_on_oom: false,
        }
    }
}

/// Guest memory: a size string (`"4G"`, `"8GiB"`, bare MiB) that is the **maximum**. Managed
/// VMs are always dynamic: the balloon may reclaim idle memory down to a fixed
/// [`DYNAMIC_MIN_MIB`] floor, governed by `hardware.reclaim` (set `reclaim = "disabled"` for
/// static-like behavior — the balloon then never engages). Parsed by the same `parse_size_mib`
/// the CLI flags use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory(pub String);

/// The dynamic-memory floor for managed VMs (MiB): the balloon never shrinks effective guest
/// RAM below this. Clamped to the configured maximum for tiny VMs.
pub const DYNAMIC_MIN_MIB: usize = 1024;

impl Default for Memory {
    fn default() -> Self {
        Memory("4G".into())
    }
}

impl Memory {
    /// Validate + normalize a CLI/UI memory string (the maximum; no ranges here — the min is
    /// always [`DYNAMIC_MIN_MIB`]).
    pub fn parse(s: &str) -> Result<Self> {
        let mib = crate::parse_size_mib(s)?;
        anyhow::ensure!(mib > 0, "memory must be > 0: {s:?}");
        Ok(Memory(s.trim().to_string()))
    }

    /// The maximum in MiB.
    pub fn max_mib(&self) -> Result<usize> {
        let mib = crate::parse_size_mib(&self.0).context("vm.toml hardware.memory")?;
        anyhow::ensure!(mib > 0, "vm.toml hardware.memory must be > 0");
        Ok(mib)
    }

    /// The `--memory MIN..MAX` argument this configuration boots with (min clamped to max).
    pub fn to_memory_arg(&self) -> Result<String> {
        let max = self.max_mib()?;
        Ok(format!("{}..{max}", DYNAMIC_MIN_MIB.min(max)))
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
    /// What resolution the guest display is driven to. See [`DisplayResolution`].
    pub resolution: DisplayResolution,
    /// What closing the VM window does. See [`WindowCloseAction`]; default = suspend
    /// (the M9.4 UX: closing the window parks the VM, reopening resumes it).
    pub on_window_close: WindowCloseAction,
    /// Drive the guest at the host display's **device pixels** rather than its points, so a
    /// Retina panel renders at its native resolution and the guest picks a 2x scale, instead of
    /// rendering at half resolution and letting Core Animation upscale it.
    ///
    /// Default on: without it a 2x display is visibly soft, which is the whole reason the mode
    /// exists. It costs 4x the guest framebuffer and 4x the fill, so `hidpi = false` restores
    /// the point-for-pixel behavior on a machine where that trade is wrong.
    pub hidpi: bool,
    /// What fullscreen does with the camera-housing strip on a notched built-in display.
    /// See [`NotchPolicy`].
    pub notch: NotchPolicy,
    /// How long the pointer must be pressed against a **fullscreen** guest's edge before it is
    /// released to the rest of the desktop, in **seconds** (0 disables the grab entirely).
    ///
    /// **The unit changed in 2026-08 and old values are migrated** — see [`EdgeHold::from_toml`].
    /// It used to be points of accumulated push, which is a post-ballistics quantity nobody can
    /// perceive: the same nominal distance was either free or a wall depending on how fast you
    /// moved (`spikes/edge-pressure/RESULTS.md` round 3). A duration is directly felt, and it is
    /// the unit the chrome-ask gesture was independently measured into.
    ///
    /// **This is the hold for the TOP edge; the sides earn their release sooner** — see
    /// `fit::edge_timing`. One number cannot serve both gestures: pushing up asks for the macOS
    /// chrome at a target the user can see, while pushing sideways happens mid-travel with nothing
    /// on screen to aim at, and dogfood found the top right and the sides too hard at the same
    /// value.
    ///
    /// Ignored windowed, and needs the Accessibility grant the capture tap already asks for.
    #[serde(rename = "edge-resistance")]
    pub edge_resistance: f64,
}

impl Default for DisplayCfg {
    fn default() -> Self {
        Self {
            window: true,
            gpu: GpuMode::Auto,
            resolution: DisplayResolution::default(),
            on_window_close: WindowCloseAction::default(),
            hidpi: true,
            notch: NotchPolicy::default(),
            edge_resistance: DEFAULT_EDGE_RESISTANCE,
        }
    }
}

/// Default `[display] edge-resistance`: [`EdgeHold::Standard`], in seconds.
pub const DEFAULT_EDGE_RESISTANCE: f64 = EdgeHold::Standard.seconds();

/// The four presets, as the duration a deliberate edge press must last.
///
/// Measured basis (`spikes/edge-pressure/RESULTS.md` round 2): an incidental corner push peaks at
/// 0.02 s of charge while a deliberate lean reaches 1.0 s, so every value here sits in a 50x gap
/// — which is what lets a corner *tap* charge the guest's own hot corner without ever releasing
/// the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeHold {
    /// No grab at all: the pointer behaves exactly as it does outside fullscreen.
    Off,
    Light,
    Standard,
    Firm,
}

impl EdgeHold {
    pub const ALL: [EdgeHold; 4] = [Self::Off, Self::Light, Self::Standard, Self::Firm];

    pub const fn seconds(self) -> f64 {
        match self {
            Self::Off => 0.0,
            Self::Light => 0.15,
            Self::Standard => 0.30,
            Self::Firm => 0.60,
        }
    }

    pub const fn title(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Light => "Light",
            Self::Standard => "Standard",
            Self::Firm => "Firm",
        }
    }

    /// Read a `vm.toml` value, migrating the pre-2026-08 **points** encoding.
    ///
    /// The two ranges cannot overlap — a hold is under a second, the old presets were 50/100/200
    /// — so a number of 10 or more is unambiguously the old unit and is mapped **by preset
    /// position**, not by arithmetic: the old and new scales measure different things, and
    /// pretending 100 pt converts to some number of seconds would be false precision. An
    /// unrecognised old number lands on Standard rather than Off, because silently disabling a
    /// feature someone configured is the worse failure.
    pub fn from_toml(value: f64) -> Self {
        if !value.is_finite() || value <= 0.0 {
            return Self::Off;
        }
        if value >= 10.0 {
            return match value as u32 {
                50 => Self::Light,
                200 => Self::Firm,
                _ => Self::Standard,
            };
        }
        // Already seconds: snap to the nearest preset so the control centre always has a
        // selection, while `Self::seconds` stays the single source of the actual durations.
        Self::ALL
            .into_iter()
            .filter(|p| *p != Self::Off)
            .min_by(|a, b| {
                let d = |p: &Self| (p.seconds() - value).abs();
                d(a).partial_cmp(&d(b)).unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or(Self::Standard)
    }
}

/// The `[display] notch` key — what a fullscreen VM does with the camera-housing strip on a
/// notched built-in display (MacBook Pro / Air).
///
/// The app ships `NSPrefersDisplaySafeAreaCompatibilityMode = true`, which — despite the name —
/// is what makes AppKit hand a fullscreen window the **whole** panel (measured on a 1512x982
/// built-in Retina display: the key absent gives a 949 pt-tall fullscreen window, the key true
/// gives 982; `spikes/notch-fullscreen/`). limina then applies the policy itself, per VM, so the
/// choice is not an app-wide build-time constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NotchPolicy {
    /// Keep the guest below the housing: the scanout is inset by the notch height and that
    /// strip stays black. The default, and what limina did before the key existed.
    #[default]
    Avoid,
    /// Give the guest the full panel. It gains the notch height in usable rows, at the cost of
    /// the housing physically covering the middle of the guest's top edge.
    Extend,
}

impl std::fmt::Display for NotchPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Avoid => "avoid",
            Self::Extend => "extend",
        })
    }
}

impl std::str::FromStr for NotchPolicy {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim() {
            "avoid" => Ok(Self::Avoid),
            "extend" => Ok(Self::Extend),
            other => anyhow::bail!("display notch must be \"avoid\" or \"extend\": got {other:?}"),
        }
    }
}

/// The `[display] on_window_close` key — what closing the VM window means (M9.4).
/// `limina stop` / Ctrl-C always power off regardless; this only governs the red button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WindowCloseAction {
    /// Suspend the VM (snapshot + teardown; the next start resumes). The default.
    #[default]
    Suspend,
    /// Power the guest off (the pre-M9.4 behavior).
    Shutdown,
    /// Ask each time (Suspend / Shut Down / Cancel).
    Ask,
}

/// The `[display] resolution` key — one key, three shapes, no invalid combos:
///
/// - `"host"` (default): the guest is driven to the point size of the screen the
///   window is on; the window letterboxes as needed. Fullscreen needs no modeset.
/// - `"dynamic"`: the guest follows the window (drag-end resize push), and guest
///   modesets resize the window — the original shipped behavior.
/// - `"WIDTHxHEIGHT"`: fixed; the guest is driven there once at boot, never again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum DisplayResolution {
    #[default]
    Host,
    Dynamic,
    Fixed(u32, u32),
}

impl std::fmt::Display for DisplayResolution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Host => f.write_str("host"),
            Self::Dynamic => f.write_str("dynamic"),
            Self::Fixed(w, h) => write!(f, "{w}x{h}"),
        }
    }
}

impl std::str::FromStr for DisplayResolution {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();
        match s {
            "host" => return Ok(Self::Host),
            "dynamic" => return Ok(Self::Dynamic),
            _ => {}
        }
        let parsed = s.split_once(['x', 'X']).and_then(|(w, h)| {
            Some((w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?))
        });
        match parsed {
            // 64 px is the floor the runtime resize path already enforces; reject
            // smaller fixed sizes at load instead of boot.
            Some((w, h)) if w >= 64 && h >= 64 => Ok(Self::Fixed(w, h)),
            _ => anyhow::bail!(
                "display resolution must be \"host\", \"dynamic\", or WIDTHxHEIGHT \
                 with both dimensions >= 64 (e.g. \"1920x1080\"): got {s:?}"
            ),
        }
    }
}

impl TryFrom<String> for DisplayResolution {
    type Error = anyhow::Error;

    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

impl From<DisplayResolution> for String {
    fn from(r: DisplayResolution) -> String {
        r.to_string()
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
    /// Modifier normalization: Control stays Control, the Option position becomes Meta/Super and
    /// the Command position becomes Alt. The TOML key keeps its original name so existing
    /// `vm.toml` files still parse; the *rule* is positional, which is why it looks like a swap.
    /// Seeds the Input menu's switch, which can then move it for the session.
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

/// Parse a `Memory` into what the CLI consumes: `(ram_mib, "MIN..MAX")`. Managed VMs always
/// boot dynamic — `ram_mib` is the max (what libkrun allocates) and the range floors at
/// [`DYNAMIC_MIN_MIB`] (clamped for tiny VMs).
pub fn memory_to_cli(memory: &Memory) -> Result<(usize, Option<String>)> {
    Ok((memory.max_mib()?, Some(memory.to_memory_arg()?)))
}

#[cfg(test)]
mod edge_hold_tests {
    use super::*;

    #[test]
    fn the_old_points_encoding_migrates_by_preset_position() {
        // Every managed VM on disk carries one of these. The two scales measure different things,
        // so the mapping is positional — converting 100 pt into "some seconds" would be false
        // precision.
        assert_eq!(EdgeHold::from_toml(50.0), EdgeHold::Light);
        assert_eq!(EdgeHold::from_toml(100.0), EdgeHold::Standard);
        assert_eq!(EdgeHold::from_toml(200.0), EdgeHold::Firm);
        assert_eq!(
            EdgeHold::from_toml(0.0),
            EdgeHold::Off,
            "0 always meant off"
        );
    }

    #[test]
    fn a_hand_edited_old_value_lands_on_standard_not_off() {
        // Silently disabling a feature someone deliberately configured is the worse failure.
        assert_eq!(EdgeHold::from_toml(137.0), EdgeHold::Standard);
        assert_eq!(EdgeHold::from_toml(1000.0), EdgeHold::Standard);
    }

    #[test]
    fn a_value_already_in_seconds_snaps_to_its_preset() {
        for p in EdgeHold::ALL {
            if p == EdgeHold::Off {
                continue;
            }
            assert_eq!(
                EdgeHold::from_toml(p.seconds()),
                p,
                "{} round-trips",
                p.title()
            );
        }
        assert_eq!(
            EdgeHold::from_toml(0.31),
            EdgeHold::Standard,
            "near-miss snaps"
        );
    }

    #[test]
    fn nonsense_is_off_rather_than_a_panic() {
        // It comes from a user-editable file.
        for v in [f64::NAN, f64::INFINITY, -1.0, -0.0] {
            assert_eq!(EdgeHold::from_toml(v), EdgeHold::Off, "{v} is not a hold");
        }
        // ...but a positive infinity is not "off" by accident of sign: it is >= 10, so it would
        // migrate as an old value. Guard that it does not slip through as a duration.
        assert!(EdgeHold::from_toml(f64::INFINITY).seconds().is_finite());
    }

    #[test]
    fn the_default_is_standard_and_every_preset_sits_in_the_measured_gap() {
        assert_eq!(
            EdgeHold::from_toml(DEFAULT_EDGE_RESISTANCE),
            EdgeHold::Standard
        );
        // An incidental corner push peaks at 0.02 s of charge; a deliberate lean reaches 1.0 s.
        for p in EdgeHold::ALL {
            if p == EdgeHold::Off {
                continue;
            }
            assert!(
                p.seconds() > 0.05,
                "{} must not fire on an incidental push",
                p.title()
            );
            assert!(
                p.seconds() < 1.0,
                "{} must be reachable by a deliberate lean",
                p.title()
            );
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
            power: PowerCfg::default(),
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
        assert_eq!(back.hardware.memory, Memory("4G".into()));
        assert_eq!(back.disks.len(), 1);
        assert_eq!(back.disks[0].path, PathBuf::from("disks/root.raw"));
        assert_eq!(back.networks[0].ssh_port, 0);
        assert!(back.display.window);
        assert_eq!(back.display.resolution, DisplayResolution::Host);
        assert!(back.input.swap_cmd_opt);
    }

    #[test]
    fn balloon_deflate_on_oom_defaults_off_and_round_trips() {
        // Absent key (every pre-existing vm.toml) → off: accounting stays transparent.
        let h: Hardware = toml::from_str("").unwrap();
        assert!(!h.balloon_deflate_on_oom);

        // The escape hatch survives a save/load cycle.
        let h: Hardware = toml::from_str("balloon_deflate_on_oom = true").unwrap();
        assert!(h.balloon_deflate_on_oom);
        let back: Hardware = toml::from_str(&toml::to_string(&h).unwrap()).unwrap();
        assert!(back.balloon_deflate_on_oom);
    }

    #[test]
    fn display_resolution_round_trips_and_defaults_to_host() {
        // The key is absent → match-host (the default for new AND pre-existing VMs).
        let d: DisplayCfg = toml::from_str("").unwrap();
        assert_eq!(d.resolution, DisplayResolution::Host);

        for (text, want) in [
            ("resolution = \"host\"", DisplayResolution::Host),
            ("resolution = \"dynamic\"", DisplayResolution::Dynamic),
            (
                "resolution = \"1920x1080\"",
                DisplayResolution::Fixed(1920, 1080),
            ),
        ] {
            let d: DisplayCfg = toml::from_str(text).unwrap();
            assert_eq!(d.resolution, want, "{text}");
            // Serialize → parse round-trip preserves the value.
            let back: DisplayCfg = toml::from_str(&toml::to_string(&d).unwrap()).unwrap();
            assert_eq!(back.resolution, want, "round-trip of {text}");
        }
    }

    #[test]
    fn window_close_action_round_trips_and_defaults_to_suspend() {
        // Absent key → suspend (the M9.4 default: closing the window parks the VM). This is
        // also what every pre-M9.4 vm.toml resolves to.
        let d: DisplayCfg = toml::from_str("").unwrap();
        assert_eq!(d.on_window_close, WindowCloseAction::Suspend);

        for (text, want) in [
            ("on_window_close = \"suspend\"", WindowCloseAction::Suspend),
            (
                "on_window_close = \"shutdown\"",
                WindowCloseAction::Shutdown,
            ),
            ("on_window_close = \"ask\"", WindowCloseAction::Ask),
        ] {
            let d: DisplayCfg = toml::from_str(text).unwrap();
            assert_eq!(d.on_window_close, want, "{text}");
            let back: DisplayCfg = toml::from_str(&toml::to_string(&d).unwrap()).unwrap();
            assert_eq!(back.on_window_close, want, "round-trip of {text}");
        }
        // Garbage is rejected loudly, not defaulted.
        assert!(toml::from_str::<DisplayCfg>("on_window_close = \"park\"").is_err());
    }

    #[test]
    fn display_resolution_rejects_garbage() {
        for bad in ["800x", "axb", "0x0", "x600", "800", "32x32", ""] {
            let err = bad.parse::<DisplayResolution>();
            assert!(err.is_err(), "{bad:?} should be rejected");
            let text = format!("resolution = {bad:?}");
            assert!(
                toml::from_str::<DisplayCfg>(&text).is_err(),
                "{text} should fail to deserialize"
            );
        }
        // Whitespace and a capital X are tolerated.
        assert_eq!(
            " 1280 x 800 ".parse::<DisplayResolution>().unwrap(),
            DisplayResolution::Fixed(1280, 800)
        );
        assert_eq!(
            "1280X800".parse::<DisplayResolution>().unwrap(),
            DisplayResolution::Fixed(1280, 800)
        );
    }

    #[test]
    fn memory_is_the_maximum_and_boots_dynamic() {
        // A plain size string is the max; the CLI form floors at DYNAMIC_MIN_MIB.
        let m: Memory = toml::from_str::<Hardware>("memory = \"8G\"")
            .unwrap()
            .memory;
        assert_eq!(m, Memory("8G".into()));
        assert_eq!(
            memory_to_cli(&m).unwrap(),
            (8192, Some("1024..8192".to_string()))
        );
        // Tiny VM: the floor clamps to the max (a degenerate range the policy no-ops on).
        let m = Memory("512M".into());
        assert_eq!(
            memory_to_cli(&m).unwrap(),
            (512, Some("512..512".to_string()))
        );
        // Garbage is rejected.
        assert!(Memory("8Q".into()).max_mib().is_err());
        assert!(Memory::parse("0").is_err());
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
