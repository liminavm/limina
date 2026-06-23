// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! End-to-end test harness for limina.
//!
//! The whole point of this crate is to exercise limina **the way a user does** — by
//! launching the real `limina` supervisor binary, which spawns the real (codesigned)
//! `limina-vmm` worker, which boots a guest on Hypervisor.framework. We assert on what
//! the guest emits and then drive a clean teardown. There are deliberately **no
//! shortcuts to libkrun's internal API**: if a test passes here, the shipped binaries
//! work.
//!
//! Boot tests need HVF (codesign + `com.apple.security.hypervisor`) and a guest image,
//! so they cannot run under a plain sandboxed `cargo test`. They are gated behind the
//! `LIMINA_HVF_TESTS` env var; use [`require_hvf_or_skip`] at the top of each. The
//! `scripts/test-boot.sh` runner builds, signs, and runs them with the gate set.
//!
//! ## Layers (see docs/roadmap.md testing plan)
//! - **L0** — pure unit tests in the crates themselves (no HVF). Not here.
//! - **L1** — fast boot tests against our own tiny kernel + Rust init (sub-second).
//!   *Coming next*; the [`Guest`] harness is built to host them.
//! - **L2** — stock-baseline conformance against the unmodified Fedora image. This is
//!   what `tests/boot.rs` exercises today.

use std::ffi::OsStr;
use std::fs;
use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

/// Stable location of the krunkit EFI firmware blob (an EDK2 `.fd`). Overridable with
/// `LIMINA_FIRMWARE`. This is the same firmware the M1 boot spikes used. It is "silent" — no
/// GOP-*producing* video driver — so firmware/GRUB render only to the serial console.
const DEFAULT_FIRMWARE: &str = "/opt/homebrew/share/krunkit/KRUN_EFI.silent.fd";

/// Stable location of OUR GOP-capable EDK2 firmware (built by `scripts/build-krun-efi.sh`,
/// carries VirtioGpuDxe). Overridable with `LIMINA_GOP_FIRMWARE`. Unlike the silent firmware,
/// firmware → GRUB → kernel all render to the virtio-gpu scanout — the windowed boot console.
const DEFAULT_GOP_FIRMWARE: &str = "target/krun-efi/KRUN_EFI.gop.fd";

/// The guest image the tests boot from, by default — a **frozen CoW snapshot** of the dev
/// image, NOT the live `Fedora-Workstation-43.raw` (which evolves as we provision/develop the
/// guest). Created once with `cp -c Fedora-Workstation-43.raw Fedora-Workstation-43.test.raw`
/// (APFS clone: instant, shares blocks; each side copies only what it writes, so the dev image
/// can change without perturbing tests). Refresh the fixture by re-cloning. Per-run the harness
/// still makes its own writable clone of this for net/disk-root boots. Override: `LIMINA_TEST_DISK`.
/// NOTE: this image carries a baked-in `claude` test user + SSH key (see memory
/// `limina-fedora-access`); making that provisioning reproducible is a tracked follow-up.
const DEFAULT_TEST_DISK: &str = "Fedora-Workstation-43.test.raw";

/// gvproxy's default inbound port-forward: `127.0.0.1:2222 → 192.168.127.2:22`. With the
/// well-known vfkit MAC the guest gets the static `.2` lease, so this reaches its sshd.
const FORWARDED_SSH_ADDR: &str = "127.0.0.1:2222";

/// Shared scp options matching [`Guest::ssh_exec`]'s ssh invocation.
const SCP_OPTS: [&str; 11] = [
    "-P",
    "2222",
    "-o",
    "StrictHostKeyChecking=no",
    "-o",
    "UserKnownHostsFile=/dev/null",
    "-o",
    "BatchMode=yes",
    "-o",
    "LogLevel=ERROR",
    "-q",
];

/// Is HVF-backed boot testing enabled for this run?
///
/// Returns true iff `LIMINA_HVF_TESTS` is set to a truthy value (`1`/`true`/`yes`). Boot
/// tests are skipped otherwise so a plain `cargo test` (no codesigning, often
/// sandboxed) stays green.
pub fn hvf_enabled() -> bool {
    matches!(
        std::env::var("LIMINA_HVF_TESTS").ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

/// Gate a boot test. Call at the top: `if !require_hvf_or_skip("name") { return; }`.
///
/// Prints a clear SKIPPED line (Rust's test harness has no native "skipped" state, so a
/// skipped boot test reports as passed — the message is how you tell them apart).
#[must_use]
pub fn require_hvf_or_skip(test_name: &str) -> bool {
    if hvf_enabled() {
        return true;
    }
    eprintln!(
        "SKIPPED {test_name}: HVF boot tests are off. Set LIMINA_HVF_TESTS=1 and run via \
         scripts/test-boot.sh (builds + codesigns the worker)."
    );
    false
}

/// The cargo target profile dir (e.g. `target/debug`), derived from the running test
/// binary, which lives at `target/<profile>/deps/<test>-<hash>`.
fn target_profile_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("current_exe")?;
    // .../target/<profile>/deps/<test bin>  ->  .../target/<profile>
    let deps = exe.parent().context("test exe has no parent")?;
    let profile = if deps.file_name() == Some(OsStr::new("deps")) {
        deps.parent().context("deps has no parent")?
    } else {
        deps
    };
    Ok(profile.to_path_buf())
}

/// Resolve a built binary by name: `$<env_override>` if set, else next to the test
/// binary in the cargo target dir.
fn resolve_bin(name: &str, env_override: &str) -> Result<PathBuf> {
    if let Ok(p) = std::env::var(env_override) {
        let p = PathBuf::from(p);
        anyhow::ensure!(p.exists(), "{env_override}={p:?} does not exist");
        return Ok(p);
    }
    let p = target_profile_dir()?.join(name);
    anyhow::ensure!(
        p.exists(),
        "{name} not found at {p:?}; build it first (cargo build -p limina -p limina-vmm) \
         or set {env_override}"
    );
    Ok(p)
}

/// Resolve the **KosmicKrisp** host Vulkan ICD — limina's one supported venus backend.
///
/// venus (the guest 3D path) is driven on the host by a Vulkan driver. We support exactly one:
/// KosmicKrisp (mesa's Vulkan-on-Metal), a machine-local dev build under
/// `/Volumes/mesa-cs/build-kk`. MoltenVK is **not** supported — its venus path corrupts the
/// guest compositor (the #28 coherency / #32 stencil class of bugs KK fixed), so we never fall
/// through to the Vulkan loader's MoltenVK default. Returns `None` when KK isn't built/mounted;
/// callers either SKIP (a venus-requiring test) or degrade to software-2D (see [`Guest::boot`]).
/// Same discovery as `spikes/venus-draw-probe/boot-seated-kk.sh`.
pub fn kosmickrisp_icd() -> Option<PathBuf> {
    let dir = Path::new("/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan");
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with("kosmickrisp_mesa_devenv_icd.") && n.ends_with(".json")
            })
        })
}

/// Resolve the **zink-on-KosmicKrisp** host GL Mesa prefix — the GL provider for
/// virglrenderer's `vrend` (the baseline-tier virgl path; see memory `limina-baseline-3d-plan`
/// and `spikes/virgl-zink-kk`). A stock 4 KiB guest can't map venus's host-visible blobs, so it
/// drives virgl/vrend, whose host GL is zink-on-KK from this machine-local Mesa build (default
/// `/Volumes/mesa-cs/zink-kk-prefix`, override `MESA_PREFIX`). Returns `None` when it isn't
/// built/mounted; a virgl-requiring test should SKIP. Same prefix as
/// `spikes/virgl-zink-kk/boot-virgl-guest.sh` and the worker `build.rs`.
pub fn zink_kk_mesa_prefix() -> Option<PathBuf> {
    let prefix = std::env::var("MESA_PREFIX")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/Volumes/mesa-cs/zink-kk-prefix"));
    // egl.pc is the load-bearing artifact (vrend dlopens libEGL/libGLESv2 from here at runtime).
    prefix
        .join("lib/pkgconfig/egl.pc")
        .exists()
        .then_some(prefix)
}

/// What the guest boots from.
#[derive(Debug, Clone)]
pub enum Boot {
    /// EFI firmware + a disk image — the L2 stock-baseline path.
    Firmware {
        firmware: PathBuf,
        disk: PathBuf,
        /// Open the disk read-only so tests never mutate a shared image.
        read_only: bool,
    },
    /// Direct kernel + virtio-fs rootfs directory + cmdline — the fast L1 path.
    Kernel {
        kernel: PathBuf,
        rootfs: PathBuf,
        cmdline: String,
    },
    /// Direct kernel + a virtio-blk disk holding the root fs + cmdline — the **enhanced
    /// tier**: our custom (e.g. 16 KiB-page) kernel direct-booting a real distro disk with
    /// no initramfs (all drivers built in). The cmdline names the root device (e.g.
    /// `root=/dev/vda3 rootflags=subvol=root rootfstype=btrfs`).
    KernelDisk {
        kernel: PathBuf,
        disk: PathBuf,
        cmdline: String,
    },
}

/// A vsock channel to the guest agent: the host listens on `socket_path`, the guest
/// connects to `CID_HOST:port`.
#[derive(Debug, Clone)]
pub struct VsockCfg {
    pub port: u32,
    pub socket_path: PathBuf,
}

/// A virtio-gpu display attached to the guest, with frame capture for assertions.
#[derive(Debug, Clone)]
pub struct DisplayCfg {
    pub width: u32,
    pub height: u32,
    /// Force the software-2D-only GPU (`--gpu-software-2d`). True for the deterministic 2D
    /// capture oracle (no venus/Metal dep); false to run the default **coexist** device so
    /// venus 3D is available (the enhanced-tier path). A display is required for the GPU
    /// device to exist at all, even when a test only probes 3D over SSH.
    pub software_2d: bool,
}

/// Which guest console device an interactive (`console_input`) session is wired through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleChannel {
    /// The PL011 serial tty (`ttyAMA0`) — the firmware/early-boot console *and*, since the
    /// halfword-MMIO fix, a working interactive serial shell (the M2.5 Track A goal).
    Serial,
    /// virtio-console (`hvc0`) — the robust bidirectional data console.
    Virtio,
}

/// How to boot a guest under the harness.
#[derive(Debug, Clone)]
pub struct GuestConfig {
    /// The `limina` supervisor binary (it locates the `limina-vmm` worker as its sibling).
    pub limina_bin: PathBuf,
    /// The `limina-vmm` worker binary — not launched directly, but tracked so the harness
    /// can guarantee no orphaned VM survives a wedged supervisor.
    pub vmm_bin: PathBuf,
    /// What the guest boots from.
    pub boot: Boot,
    /// Optional vsock channel to the guest agent.
    pub vsock: Option<VsockCfg>,
    /// Optional virtio-gpu display; when set, frames are captured for assertions.
    pub display: Option<DisplayCfg>,
    /// vCPUs.
    pub cpus: u8,
    /// Guest RAM in MiB.
    pub ram_mib: usize,
    /// Grace the *supervisor* gives the guest to power off before it force-kills the
    /// worker. Kept short for tests.
    pub shutdown_grace: Duration,
    /// Wire an interactive console: the harness feeds the guest serial *input* via a FIFO
    /// (so a test can type), in addition to capturing output. See [`Guest::console_send`].
    pub console_input: bool,
    /// Which device an interactive console is wired through (only meaningful when
    /// `console_input` is set).
    pub console_channel: ConsoleChannel,
    /// Attach a user-mode NAT NIC (the supervisor spawns + supervises a gvproxy gateway and
    /// captures its `-debug` packet log — the host-side network oracle). See [`Guest::wait_for_gateway_log`].
    pub net: bool,
    /// Capture the supervisor's own stderr (its log) to a scratch file instead of letting
    /// it flow to the test's stderr — for asserting on supervisor-side events (e.g. the
    /// control plane's "guest agent connected"). See [`Guest::wait_for_supervisor_log`].
    pub supervisor_log: bool,
    /// Pin the supervisor-owned control socket to a known scratch path (`--control-socket`)
    /// so the harness can join the plane as a peer itself. See [`Guest::connect_control`].
    pub control_socket: bool,
    /// Extra environment variables for the supervisor process (e.g. `LIMINA_PASTEBOARD`).
    pub envs: Vec<(String, String)>,
    /// Host directories shared into the guest (`--share name=path[:ro]` → virtiofs tag
    /// `limina-<name>`, auto-mounted at `/media/<name>` by the guest init/agent). The
    /// bool is read-only.
    pub shares: Vec<(String, PathBuf, bool)>,
}

impl GuestConfig {
    /// L2 config: the in-repo Fedora image via EFI firmware (read-only).
    ///
    /// Overrides: `LIMINA_BIN`, `LIMINA_VMM_BIN`, `LIMINA_FIRMWARE`, `LIMINA_TEST_DISK`,
    /// `LIMINA_TEST_SHUTDOWN_GRACE` (seconds).
    pub fn fedora_from_env() -> Result<GuestConfig> {
        let firmware = std::env::var("LIMINA_FIRMWARE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_FIRMWARE));
        anyhow::ensure!(
            firmware.exists(),
            "firmware not found at {firmware:?} (set LIMINA_FIRMWARE)"
        );

        let disk = match std::env::var("LIMINA_TEST_DISK") {
            Ok(p) => PathBuf::from(p),
            Err(_) => repo_root().join(DEFAULT_TEST_DISK),
        };
        anyhow::ensure!(
            disk.exists(),
            "guest disk not found at {disk:?} (set LIMINA_TEST_DISK)"
        );

        Ok(GuestConfig {
            limina_bin: resolve_bin("limina", "LIMINA_BIN")?,
            vmm_bin: resolve_bin("limina-vmm", "LIMINA_VMM_BIN")?,
            boot: Boot::Firmware {
                firmware,
                disk,
                read_only: true,
            },
            vsock: None,
            display: None,
            cpus: 4,
            ram_mib: 4096,
            shutdown_grace: Duration::from_secs(grace_from_env()),
            console_input: false,
            console_channel: ConsoleChannel::Virtio,
            net: false,
            supervisor_log: false,
            control_socket: false,
            envs: Vec::new(),
            shares: Vec::new(),
        })
    }

    /// L2 config that EFI-boots the Fedora image on **our GOP firmware** with a captured
    /// software-2D display + NAT — the vehicle for the *visual* boot test (firmware → GRUB →
    /// kernel rendered into the window, read via [`Guest::wait_for_capture`]). `with_net` forces
    /// a writable COW clone, so the shared image is never mutated and the guest can complete boot.
    ///
    /// Overrides: `LIMINA_GOP_FIRMWARE` (default `target/krun-efi/KRUN_EFI.gop.fd`), plus the usual
    /// `LIMINA_TEST_DISK`/`LIMINA_BIN`/`LIMINA_VMM_BIN`. Returns an error (the test should SKIP) if the
    /// GOP firmware is missing — build it with `scripts/build-krun-efi.sh`.
    pub fn fedora_gop_from_env() -> Result<GuestConfig> {
        let firmware = std::env::var("LIMINA_GOP_FIRMWARE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| repo_root().join(DEFAULT_GOP_FIRMWARE));
        anyhow::ensure!(
            firmware.exists(),
            "GOP firmware not found at {firmware:?}; build it with `scripts/build-krun-efi.sh` \
             (or set LIMINA_GOP_FIRMWARE)"
        );
        let mut cfg = GuestConfig::fedora_from_env()?;
        if let Boot::Firmware { firmware: f, .. } = &mut cfg.boot {
            *f = firmware;
        }
        // software-2D display capture (the GOP scanout oracle) + NAT (writable clone + sshd).
        Ok(cfg.with_display(1280, 800).with_net())
    }

    /// L2 baseline-tier config: a **stock 4 KiB** Fedora that **autologins to a seated GNOME
    /// session**, EFI-booted on the GOP firmware. The vehicle for baseline-3D (virgl) tests that
    /// need a real GL session over SSH — `Fedora-Workstation-44.boot.raw` (vanilla F44 + autologin
    /// `claude`, no enhancements; see memory `limina-fedora-access` / `docs/images.md`). Pair with
    /// [`with_coexist_display`](GuestConfig::with_coexist_display) +
    /// [`with_virgl_host_gl`](GuestConfig::with_virgl_host_gl) + [`with_net`](GuestConfig::with_net).
    ///
    /// Overrides: `LIMINA_TEST_DISK_BASELINE` (default `Fedora-Workstation-44.boot.raw`),
    /// `LIMINA_GOP_FIRMWARE` (default `target/krun-efi/KRUN_EFI.gop.fd`), plus the usual
    /// `LIMINA_BIN`/`LIMINA_VMM_BIN`. Returns an error (the test should SKIP) if the GOP firmware
    /// or the baseline disk is missing.
    pub fn baseline_fedora_from_env() -> Result<GuestConfig> {
        let firmware = std::env::var("LIMINA_GOP_FIRMWARE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| repo_root().join(DEFAULT_GOP_FIRMWARE));
        anyhow::ensure!(
            firmware.exists(),
            "GOP firmware not found at {firmware:?}; build it with `scripts/build-krun-efi.sh` \
             (or set LIMINA_GOP_FIRMWARE)"
        );
        let disk = match std::env::var("LIMINA_TEST_DISK_BASELINE") {
            Ok(p) => PathBuf::from(p),
            Err(_) => repo_root().join("Fedora-Workstation-44.boot.raw"),
        };
        anyhow::ensure!(
            disk.exists(),
            "baseline (4 KiB autologin) disk not found at {disk:?} (set LIMINA_TEST_DISK_BASELINE); \
             this is the machine-local seated baseline, see memory `limina-fedora-access`"
        );
        let mut cfg = GuestConfig::fedora_from_env()?;
        cfg.boot = Boot::Firmware {
            firmware,
            disk,
            read_only: true, // with_net cow-clones to a writable disk; the source stays pristine
        };
        Ok(cfg)
    }

    /// L1 config: our tiny direct-boot guest (kernel Image + virtio-fs rootfs).
    ///
    /// Build it first with `scripts/build-test-guest.sh`. Overrides: `LIMINA_BIN`,
    /// `LIMINA_VMM_BIN`, `LIMINA_TEST_KERNEL`, `LIMINA_TEST_ROOTFS`, `LIMINA_TEST_CMDLINE`,
    /// `LIMINA_TEST_SHUTDOWN_GRACE`.
    pub fn l1_from_env() -> Result<GuestConfig> {
        let guest_dir = repo_root().join("target/test-guest");
        let kernel = std::env::var("LIMINA_TEST_KERNEL")
            .map(PathBuf::from)
            .unwrap_or_else(|_| guest_dir.join("Image"));
        let rootfs = std::env::var("LIMINA_TEST_ROOTFS")
            .map(PathBuf::from)
            .unwrap_or_else(|_| guest_dir.join("rootfs"));
        anyhow::ensure!(
            kernel.exists() && rootfs.exists(),
            "L1 guest missing ({kernel:?} / {rootfs:?}); run scripts/build-test-guest.sh"
        );
        let cmdline = std::env::var("LIMINA_TEST_CMDLINE")
            .unwrap_or_else(|_| "console=ttyAMA0 rootfstype=virtiofs rw init=/init".to_string());

        Ok(GuestConfig {
            limina_bin: resolve_bin("limina", "LIMINA_BIN")?,
            vmm_bin: resolve_bin("limina-vmm", "LIMINA_VMM_BIN")?,
            boot: Boot::Kernel {
                kernel,
                rootfs,
                cmdline,
            },
            vsock: None,
            display: None,
            cpus: 2,
            ram_mib: 1024,
            shutdown_grace: Duration::from_secs(grace_from_env()),
            console_input: false,
            console_channel: ConsoleChannel::Virtio,
            net: false,
            supervisor_log: false,
            control_socket: false,
            envs: Vec::new(),
            shares: Vec::new(),
        })
    }

    /// Enhanced-tier config: our custom **16 KiB-page** kernel direct-booting the in-repo
    /// Fedora image's btrfs root (no initramfs), with the coexist (venus) GPU and NAT so a
    /// test can SSH in and confirm venus. A 16 KiB guest places host-visible virtio-gpu blobs
    /// on 16 KiB boundaries, so `hv_vm_map` accepts them and venus works (vs the stock 4 KiB
    /// guest, which degrades to llvmpipe) — see memory `limina-tier2-venus`.
    ///
    /// Build the kernel first: `scripts/build-test-kernel.sh PAGESIZE=16k`. Overrides:
    /// `LIMINA_TEST_KERNEL_16K` (default `target/test-guest/kernel/Image-16k`), `LIMINA_TEST_DISK`,
    /// plus the usual `LIMINA_BIN`/`LIMINA_VMM_BIN`. Returns an error (the test should SKIP) if the
    /// 16 KiB kernel or the disk is missing.
    pub fn enhanced_fedora_from_env() -> Result<GuestConfig> {
        let kernel = std::env::var("LIMINA_TEST_KERNEL_16K")
            .map(PathBuf::from)
            .unwrap_or_else(|_| repo_root().join("target/test-guest/kernel/Image-16k"));
        anyhow::ensure!(
            kernel.exists(),
            "16 KiB kernel not found at {kernel:?}; build it with \
             `scripts/build-test-kernel.sh PAGESIZE=16k` (or set LIMINA_TEST_KERNEL_16K)"
        );
        let disk = match std::env::var("LIMINA_TEST_DISK") {
            Ok(p) => PathBuf::from(p),
            Err(_) => repo_root().join(DEFAULT_TEST_DISK),
        };
        anyhow::ensure!(
            disk.exists(),
            "guest disk not found at {disk:?} (set LIMINA_TEST_DISK)"
        );

        Ok(GuestConfig {
            limina_bin: resolve_bin("limina", "LIMINA_BIN")?,
            vmm_bin: resolve_bin("limina-vmm", "LIMINA_VMM_BIN")?,
            boot: Boot::KernelDisk {
                kernel,
                disk,
                // vda3 = Fedora's btrfs root (subvol=root); selinux=0 keeps a custom-kernel
                // boot simple; ttyAMA0 surfaces the kernel/login banner in the console capture.
                cmdline: "root=/dev/vda3 rootflags=subvol=root rootfstype=btrfs rw selinux=0 \
                          console=ttyAMA0"
                    .to_string(),
            },
            vsock: None,
            display: None,
            cpus: 4,
            ram_mib: 4096,
            shutdown_grace: Duration::from_secs(grace_from_env()),
            console_input: false,
            console_channel: ConsoleChannel::Virtio,
            net: false,
            supervisor_log: false,
            control_socket: false,
            envs: Vec::new(),
            shares: Vec::new(),
        })
    }

    /// Like [`GuestConfig::enhanced_fedora_from_env`], but booting the **seated dev-enh
    /// golden** (`Fedora-Workstation-43.dev-enh.raw`, override `LIMINA_TEST_DISK_ENH`): gdm
    /// autologin to a gnome-shell-on-venus session with `/opt/mesa-zink`, the x11-enabled
    /// venus ICD, and `apitrace` baked in (see memory `limina-fedora-access` for the bake
    /// inventory). This is the vehicle for tests that need a *running graphical session*
    /// (Xwayland, the zink→venus GL stack) rather than just venus enumeration.
    ///
    /// The host Vulkan backend (KosmicKrisp) is wired automatically by [`Guest::boot`] for any
    /// coexist/venus display — see [`kosmickrisp_icd`]; a venus-requiring test should still SKIP
    /// up front when KK is absent (so it doesn't silently run on the software-2D fallback).
    /// Returns an error (the test should SKIP) if the 16 KiB kernel or the dev-enh disk is missing.
    pub fn seated_fedora_from_env() -> Result<GuestConfig> {
        let mut cfg = GuestConfig::enhanced_fedora_from_env()?;
        let disk = match std::env::var("LIMINA_TEST_DISK_ENH") {
            Ok(p) => PathBuf::from(p),
            Err(_) => repo_root().join("Fedora-Workstation-43.dev-enh.raw"),
        };
        anyhow::ensure!(
            disk.exists(),
            "seated dev-enh disk not found at {disk:?} (set LIMINA_TEST_DISK_ENH); this is the \
             machine-local seated golden, see memory `limina-fedora-access`"
        );
        if let Boot::KernelDisk { disk: d, .. } = &mut cfg.boot {
            *d = disk;
        }
        Ok(cfg)
    }

    /// Attach a user-mode NAT NIC. The supervisor spawns a gvproxy gateway and captures its
    /// `-debug` log; assert on it via [`Guest::wait_for_gateway_log`] (DHCP lease, outbound).
    pub fn with_net(mut self) -> GuestConfig {
        self.net = true;
        self
    }

    /// Attach a virtio-gpu display at `width`x`height` and capture presented frames. Use
    /// [`Guest::display_capture_path`]/[`Guest::wait_for_capture`] after boot to read the
    /// captured PNG. The L1 init draws a deterministic pattern to `/dev/fb0` and forces a
    /// flush, so the present is explicit rather than relying on fbcon's deferred I/O.
    pub fn with_display(mut self, width: u32, height: u32) -> GuestConfig {
        self.display = Some(DisplayCfg {
            width,
            height,
            software_2d: true,
        });
        self
    }

    /// Attach a virtio-gpu display running the default **coexist** device (software-2D 2D +
    /// venus 3D) — i.e. *without* `--gpu-software-2d`, so venus is available. Use when a test
    /// needs the GPU device present to probe 3D (e.g. `vulkaninfo` over SSH) rather than to
    /// assert on captured 2D pixels. See [`GuestConfig::enhanced_fedora_from_env`].
    pub fn with_coexist_display(mut self, width: u32, height: u32) -> GuestConfig {
        self.display = Some(DisplayCfg {
            width,
            height,
            software_2d: false,
        });
        self
    }

    /// Wire the **zink-on-KosmicKrisp host GL** worker environment so virglrenderer's `vrend`
    /// resolves its host GL to our Mesa build (the baseline-tier virgl path). Mirrors
    /// `spikes/virgl-zink-kk/boot-virgl-guest.sh`: point the dynamic loader and Mesa at the
    /// zink-on-KK prefix + our EGL-enabled epoxy, force the zink gallium driver, and pick the
    /// surfaceless EGL platform. These are HOST env vars on the worker (codesigned with
    /// `allow-dyld-environment-variables`, so `DYLD_*` survives); the worker propagates them from
    /// the supervisor it is spawned from. The KK Vulkan ICD itself is wired automatically by
    /// [`Guest::boot`] for any coexist display. No-op-safe: a test should still SKIP up front when
    /// [`zink_kk_mesa_prefix`] / [`kosmickrisp_icd`] are absent (else vrend can't bring up host GL).
    ///
    /// Production will instead bundle these dylibs in the `.app` via `@rpath` (no `DYLD_*`); this
    /// is the dev/test path. Pair with [`with_coexist_display`](GuestConfig::with_coexist_display).
    pub fn with_virgl_host_gl(mut self) -> GuestConfig {
        let prefix = zink_kk_mesa_prefix()
            .unwrap_or_else(|| PathBuf::from("/Volumes/mesa-cs/zink-kk-prefix"));
        let epoxy = repo_root().join("third_party/epoxy-egl-prefix");
        let dyld = format!(
            "{}/lib:{}/lib:/opt/homebrew/lib",
            prefix.display(),
            epoxy.display()
        );
        let drivers = format!("{}/lib", prefix.display());
        for (k, v) in [
            ("DYLD_FALLBACK_LIBRARY_PATH", dyld.as_str()),
            ("MESA_LOADER_DRIVER_OVERRIDE", "zink"),
            ("GALLIUM_DRIVER", "zink"),
            ("LIBGL_DRIVERS_PATH", drivers.as_str()),
            ("EGL_PLATFORM", "surfaceless"),
        ] {
            self.envs.push((k.to_string(), v.to_string()));
        }
        self
    }

    /// Append `extra` to the guest kernel cmdline (direct-kernel boots only). E.g.
    /// `systemd.unit=multi-user.target` to boot without a graphical session — useful when a
    /// test must manipulate the GPU device (driver unbind/rebind) without a compositor
    /// holding `card0`. No-op for firmware boots (the cmdline is GRUB-owned there).
    pub fn with_cmdline_extra(mut self, extra: &str) -> GuestConfig {
        match &mut self.boot {
            Boot::Kernel { cmdline, .. } | Boot::KernelDisk { cmdline, .. } => {
                cmdline.push(' ');
                cmdline.push_str(extra);
            }
            Boot::Firmware { .. } => {}
        }
        self
    }

    /// Enable the guest vsock agent on `port`: the host listens on a UNIX socket and the
    /// kernel cmdline gets `limina.agent_port=<port>` so the init runs the agent. Kernel
    /// boot only (the L1 guest). The HARNESS drives the host side of the protocol
    /// (`agent_accept`) — limina passes the vsock plumbing through and stays out of it.
    pub fn with_vsock(mut self, port: u32) -> GuestConfig {
        let socket_path = std::env::temp_dir().join(format!(
            "limina-vsock-{}-{}.sock",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        if let Boot::Kernel { cmdline, .. } = &mut self.boot {
            cmdline.push_str(&format!(" limina.agent_port={port}"));
        }
        self.vsock = Some(VsockCfg { port, socket_path });
        self
    }

    /// Run the guest agent against the **supervisor-owned** control plane (the product
    /// path): append the `limina.agent_port=` cmdline token at the well-known
    /// [`limina_proto::CONTROL_PORT`] WITHOUT binding a harness-side socket — limina itself
    /// binds the control socket and serves HELLO/WELCOME/SHUTDOWN. Direct-kernel boots only.
    pub fn with_control_agent(mut self) -> GuestConfig {
        match &mut self.boot {
            Boot::Kernel { cmdline, .. } | Boot::KernelDisk { cmdline, .. } => {
                cmdline.push_str(&format!(
                    " limina.agent_port={}",
                    limina_proto::CONTROL_PORT
                ));
            }
            Boot::Firmware { .. } => {}
        }
        self
    }

    /// Capture the supervisor's stderr (its log) into the scratch dir for assertions.
    pub fn with_supervisor_log(mut self) -> GuestConfig {
        self.supervisor_log = true;
        self
    }

    /// Pin the supervisor-owned control socket to a known scratch path so the harness can
    /// connect to the plane as a peer itself (playing e.g. a clipboard-capable agent —
    /// protocol-identical to a guest connecting through the vsock bridge). Pair with
    /// [`Guest::connect_control`].
    pub fn with_control_socket(mut self) -> GuestConfig {
        self.control_socket = true;
        self
    }

    /// Set an environment variable on the spawned supervisor (e.g. `LIMINA_PASTEBOARD` to
    /// point the clipboard bridge at a private named pasteboard).
    pub fn with_env(mut self, key: &str, value: &str) -> GuestConfig {
        self.envs.push((key.to_string(), value.to_string()));
        self
    }

    /// Share a host directory into the guest (`--share name=path`): virtiofs tag
    /// `limina-<name>`, auto-mounted at `/media/<name>` by the guest init/agent.
    pub fn with_share(mut self, name: &str, host_dir: &Path) -> GuestConfig {
        self.shares
            .push((name.to_string(), host_dir.to_path_buf(), false));
        self
    }

    /// Like [`GuestConfig::with_share`], but read-only (`--share name=path:ro`).
    pub fn with_share_ro(mut self, name: &str, host_dir: &Path) -> GuestConfig {
        self.shares
            .push((name.to_string(), host_dir.to_path_buf(), true));
        self
    }

    /// Append a whitespace-separated token to the kernel cmdline (direct-kernel boots
    /// only) — e.g. a `limina.*` mode flag the guest init understands.
    pub fn with_cmdline_token(mut self, token: &str) -> GuestConfig {
        match &mut self.boot {
            Boot::Kernel { cmdline, .. } | Boot::KernelDisk { cmdline, .. } => {
                cmdline.push(' ');
                cmdline.push_str(token);
            }
            Boot::Firmware { .. } => {}
        }
        self
    }

    /// Enable an interactive console: the harness feeds the guest console input via a FIFO
    /// (see [`Guest::console_send`]) on top of capturing output. Pair with a guest that
    /// reads the console — e.g. the L1 init's echo mode (`limina.console_echo`).
    ///
    /// This routes the console over **virtio-console (`hvc0`)**, the robust queue-based data
    /// console. (For the PL011 serial tty instead, use [`with_serial_input`].) To make hvc0
    /// the guest's `/dev/console` (so both kernel log and the init's I/O flow through it) we
    /// swap `console=ttyAMA0` → `console=hvc0` on the kernel cmdline.
    pub fn with_console_input(mut self) -> GuestConfig {
        self.console_input = true;
        self.console_channel = ConsoleChannel::Virtio;
        if let Boot::Kernel { cmdline, .. } = &mut self.boot {
            *cmdline = cmdline.replace("console=ttyAMA0", "console=hvc0");
        }
        self
    }

    /// Enable an interactive console over the **PL011 serial tty** (`ttyAMA0`) — the
    /// firmware/early-boot console, now also a working interactive serial shell after the
    /// halfword-MMIO fix (M2.5 Track A). Unlike [`with_console_input`] this keeps
    /// `console=ttyAMA0`, so the guest's `/dev/console` is the PL011 and both kernel log
    /// and the init's I/O flow through it. The harness feeds input via a FIFO on the same
    /// `--console`/`--console-input` device path (see [`Guest::console_send`]).
    pub fn with_serial_input(mut self) -> GuestConfig {
        self.console_input = true;
        self.console_channel = ConsoleChannel::Serial;
        self
    }

    /// Append `extra` (space-prefixed) to the kernel command line. Kernel boot only.
    pub fn append_cmdline(mut self, extra: &str) -> GuestConfig {
        if let Boot::Kernel { cmdline, .. } = &mut self.boot {
            cmdline.push(' ');
            cmdline.push_str(extra);
        }
        self
    }
}

/// Supervisor power-off grace (seconds), from `LIMINA_TEST_SHUTDOWN_GRACE` or 3.
fn grace_from_env() -> u64 {
    std::env::var("LIMINA_TEST_SHUTDOWN_GRACE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3)
}

/// Repo root, derived from this crate's compile-time manifest dir
/// (`crates/limina-test` -> repo root).
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Monotonic-ish unique suffix for scratch files (avoids `Math.random`/time deps).
static SEQ: AtomicU64 = AtomicU64::new(0);

/// A running guest under supervision. Drop tears it down so a panicking assertion can
/// never leak a live VM.
pub struct Guest {
    /// The supervisor child process.
    child: Child,
    /// Supervisor pid (for explicit signalling).
    pid: libc::pid_t,
    /// Where the guest serial console is captured.
    console_path: PathBuf,
    /// Scratch dir holding the console capture; removed on drop.
    scratch: PathBuf,
    /// Worker binary path, used by the orphan safety-net.
    vmm_bin: PathBuf,
    /// Host vsock listener (bound before boot so the guest agent can connect).
    vsock_listener: Option<UnixListener>,
    /// vsock socket path, removed on drop.
    vsock_socket: Option<PathBuf>,
    /// Captured-scanout PNG path (inside `scratch`), if a display was configured.
    capture_png: Option<PathBuf>,
    /// Runtime display-resize control socket (inside `scratch`), bound by the worker when a
    /// display is attached. See [`Guest::resize_display`].
    resize_socket: Option<PathBuf>,
    /// Write handle to the guest console input FIFO, if an interactive console was enabled.
    console_in: Option<fs::File>,
    /// Path to the gvproxy gateway's `-debug` log (inside `scratch`), if net was enabled.
    gateway_log: Option<PathBuf>,
    /// Path to the captured supervisor stderr (inside `scratch`), if enabled.
    supervisor_log: Option<PathBuf>,
    /// Path to the pinned supervisor-owned control socket (inside `scratch`), if enabled.
    control_socket: Option<PathBuf>,
    /// Set once teardown has run, so Drop doesn't double-kill.
    torn_down: bool,
}

/// A decoded captured scanout frame (8-bit RGBA, row-major).
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, RGBA8.
    pub rgba: Vec<u8>,
}

impl CapturedFrame {
    /// The RGBA pixel at `(x, y)` (zero if out of bounds).
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        if x >= self.width || y >= self.height {
            return [0, 0, 0, 0];
        }
        let i = ((y * self.width + x) * 4) as usize;
        [
            self.rgba[i],
            self.rgba[i + 1],
            self.rgba[i + 2],
            self.rgba[i + 3],
        ]
    }

    /// Count of distinct RGBA pixel values — a cheap "is there content?" probe. A blank
    /// (uniform) frame yields 1; rendered text/graphics yields many.
    pub fn distinct_colors(&self) -> usize {
        let mut set = std::collections::HashSet::new();
        for px in self.rgba.chunks_exact(4) {
            set.insert([px[0], px[1], px[2], px[3]]);
        }
        set.len()
    }

    /// The most common RGBA pixel (the presumed background) and its share of all pixels.
    pub fn dominant_color(&self) -> ([u8; 4], f64) {
        let mut counts = std::collections::HashMap::new();
        let total = (self.rgba.len() / 4).max(1);
        for px in self.rgba.chunks_exact(4) {
            *counts.entry([px[0], px[1], px[2], px[3]]).or_insert(0u64) += 1;
        }
        counts
            .into_iter()
            .max_by_key(|&(_, n)| n)
            .map(|(c, n)| (c, n as f64 / total as f64))
            .unwrap_or(([0, 0, 0, 0], 1.0))
    }
}

/// How the supervisor (and thus the VM) ended.
#[derive(Debug, Clone, Copy)]
pub struct Outcome {
    /// Exit code the supervisor returned, if it exited normally.
    pub code: Option<i32>,
    /// Signal that killed the supervisor, if any.
    pub signal: Option<i32>,
    /// True if the harness had to force the supervisor down (it didn't exit in time).
    pub forced: bool,
}

impl Guest {
    /// Boot a guest: launch the `limina` supervisor with the given config and capture its
    /// serial console to a scratch file. Returns once the process is spawned (boot
    /// proceeds asynchronously — use [`Guest::wait_for`] to await a marker).
    pub fn boot(cfg: &GuestConfig) -> Result<Guest> {
        let scratch = std::env::temp_dir().join(format!(
            "limina-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&scratch)
            .with_context(|| format!("creating scratch dir {scratch:?}"))?;
        let console_path = scratch.join("console.log");

        let mut cmd = Command::new(&cfg.limina_bin);
        cmd.arg("--cpus")
            .arg(cfg.cpus.to_string())
            .arg("--ram-mib")
            .arg(cfg.ram_mib.to_string())
            .arg("--shutdown-grace-secs")
            .arg(cfg.shutdown_grace.as_secs().to_string())
            // Point the supervisor at our built worker explicitly, so the harness and
            // supervisor never disagree about which binary is under test.
            .arg("--vmm-bin")
            .arg(&cfg.vmm_bin);
        match &cfg.boot {
            Boot::Firmware {
                firmware,
                disk,
                read_only,
            } => {
                // Networking needs the guest to reach userspace (NetworkManager does DHCP),
                // but a read-only root never gets there — Fedora can't mount rw and stalls
                // before NM. So for net tests, boot a *writable* APFS COW clone (`cp -c`:
                // instant, space-shared) inside the scratch dir, removed with it on Drop.
                let (disk_arg, read_only) = if cfg.net {
                    let clone = scratch.join("disk.raw");
                    cow_clone(disk, &clone)
                        .with_context(|| format!("cow-cloning {disk:?} for a writable net boot"))?;
                    (clone, false)
                } else {
                    (disk.clone(), *read_only)
                };
                cmd.arg("--firmware")
                    .arg(firmware)
                    .arg("--disk")
                    .arg(&disk_arg);
                if read_only {
                    cmd.arg("--read-only");
                }
            }
            Boot::Kernel {
                kernel,
                rootfs,
                cmdline,
            } => {
                cmd.arg("--kernel")
                    .arg(kernel)
                    .arg("--rootfs")
                    .arg(rootfs)
                    .arg("--cmdline")
                    .arg(cmdline);
            }
            Boot::KernelDisk {
                kernel,
                disk,
                cmdline,
            } => {
                // The guest mounts this disk rw as its root (and NetworkManager needs to write),
                // so boot a writable APFS COW clone — never mutate the shared image. (Same
                // reasoning as the Firmware+net path above.)
                let clone = scratch.join("disk.raw");
                cow_clone(disk, &clone)
                    .with_context(|| format!("cow-cloning {disk:?} for an enhanced-tier boot"))?;
                cmd.arg("--kernel")
                    .arg(kernel)
                    .arg("--cmdline")
                    .arg(cmdline)
                    .arg("--disk")
                    .arg(&clone);
            }
        }
        for (name, dir, read_only) in &cfg.shares {
            let dir_str = dir
                .to_str()
                .with_context(|| format!("share dir is not valid UTF-8: {dir:?}"))?;
            let ro = if *read_only { ":ro" } else { "" };
            cmd.arg("--share").arg(format!("{name}={dir_str}{ro}"));
        }
        // Console capture. Two channels, by use case:
        //  - Interactive (`with_console_input`): route over virtio-console (`hvc0`). The
        //    guest exposes hvc0 as a real bidirectional tty out of the box; with
        //    `console=hvc0` (set by `with_console_input`) it's the guest's `/dev/console`,
        //    so kernel log AND the init's I/O both land in `console_path`. A FIFO feeds
        //    input — opened O_RDWR here (never blocks, never EOFs) before spawn, so a test
        //    can `console_send` the instant the guest starts reading.
        //  - Output-only: the PL011 serial captured to `console_path` (the default path).
        let console_in = if cfg.console_input {
            let fifo = scratch.join("console.in");
            mkfifo(&fifo)?;
            let (out_flag, in_flag) = match cfg.console_channel {
                // PL011 serial tty (ttyAMA0): the same --console device, now bidirectional.
                ConsoleChannel::Serial => ("--console", "--console-input"),
                // virtio-console (hvc0): the robust bidirectional data console.
                ConsoleChannel::Virtio => ("--virtio-console", "--virtio-console-input"),
            };
            cmd.arg(out_flag).arg(&console_path).arg(in_flag).arg(&fifo);
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&fifo)
                .with_context(|| format!("opening console input fifo {fifo:?}"))?;
            Some(file)
        } else {
            cmd.arg("--console").arg(&console_path);
            None
        };

        // Display: capture the scanout into the scratch dir (auto-cleaned on Drop). Whenever a
        // display is attached we also wire a runtime resize control socket so tests can drive
        // window-resize via [`Guest::resize_display`] (the worker binds it; we connect per call).
        let resize_socket = cfg.display.as_ref().map(|_| scratch.join("resize.sock"));
        let capture_png = match &cfg.display {
            Some(d) => {
                let png = scratch.join("scanout.png");
                cmd.arg("--display-capture")
                    .arg(&png)
                    .arg("--display-size")
                    .arg(format!("{}x{}", d.width, d.height))
                    .arg("--display-control-socket")
                    .arg(
                        resize_socket
                            .as_ref()
                            .expect("resize_socket set with a display"),
                    );
                // The 2D capture oracle forces the software-2D GPU so it's deterministic and
                // independent of venus/Metal (the worker default is the coexist device). A
                // coexist/3D test (`with_coexist_display`) leaves venus on.
                if d.software_2d {
                    cmd.arg("--gpu-software-2d");
                } else {
                    // Coexist/venus: KosmicKrisp is our ONLY supported host Vulkan backend.
                    // Point the worker at it; if KK isn't built, degrade to software-2D
                    // (llvmpipe) rather than fall through to the loader's MoltenVK default,
                    // whose venus path crashes the guest compositor (#28/#32 class). Respect an
                    // explicit caller-set VK_ICD_FILENAMES (e.g. a one-off A/B).
                    let explicit_icd = cfg.envs.iter().any(|(k, _)| k == "VK_ICD_FILENAMES");
                    if !explicit_icd {
                        match kosmickrisp_icd() {
                            Some(icd) => {
                                cmd.env("VK_ICD_FILENAMES", &icd);
                            }
                            None => {
                                eprintln!(
                                    "limina-test: no KosmicKrisp ICD under /Volumes/mesa-cs/build-kk \
                                     — venus unavailable, degrading to software-2D (MoltenVK is \
                                     not a supported backend)"
                                );
                                cmd.arg("--gpu-software-2d");
                            }
                        }
                    }
                }
                Some(png)
            }
            None => None,
        };

        // vsock: bind the host listener BEFORE spawning, so the guest agent's connect
        // (which libkrun bridges to this socket) can't race ahead of us.
        let (vsock_listener, vsock_socket) = match &cfg.vsock {
            Some(v) => {
                let _ = fs::remove_file(&v.socket_path);
                let listener = UnixListener::bind(&v.socket_path)
                    .with_context(|| format!("binding vsock socket {:?}", v.socket_path))?;
                cmd.arg("--vsock-port")
                    .arg(v.port.to_string())
                    .arg("--vsock-socket")
                    .arg(&v.socket_path);
                (Some(listener), Some(v.socket_path.clone()))
            }
            None => (None, None),
        };

        // Networking: ask the supervisor to bring up the gvproxy NAT gateway and capture its
        // -debug packet log into the scratch dir (the host-side network oracle — stock Fedora
        // is silent on serial after GRUB, so the guest console can't witness DHCP/DNS).
        let gateway_log = if cfg.net {
            let log = scratch.join("gvproxy.log");
            cmd.arg("--net").arg("--net-log").arg(&log);
            Some(log)
        } else {
            None
        };

        // Control socket at a known path, so the test can join the supervisor-owned
        // plane as a peer (see connect_control).
        let control_socket = if cfg.control_socket {
            let sock = scratch.join("control.sock");
            cmd.arg("--control-socket").arg(&sock);
            Some(sock)
        } else {
            None
        };

        for (k, v) in &cfg.envs {
            cmd.env(k, v);
        }

        // Supervisor/worker logs: flow to the test's stderr by default (visible with
        // --nocapture); captured to a scratch file when the test asserts on them.
        cmd.stdin(Stdio::null());
        let supervisor_log = if cfg.supervisor_log {
            let path = scratch.join("supervisor.log");
            let file = fs::File::create(&path)
                .with_context(|| format!("creating supervisor log {path:?}"))?;
            cmd.stderr(Stdio::from(file));
            Some(path)
        } else {
            None
        };

        let child = cmd
            .spawn()
            .with_context(|| format!("spawning supervisor {:?}", cfg.limina_bin))?;
        let pid = child.id() as libc::pid_t;

        Ok(Guest {
            child,
            pid,
            console_path,
            scratch,
            vmm_bin: cfg.vmm_bin.clone(),
            vsock_listener,
            vsock_socket,
            capture_png,
            resize_socket,
            console_in,
            gateway_log,
            supervisor_log,
            control_socket,
            torn_down: false,
        })
    }

    /// Connect to the supervisor-owned control socket as a peer (requires
    /// [`GuestConfig::with_control_socket`]), retrying until the supervisor binds it.
    /// The returned [`AgentConn`] speaks raw limina-proto — the caller does its own
    /// HELLO/WELCOME handshake, exactly like a guest agent would.
    pub fn connect_control(&mut self, timeout: Duration) -> Result<AgentConn> {
        let sock = self
            .control_socket
            .clone()
            .context("no control socket (use GuestConfig::with_control_socket)")?;
        let deadline = Instant::now() + timeout;
        loop {
            match UnixStream::connect(&sock) {
                Ok(stream) => return Ok(AgentConn { stream }),
                Err(_) if Instant::now() < deadline => {
                    if let Some(status) = self.child.try_wait().context("polling supervisor")? {
                        bail!("supervisor exited ({status:?}) before binding {sock:?}");
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("connecting to control socket {sock:?}"));
                }
            }
        }
    }

    /// Path to the captured-scanout PNG (inside the scratch dir), if a display was
    /// configured. The file appears once the guest presents its first frame and is
    /// overwritten with each subsequent frame (latest wins). Removed on Drop.
    pub fn display_capture_path(&self) -> Option<&Path> {
        self.capture_png.as_deref()
    }

    /// Request a runtime display resize: connect to the worker's display-control socket and
    /// send `resize <width> <height>`. The worker applies it to the live virtio-gpu (raising a
    /// config-change), so the guest re-modesets to the new resolution. Retries the connect for
    /// a few seconds because the worker binds the socket partway through boot. Requires a
    /// display in the [`GuestConfig`]. See `docs/design/runtime-display-resize.md`.
    pub fn resize_display(&self, width: u32, height: u32) -> Result<()> {
        use std::io::Write;
        let path = self
            .resize_socket
            .as_ref()
            .context("resize_display requires a display in the GuestConfig")?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match UnixStream::connect(path) {
                Ok(s) => break s,
                Err(e) if Instant::now() < deadline => {
                    // The worker hasn't bound the socket yet (early boot); back off and retry.
                    std::thread::sleep(Duration::from_millis(100));
                    let _ = e;
                }
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("connecting to the display-control socket {path:?}")
                    })
                }
            }
        };
        writeln!(stream, "resize {width} {height}")
            .with_context(|| format!("sending resize to {path:?}"))?;
        stream.flush().ok();
        Ok(())
    }

    /// Decode the current captured scanout PNG. Errors if no display was configured or no
    /// frame has been captured yet. The worker writes the PNG atomically (temp + rename),
    /// so any file that exists is complete.
    pub fn read_capture(&self) -> Result<CapturedFrame> {
        let path = self
            .capture_png
            .as_ref()
            .context("no display configured (use GuestConfig::with_display)")?;
        let file = fs::File::open(path).with_context(|| format!("opening capture {path:?}"))?;
        let mut reader = png::Decoder::new(BufReader::new(file))
            .read_info()
            .context("reading PNG header")?;
        let mut rgba = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut rgba).context("decoding PNG frame")?;
        rgba.truncate(info.buffer_size());
        anyhow::ensure!(
            info.color_type == png::ColorType::Rgba && info.bit_depth == png::BitDepth::Eight,
            "unexpected capture format {:?}/{:?} (expected RGBA8)",
            info.color_type,
            info.bit_depth
        );
        Ok(CapturedFrame {
            width: info.width,
            height: info.height,
            rgba,
        })
    }

    /// Block until at least one scanout frame has been captured (and decode it), or
    /// `timeout` elapses, or the supervisor exits first. The returned frame is the latest
    /// one present when the wait succeeds.
    pub fn wait_for_capture(&mut self, timeout: Duration) -> Result<CapturedFrame> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.capture_png.as_ref().is_some_and(|p| p.exists()) {
                if let Ok(frame) = self.read_capture() {
                    return Ok(frame);
                }
            }
            if let Some(status) = self.child.try_wait().context("polling supervisor")? {
                // The supervisor exited; a final frame may have landed just before it did.
                if let Ok(frame) = self.read_capture() {
                    return Ok(frame);
                }
                bail!("supervisor exited ({status}) before any frame was captured");
            }
            if Instant::now() >= deadline {
                bail!("timed out after {timeout:?} waiting for a captured frame");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Accept the guest agent's vsock connection (the guest connects shortly after boot),
    /// returning a typed control-plane channel (limina-proto frames). Errors if no vsock was
    /// configured, the guest never connects within `timeout`, or the supervisor exits first.
    pub fn agent_accept(&mut self, timeout: Duration) -> Result<AgentConn> {
        let listener = self
            .vsock_listener
            .as_ref()
            .context("no vsock configured (use GuestConfig::with_vsock)")?;
        listener
            .set_nonblocking(true)
            .context("set_nonblocking on vsock listener")?;

        let deadline = Instant::now() + timeout;
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).ok();
                    return Ok(AgentConn { stream });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if let Some(status) = self.child.try_wait().context("polling supervisor")? {
                        bail!("supervisor exited ({status}) before the guest agent connected");
                    }
                    if Instant::now() >= deadline {
                        bail!("timed out after {timeout:?} waiting for the guest agent");
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(e).context("accepting guest agent connection"),
            }
        }
    }

    /// Current captured console text (lossy UTF-8; empty if nothing yet).
    pub fn console(&self) -> String {
        fs::read(&self.console_path)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    }

    /// Block until `needle` appears in the console, or `timeout` elapses, or the
    /// supervisor exits early (all three are errors except the match).
    pub fn wait_for(&mut self, needle: &str, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let console = self.console();
            if console.contains(needle) {
                return Ok(());
            }
            if let Some(status) = self.child.try_wait().context("polling supervisor")? {
                bail!(
                    "supervisor exited ({status}) before console showed {needle:?}.\n\
                     --- console tail ---\n{}",
                    tail(&console, 40)
                );
            }
            if Instant::now() >= deadline {
                bail!(
                    "timed out after {timeout:?} waiting for {needle:?}.\n\
                     --- console tail ---\n{}",
                    tail(&console, 40)
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Current gvproxy gateway `-debug` log text (lossy UTF-8; empty if no net / nothing yet).
    pub fn gateway_log(&self) -> String {
        self.gateway_log
            .as_ref()
            .and_then(|p| fs::read(p).ok())
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    }

    /// Current captured supervisor log text (lossy UTF-8; empty if not captured / nothing
    /// yet). Requires [`GuestConfig::with_supervisor_log`].
    pub fn supervisor_log(&self) -> String {
        self.supervisor_log
            .as_ref()
            .and_then(|p| fs::read(p).ok())
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    }

    /// Block until `needle` appears in the supervisor's log, or `timeout` elapses, or the
    /// supervisor exits early. Requires [`GuestConfig::with_supervisor_log`].
    pub fn wait_for_supervisor_log(&mut self, needle: &str, timeout: Duration) -> Result<()> {
        anyhow::ensure!(
            self.supervisor_log.is_some(),
            "no supervisor log (use GuestConfig::with_supervisor_log)"
        );
        let deadline = Instant::now() + timeout;
        loop {
            let log = self.supervisor_log();
            if log.contains(needle) {
                return Ok(());
            }
            if let Some(status) = self.child.try_wait().context("polling supervisor")? {
                bail!(
                    "supervisor exited ({status}) before its log showed {needle:?}.\n\
                     --- supervisor log tail ---\n{}",
                    tail(&log, 20)
                );
            }
            if Instant::now() >= deadline {
                bail!(
                    "timed out after {timeout:?} waiting for {needle:?} in the supervisor log.\n\
                     --- supervisor log tail ---\n{}",
                    tail(&log, 20)
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Block until `needle` appears in the gvproxy gateway log, or `timeout` elapses, or the
    /// supervisor exits early. The gateway log is the host-side network oracle (DHCP, DNS,
    /// NAT packets). Requires [`GuestConfig::with_net`].
    pub fn wait_for_gateway_log(&mut self, needle: &str, timeout: Duration) -> Result<()> {
        anyhow::ensure!(
            self.gateway_log.is_some(),
            "no gateway log (use GuestConfig::with_net)"
        );
        let deadline = Instant::now() + timeout;
        loop {
            let log = self.gateway_log();
            if log.contains(needle) {
                return Ok(());
            }
            if let Some(status) = self.child.try_wait().context("polling supervisor")? {
                bail!(
                    "supervisor exited ({status}) before the gateway log showed {needle:?}.\n\
                     --- gateway log tail ---\n{}",
                    tail(&log, 20)
                );
            }
            if Instant::now() >= deadline {
                bail!(
                    "timed out after {timeout:?} waiting for {needle:?} in the gateway log.\n\
                     --- gateway log tail ---\n{}",
                    tail(&log, 20)
                );
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Block until the guest's SSH server answers through gvproxy's inbound port-forward
    /// (`127.0.0.1:2222 → guest:22`), returning its banner (e.g. `SSH-2.0-OpenSSH_10.0`).
    /// Proves the inbound NAT path end-to-end (host → gvproxy forward → guest sshd) — what
    /// makes `ssh -p 2222 user@127.0.0.1` work. Requires [`GuestConfig::with_net`] and a guest
    /// running sshd. gvproxy listens on 2222 immediately but only yields a banner once it can
    /// dial the guest, so an empty/short read just means "not ready yet" — keep polling.
    pub fn wait_for_ssh_banner(&mut self, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        let addr: std::net::SocketAddr = FORWARDED_SSH_ADDR.parse().expect("valid forward addr");
        loop {
            if let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
                stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
                let mut buf = [0u8; 128];
                if let Ok(n) = stream.read(&mut buf) {
                    let banner = String::from_utf8_lossy(&buf[..n]).trim().to_string();
                    if banner.starts_with("SSH-") {
                        return Ok(banner);
                    }
                }
            }
            if let Some(status) = self.child.try_wait().context("polling supervisor")? {
                bail!("supervisor exited ({status}) before SSH was reachable");
            }
            if Instant::now() >= deadline {
                bail!("no SSH banner from {FORWARDED_SSH_ADDR} within {timeout:?}");
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Run `remote_cmd` in the guest over SSH (through gvproxy's `127.0.0.1:2222 → guest:22`
    /// forward) and return its stdout. Logs in as the in-image `claude` user via the host's
    /// default key (passwordless — see memory `limina-fedora-access`); host-key checks are
    /// disabled (the forward reuses 127.0.0.1:2222 across boots). Requires [`GuestConfig::with_net`]
    /// and a booted guest running sshd — call [`Guest::wait_for_ssh_banner`] first. Errors if
    /// ssh exits non-zero (stderr is included in the message).
    pub fn ssh_exec(&self, remote_cmd: &str) -> Result<String> {
        let out = Command::new("ssh")
            .args([
                "-p",
                "2222",
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "LogLevel=ERROR",
                "claude@127.0.0.1",
                remote_cmd,
            ])
            .output()
            .context("spawning ssh to the guest (127.0.0.1:2222)")?;
        if !out.status.success() {
            bail!(
                "ssh `{remote_cmd}` failed ({}):\n{}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Keep running `remote_cmd` over SSH until it succeeds (exit 0) or `timeout` elapses;
    /// returns the first successful stdout. For waiting on guest-side state that comes up
    /// asynchronously after sshd (e.g. the autologin graphical session).
    pub fn ssh_poll(&self, remote_cmd: &str, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let last_err = match self.ssh_exec(remote_cmd) {
                Ok(out) => return Ok(out),
                Err(e) => e,
            };
            if Instant::now() >= deadline {
                bail!("`{remote_cmd}` did not succeed within {timeout:?}; last: {last_err}");
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    /// Copy a local file into the guest at `remote_path` via scp (same forward and login
    /// as [`Guest::ssh_exec`]).
    pub fn scp_to_guest(&self, local: &Path, remote_path: &str) -> Result<()> {
        let out = Command::new("scp")
            .args(SCP_OPTS)
            .arg(local)
            .arg(format!("claude@127.0.0.1:{remote_path}"))
            .output()
            .context("spawning scp to the guest")?;
        anyhow::ensure!(
            out.status.success(),
            "scp {local:?} -> guest:{remote_path} failed ({}):\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
        Ok(())
    }

    /// Copy guest files matching `remote_glob` (expanded by the guest shell) into the local
    /// directory `local_dir` via scp.
    pub fn scp_from_guest(&self, remote_glob: &str, local_dir: &Path) -> Result<()> {
        fs::create_dir_all(local_dir)
            .with_context(|| format!("creating {local_dir:?} for scp output"))?;
        let out = Command::new("scp")
            .args(SCP_OPTS)
            .arg(format!("claude@127.0.0.1:{remote_glob}"))
            .arg(local_dir)
            .output()
            .context("spawning scp from the guest")?;
        anyhow::ensure!(
            out.status.success(),
            "scp guest:{remote_glob} -> {local_dir:?} failed ({}):\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
        Ok(())
    }

    /// The per-guest scratch directory (removed on drop) — a place for test-local
    /// artifacts like pulled snapshot frames.
    pub fn scratch_dir(&self) -> &Path {
        &self.scratch
    }

    /// Feed `line` (a newline is appended) to the guest serial console input. Requires
    /// [`GuestConfig::with_console_input`]. Lets a test "type" at the guest — e.g. a command
    /// for the L1 init's echo mode (or, later, a real shell) to read and respond to.
    pub fn console_send(&mut self, line: &str) -> Result<()> {
        let f = self
            .console_in
            .as_mut()
            .context("console input not enabled (use GuestConfig::with_console_input)")?;
        f.write_all(line.as_bytes())
            .and_then(|()| f.write_all(b"\n"))
            .and_then(|()| f.flush())
            .context("writing to console input fifo")?;
        Ok(())
    }

    /// Send `cmd` to the guest's console **command mode** (the L1 init's `limina.console_shell`)
    /// and return that command's output — everything the guest wrote between this command and its
    /// `LIMINA_SHELL_DONE` frame terminator. Requires an interactive console
    /// ([`GuestConfig::with_serial_input`]/[`with_console_input`]) and a guest in shell mode
    /// (await `LIMINA_SHELL_READY` first). This is the "type a command, assert its output"
    /// primitive that closes M2.5 Track A; tests typically `.contains()` on the result.
    ///
    /// Robust against interleaved kernel log: we slice between the previous frame boundary
    /// (the ready marker or the prior `DONE` line) and *this* command's `DONE`, keyed off the
    /// running count of terminators rather than fragile offsets.
    pub fn console_command(&mut self, cmd: &str, timeout: Duration) -> Result<String> {
        const DONE: &str = "LIMINA_SHELL_DONE";
        const READY: &str = "LIMINA_SHELL_READY";
        let done_before = self.console().matches(DONE).count();
        self.console_send(cmd)?;
        let deadline = Instant::now() + timeout;
        loop {
            let console = self.console();
            let dones: Vec<usize> = console.match_indices(DONE).map(|(i, _)| i).collect();
            if dones.len() > done_before {
                let this = dones[done_before];
                // Output starts after the previous frame boundary: the ready marker for the
                // first command, otherwise the end of the prior DONE line. Both boundaries are
                // ASCII, so these byte offsets are valid UTF-8 slice points.
                let start = if done_before == 0 {
                    console.find(READY).map(|i| i + READY.len()).unwrap_or(0)
                } else {
                    let prev = dones[done_before - 1];
                    console[prev..]
                        .find('\n')
                        .map(|nl| prev + nl + 1)
                        .unwrap_or(prev)
                };
                return Ok(console[start..this].trim().to_string());
            }
            if let Some(status) = self.child.try_wait().context("polling supervisor")? {
                bail!(
                    "supervisor exited ({status}) before console command {cmd:?} completed.\n\
                     --- console tail ---\n{}",
                    tail(&console, 40)
                );
            }
            if Instant::now() >= deadline {
                bail!(
                    "timed out after {timeout:?} waiting for {cmd:?} to complete (no {DONE}).\n\
                     --- console tail ---\n{}",
                    tail(&console, 40)
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Request a clean shutdown (SIGTERM to the supervisor, which asks the guest to
    /// power off and force-kills the worker after its own grace), then wait up to
    /// `timeout` for the supervisor to exit. Escalates to SIGKILL if it overruns.
    pub fn shutdown(mut self, timeout: Duration) -> Result<Outcome> {
        let outcome = self.terminate(timeout)?;
        Ok(outcome)
    }

    /// Internal teardown shared by [`Guest::shutdown`] and `Drop`.
    fn terminate(&mut self, timeout: Duration) -> Result<Outcome> {
        if self.torn_down {
            return Ok(Outcome {
                code: None,
                signal: None,
                forced: false,
            });
        }
        self.torn_down = true;

        // Already gone?
        if let Some(status) = self.child.try_wait().context("polling supervisor")? {
            return Ok(self.outcome_from(status, false));
        }

        // Ask nicely: SIGTERM the supervisor; it drives guest power-off then reaps the
        // worker. Reaping the supervisor guarantees the worker is reaped too.
        unsafe {
            libc::kill(self.pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().context("polling supervisor")? {
                return Ok(self.outcome_from(status, false));
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        // Overran: force the supervisor down and net any orphaned worker, so a wedged
        // supervisor can never leave a live VM holding HVF.
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
        }
        self.kill_stray_workers();
        let status = self
            .child
            .wait()
            .context("waiting on supervisor after SIGKILL")?;
        Ok(self.outcome_from(status, true))
    }

    fn outcome_from(&self, status: std::process::ExitStatus, forced: bool) -> Outcome {
        use std::os::unix::process::ExitStatusExt;
        Outcome {
            code: status.code(),
            signal: status.signal(),
            forced,
        }
    }

    /// Best-effort: SIGKILL any process whose argv mentions our worker binary path.
    /// Only reached on the forced path; the worker path is unique to this build, so this
    /// can't hit an unrelated VM.
    fn kill_stray_workers(&self) {
        if let Some(path) = self.vmm_bin.to_str() {
            let _ = Command::new("pkill")
                .args(["-9", "-f", path])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        // Don't let a panicking test leak a live VM. Give the graceful path a short,
        // bounded window, then the forced path nets everything.
        let grace = Duration::from_secs(8);
        let _ = self.terminate(grace);
        let _ = fs::remove_dir_all(&self.scratch);
        if let Some(sock) = &self.vsock_socket {
            let _ = fs::remove_file(sock);
        }
    }
}

/// A line-oriented connection to the guest vsock agent.
pub struct AgentConn {
    stream: UnixStream,
}

impl AgentConn {
    /// Receive one control-plane message, honoring `timeout`. Errors on timeout, EOF, or
    /// a malformed frame. Returns `(channel, message)`; unknown message types come back
    /// as [`limina_proto::Message::Unknown`], not as errors.
    pub fn recv(&mut self, timeout: Duration) -> Result<(u32, limina_proto::Message)> {
        self.stream
            .set_read_timeout(Some(timeout))
            .context("set_read_timeout")?;
        limina_proto::read_message(&mut self.stream)
            .context("reading control-plane message from guest agent (timeout?)")
    }

    /// Send one message to the agent on the control channel.
    pub fn send(&mut self, msg: &limina_proto::Message) -> Result<()> {
        limina_proto::write_message(&mut self.stream, limina_proto::CHANNEL_CONTROL, msg)
            .context("sending control-plane message to guest agent")
    }
}

/// Last `n` lines of `s`.
fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// APFS copy-on-write clone `src` → `dst` (`cp -c`): instant and space-shared, so a test
/// can boot a *writable* copy of the multi-GB image without mutating or duplicating it.
fn cow_clone(src: &Path, dst: &Path) -> Result<()> {
    let status = Command::new("cp")
        .arg("-c")
        .arg(src)
        .arg(dst)
        .status()
        .with_context(|| format!("running cp -c {src:?} {dst:?}"))?;
    anyhow::ensure!(status.success(), "cp -c {src:?} {dst:?} failed ({status})");
    Ok(())
}

/// Create a FIFO (named pipe) at `path`, mode 0600.
fn mkfifo(path: &Path) -> Result<()> {
    let c = std::ffi::CString::new(path.to_string_lossy().into_owned())
        .with_context(|| format!("path with NUL: {path:?}"))?;
    // SAFETY: `c` is a valid NUL-terminated C string for the lifetime of the call.
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("mkfifo {path:?}"));
    }
    Ok(())
}

/// Convenience: assert that a captured console contains all of `needles`, with a
/// helpful message naming the first missing one.
pub fn assert_console_has(console: &str, needles: &[&str]) -> Result<()> {
    for n in needles {
        if !console.contains(n) {
            return Err(anyhow!(
                "console missing expected marker {n:?}.\n--- tail ---\n{}",
                tail(console, 40)
            ));
        }
    }
    Ok(())
}

// --- named-pasteboard helpers (the M5 clipboard-bridge oracle) -----------------------
//
// Tests point the supervisor at a private NAMED pasteboard via the LIMINA_PASTEBOARD env
// (never the user's real clipboard) and use these to play the macOS side: reading what
// the bridge wrote, and writing what the bridge should offer to the guest.

/// Read the string content of the named pasteboard (None if empty/non-string).
pub fn pasteboard_text(name: &str) -> Option<String> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    use objc2_foundation::NSString;
    unsafe {
        let pb = NSPasteboard::pasteboardWithName(&NSString::from_str(name));
        pb.stringForType(NSPasteboardTypeString)
            .map(|s| s.to_string())
    }
}

/// Replace the named pasteboard's content with `text` (bumps its change count, exactly
/// like a macOS app copying).
pub fn set_pasteboard_text(name: &str, text: &str) {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    use objc2_foundation::NSString;
    unsafe {
        let pb = NSPasteboard::pasteboardWithName(&NSString::from_str(name));
        pb.clearContents();
        pb.setString_forType(&NSString::from_str(text), NSPasteboardTypeString);
    }
}
