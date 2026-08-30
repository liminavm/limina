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
// Re-exported so tests can build display commands without depending on the crate directly.
pub use limina_displayctl::{DisplayCommand, DisplayControl, EdidSpec, RangeSpec};

pub mod bench;
pub mod landmarks;

/// Last-resort fallback firmware: the krunkit-shipped blob (an EDK2 `.fd`), the same one the
/// M1 boot spikes used. It is a **DEBUG_GCC5 build with live ASSERTs that end in
/// `CpuDeadLoop`** — an ASSERT anywhere in it wedges the guest at 100% CPU with no output
/// (the #14 "cold-boot wedge" was exactly that), which is why it is no longer the default.
const KRUNKIT_FALLBACK_FIRMWARE: &str = "/opt/homebrew/share/krunkit/KRUN_EFI.silent.fd";

/// Stable location of OUR GOP-capable EDK2 firmware (built by `scripts/build-krun-efi.sh`,
/// carries VirtioGpuDxe). Overridable with `LIMINA_GOP_FIRMWARE`. Unlike the silent firmware,
/// firmware → GRUB → kernel all render to the virtio-gpu scanout — the windowed boot console.
const DEFAULT_GOP_FIRMWARE: &str = "target/krun-efi/KRUN_EFI.gop.fd";

/// The firmware every EFI-boot test uses: `LIMINA_FIRMWARE` if set, else OUR OWN RELEASE
/// build ([`DEFAULT_GOP_FIRMWARE`] — what limina actually ships; RELEASE degrades gracefully
/// where the DEBUG krunkit blob dead-loops), else the krunkit blob with a loud warning (a
/// fresh clone before the first `scripts/build-krun-efi.sh` run).
fn default_firmware() -> PathBuf {
    if let Ok(fw) = std::env::var("LIMINA_FIRMWARE") {
        return PathBuf::from(fw);
    }
    let ours = repo_root().join(DEFAULT_GOP_FIRMWARE);
    if ours.exists() {
        return ours;
    }
    eprintln!(
        "WARNING: {DEFAULT_GOP_FIRMWARE} not built — falling back to the krunkit DEBUG \
         firmware at {KRUNKIT_FALLBACK_FIRMWARE} (ASSERTs dead-loop silently there; run \
         scripts/build-krun-efi.sh for the real default)"
    );
    PathBuf::from(KRUNKIT_FALLBACK_FIRMWARE)
}

/// Default EFI-bootable aarch64 installer ISO for the M10 Phase 3a boot-from-media test
/// ([`GuestConfig::iso_boot_from_env`]). Gitignored & large (~1.1 GB) like the `.raw` images, so the
/// test SKIPs when it (or `LIMINA_TEST_ISO`) is absent. Fetch from dl.fedoraproject.org.
const DEFAULT_TEST_ISO: &str = "Fedora-Server-netinst-aarch64-43-1.6.iso";

/// The Fedora release the L2 image set targets — `LIMINA_FEDORA_REL` (default `"44"`). The image
/// set is mirrored per release (`vanilla`/`accessible`/`stock.test`/`enhanced`/`enhanced.test`), so
/// the suite runs against either F43 or F44 by flipping this one var. Per-image overrides
/// (`LIMINA_TEST_DISK`, `LIMINA_TEST_DISK_ENH`, …) still win when set.
///
/// **Default moved 43 → 44 on 2026-08-15.** F44 is the dogfood/dev family the guest components are
/// actually built for, and the F43 pair had drifted a release behind (r9-era bases never landed
/// there, task #31) — so the suite was quietly certifying stale guests. It cost a real
/// investigation: the vdagent clipboard test failed only because F43's 6.12 kernel has no
/// `uinput`, which `spice-vdagentd` treats as fatal. Tests that pin F44 explicitly
/// ([`GuestConfig::seated_efi_synoik_from_env`], [`bench::tier_config`](crate::bench::tier_config))
/// did so to escape this default; their pins are now redundant but harmless.
fn fedora_rel() -> String {
    std::env::var("LIMINA_FEDORA_REL").unwrap_or_else(|_| "44".to_string())
}

/// Resolve a release-specific guest image by ROLE → `Fedora-Workstation-<REL>.<role>.raw` in the
/// repo root. Key roles:
/// - `stock.test` — the **stock-tier** frozen CoW snapshot of `accessible` (stock kernel/mesa,
///   `claude` + SSH key + autologin, no enhancements). MUST stay stock: the EFI boot tests
///   (`fedora_from_env`) boot its own stock Fedora kernel to prove the compatibility floor, and the
///   venus tests (`enhanced_fedora_from_env`) boot it with an external 16 KiB kernel to prove stock
///   mesa's venus works on 16 KiB pages.
/// - `enhanced.test` — the venus golden ([`GuestConfig::seated_fedora_from_env`]).
///
/// Created once with `cp -c Fedora-Workstation-<REL>.accessible.raw Fedora-Workstation-<REL>.<role>.raw`
/// (APFS clone: instant, shares blocks); refresh by re-cloning. Per-run the harness still makes its
/// own writable clone for net/disk-root boots.
fn fedora_image(role: &str) -> PathBuf {
    repo_root().join(format!("Fedora-Workstation-{}.{role}.raw", fedora_rel()))
}

/// Loopback host gvproxy binds its inbound SSH forward on (`<host>:<ssh_port> → 192.168.127.2:22`;
/// with the well-known vfkit MAC the guest gets the static `.2` lease, so this reaches its sshd).
const FORWARDED_SSH_HOST: &str = "127.0.0.1";
/// gvproxy's default inbound SSH-forward host port; per-VM overridable via `--ssh-port` so several
/// VMs can run in parallel without colliding (see [`GuestConfig::with_ssh_port`]).
const DEFAULT_SSH_PORT: u16 = 2222;

/// Hard cap on any single guest ssh/scp command, so a wedged guest-side process fails the
/// test instead of stalling the whole suite (a zink lost-wakeup deadlock once froze
/// eglretrace mid-`venus_replay` and the un-capped ssh blocked `test-boot.sh` for 100
/// minutes — spikes/venus-replay-zink-hang-2026-07-12/). Generous on purpose: the longest
/// legitimate steps (the ~1 GiB trace-fixture upload through gvproxy, a full llvmpipe
/// reference replay) finish in a few minutes. Steps that truly need more pass their own
/// deadline via [`Guest::ssh_exec_timeout`].
pub const SSH_CMD_TIMEOUT: Duration = Duration::from_secs(900);

/// Run a spawned command to completion with a deadline: pipe and drain stdout/stderr on
/// threads (so a chatty child can't block on a full pipe), poll for exit, and on expiry
/// kill the child and fail with whatever output it produced. The `Command::output()`
/// shape, minus the ability to hang forever.
fn run_capped(mut cmd: Command, timeout: Duration) -> Result<std::process::Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("spawning the command")?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out_t = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let err_t = drain(child.stderr.take().map(|p| Box::new(p) as _));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait().context("waiting for the command")? {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    let stdout = out_t.join().unwrap_or_default();
    let stderr = err_t.join().unwrap_or_default();
    match status {
        Some(status) => Ok(std::process::Output {
            status,
            stdout,
            stderr,
        }),
        None => bail!(
            "command did not finish within {timeout:?} (killed).\nstdout tail:\n{}\nstderr tail:\n{}",
            tail(&String::from_utf8_lossy(&stdout), 10),
            tail(&String::from_utf8_lossy(&stderr), 10)
        ),
    }
}

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
/// The shipped `limina` front-end binary (honors `LIMINA_BIN`). For tests that drive the CLI
/// without booting anything — `limina check`, a start refused before the spawn — and so have
/// no reason to resolve a guest image or firmware.
pub fn limina_bin() -> Result<PathBuf> {
    resolve_bin("limina", "LIMINA_BIN")
}

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

/// Assert the worker binary still carries `com.apple.security.hypervisor`.
///
/// Any cargo invocation that relinks the worker (a concurrent build, an editor task, an
/// app-bundle build sharing `target/`) replaces the codesigned copy with a plain
/// linker-signed one — and every subsequent boot then dies with the thoroughly misleading
/// `build_microvm: Internal(Vm(VmSetup(VmCreate)))` (hv_vm_create → HV_DENIED). This burnt
/// a debugging session (the whole test-boot tail failed because a parallel
/// build unsigned the worker mid-suite). Checking up front turns that mystery into a
/// one-line diagnosis.
fn ensure_hypervisor_entitlement(vmm_bin: &Path) -> Result<()> {
    let out = Command::new("codesign")
        .args(["-d", "--entitlements", "-"])
        .arg(vmm_bin)
        .output()
        .context("running codesign to check the worker's entitlements")?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    anyhow::ensure!(
        text.contains("com.apple.security.hypervisor"),
        "worker {vmm_bin:?} is not signed with com.apple.security.hypervisor — a build \
         relinked it after signing (hv_vm_create would fail as VmSetup(VmCreate)). \
         Re-sign with crates/limina-vmm/sign.sh, and don't run cargo builds concurrently \
         with the boot tests."
    );
    Ok(())
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

/// An extra data disk attached *after* the boot disk (so it enumerates as `vdb`, `vdc`, …) —
/// M10 multiple-disk support. The harness never mutates a shared source image: a blank disk is
/// created in the scratch dir, and a writable existing image is cow-cloned there first.
#[derive(Debug, Clone)]
pub enum DataDisk {
    /// A blank sparse raw of `size_bytes`, created in the scratch dir and attached read-write.
    Blank { size_bytes: u64 },
    /// A blank qcow2 of `size_bytes` *virtual* size (created via `qemu-img` in the scratch dir,
    /// attached read-write). The physical file is tiny, so the guest only sees the full size if
    /// the worker opens it AS qcow2 — the discriminator for the format-detection test.
    BlankQcow2 { size_bytes: u64 },
    /// An existing image: cow-cloned into scratch when read-write, attached in place when `:ro`.
    Existing { path: PathBuf, read_only: bool },
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
    /// Open a real native window (`--window`) instead of capturing frames. Required by any
    /// test whose subject is the SUPERVISOR's frame-apply path: `--display-capture` never
    /// runs `window::run`, so a capture boot would exercise none of it and pass against a
    /// broken window. Mutually exclusive with capture in the CLI, so there is no PNG.
    pub windowed: bool,
    /// How many virtio-gpu scanouts to configure (`--display-pool`). 1 is the single-display
    /// device every test but the multi-display ones wants. Slots above 0 boot disconnected.
    pub pool: u32,
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
    /// Per-VM host port for gvproxy's inbound SSH forward (`--ssh-port`). `None` → the supervisor
    /// default ([`DEFAULT_SSH_PORT`], 2222). Give two concurrent VMs distinct ports so they don't
    /// collide on the host port — what lets more than one run in parallel. Set via
    /// [`with_ssh_port`](GuestConfig::with_ssh_port) (which also implies `net`).
    pub ssh_port: Option<u16>,
    /// Custom guest MAC for the NAT NIC (`--net-mac`). `None` → the well-known vfkit MAC.
    /// With a custom MAC the supervisor generates a gvproxy config rebinding the static .2
    /// lease, so this exercises the managed-VM (per-VM MAC) network path. Set via
    /// [`with_net_mac`](GuestConfig::with_net_mac) (which also implies `net`).
    pub net_mac: Option<String>,
    /// Capture the supervisor's own stderr (its log) to a scratch file instead of letting
    /// it flow to the test's stderr — for asserting on supervisor-side events (e.g. the
    /// control plane's "guest agent connected"). See [`Guest::wait_for_supervisor_log`].
    pub supervisor_log: bool,
    /// Pin the supervisor-owned control socket to a known scratch path (`--control-socket`)
    /// so the harness can join the plane as a peer itself. See [`Guest::connect_control`].
    pub control_socket: bool,
    /// Bind a runtime balloon control socket (`--balloon-control-socket`, M6 dynamic memory) at a
    /// known scratch path so the harness can drive the target / read `stats`. See
    /// [`Guest::set_balloon_target`] / [`Guest::balloon_stats`].
    pub balloon_control: bool,
    /// Dynamic-memory range `(min_mib, max_mib)` passed as `--memory MIN..MAX` (M6). `MAX` becomes
    /// the guest RAM; setting this starts the supervisor's PSI autoballoon policy. See
    /// [`with_memory`](GuestConfig::with_memory).
    pub memory: Option<(usize, usize)>,
    /// `--reclaim` mode override for the autoballoon policy (the supervisor defaults to
    /// moderate); the bench sweeps this axis.
    pub reclaim: Option<String>,
    /// Extra environment variables for the supervisor process (e.g. `LIMINA_PASTEBOARD`).
    pub envs: Vec<(String, String)>,
    /// Host directories shared into the guest (`--share name=path[:ro]` → virtiofs tag
    /// `limina-<name>`, auto-mounted at `/media/<name>` by the guest init/agent). The
    /// bool is read-only.
    pub shares: Vec<(String, PathBuf, bool)>,
    /// Extra data disks attached after the boot disk (M10) — they enumerate as `vdb`, `vdc`, …
    /// in this order. See [`with_blank_data_disk`](GuestConfig::with_blank_data_disk) /
    /// [`with_data_disk`](GuestConfig::with_data_disk).
    pub data_disks: Vec<DataDisk>,
    /// Enable host-side VM snapshot/suspend (M9): the supervisor passes `--snapshot-file
    /// <scratch>/snapshot` to the worker, so a [`Guest::snapshot`] (SIGUSR1) writes the snapshot
    /// there; the worker then exits "snapshotted" (126) and the supervisor stops (suspend =
    /// teardown, NOT relaunch). See [`with_snapshot`](GuestConfig::with_snapshot) and
    /// [`Guest::snapshot_path`].
    pub snapshot: bool,
    /// Resume from a VM snapshot instead of cold-booting (M9): arms `--snapshot-file` at this
    /// path, and the supervisor's auto-resume (a pending snapshot at the armed path is consumed
    /// and restored, one-shot) picks it up. Set via [`restore_from`](GuestConfig::restore_from)
    /// with the snapshot a previous [`Guest`]'s [`Guest::snapshot`] wrote — models the real
    /// suspend/resume split (a second, separate boot finds the file). NOTE: the consume renames
    /// the file to `<name>.consumed`. `None` = cold boot.
    pub restore_from: Option<PathBuf>,
    /// Extra flags appended verbatim to the `limina` supervisor command line, for options the
    /// harness has no dedicated knob for (e.g. `--balloon-free-page-reporting`). Set via
    /// [`with_supervisor_arg`](GuestConfig::with_supervisor_arg).
    pub extra_supervisor_args: Vec<String>,
}

impl GuestConfig {
    /// L2 config: the in-repo Fedora image via EFI firmware (read-only).
    ///
    /// Overrides: `LIMINA_BIN`, `LIMINA_VMM_BIN`, `LIMINA_FIRMWARE`, `LIMINA_TEST_DISK`,
    /// `LIMINA_TEST_SHUTDOWN_GRACE` (seconds).
    pub fn fedora_from_env() -> Result<GuestConfig> {
        let firmware = default_firmware();
        anyhow::ensure!(
            firmware.exists(),
            "firmware not found at {firmware:?} (set LIMINA_FIRMWARE)"
        );

        let disk = match std::env::var("LIMINA_TEST_DISK") {
            Ok(p) => PathBuf::from(p),
            Err(_) => fedora_image("stock.test"),
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
            ssh_port: None,
            net_mac: None,
            supervisor_log: false,
            control_socket: false,
            balloon_control: false,
            memory: None,
            reclaim: None,
            envs: Vec::new(),
            shares: Vec::new(),
            data_disks: Vec::new(),
            snapshot: false,
            restore_from: None,
            extra_supervisor_args: Vec::new(),
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

    /// L2 config that EFI-boots an **aarch64 installer ISO as the sole disk** on our GOP firmware
    /// (M10 Phase 3a — boot from install media). The ISO is `vda` (read-only); the firmware's El
    /// Torito + FAT driver stack discovers the embedded ESP and chainloads the ISO's bootloader
    /// (`\EFI\BOOT\BOOTAA64.EFI` → GRUB). There is **no `--kernel`, no separate root disk, and no
    /// guest agent / SSH** — pure firmware self-discovery; the test watches the firmware/GRUB
    /// **console** ([`Guest::wait_for`]) for the bootloader-reached signal.
    ///
    /// Overrides: `LIMINA_TEST_ISO` (default repo-root [`DEFAULT_TEST_ISO`]), `LIMINA_GOP_FIRMWARE`
    /// (default [`DEFAULT_GOP_FIRMWARE`]). Returns an error (the test should SKIP) if the GOP firmware
    /// or the ISO is missing — both are gitignored & large.
    pub fn iso_boot_from_env() -> Result<GuestConfig> {
        let firmware = std::env::var("LIMINA_GOP_FIRMWARE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| repo_root().join(DEFAULT_GOP_FIRMWARE));
        anyhow::ensure!(
            firmware.exists(),
            "GOP firmware not found at {firmware:?}; build it with `scripts/build-krun-efi.sh` \
             (or set LIMINA_GOP_FIRMWARE)"
        );
        let iso = std::env::var("LIMINA_TEST_ISO")
            .map(PathBuf::from)
            .unwrap_or_else(|_| repo_root().join(DEFAULT_TEST_ISO));
        anyhow::ensure!(
            iso.exists(),
            "test ISO not found at {iso:?} (set LIMINA_TEST_ISO); fetch an EFI-bootable aarch64 \
             installer ISO, e.g. {DEFAULT_TEST_ISO} from dl.fedoraproject.org"
        );

        // Build directly (not via `fedora_from_env`, which would require the Fedora root disk): the
        // ISO IS the boot disk. No display — GRUB still mirrors its menu to the firmware's serial
        // ConOut even with the GOP firmware, so a serial `wait_for` is the lean, deterministic signal
        // (the spike's GOP-scanout PNG is the separate visual proof; see spikes/m10-iso-boot).
        Ok(GuestConfig {
            limina_bin: resolve_bin("limina", "LIMINA_BIN")?,
            vmm_bin: resolve_bin("limina-vmm", "LIMINA_VMM_BIN")?,
            boot: Boot::Firmware {
                firmware,
                disk: iso,
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
            ssh_port: None,
            net_mac: None,
            supervisor_log: false,
            control_socket: false,
            balloon_control: false,
            memory: None,
            reclaim: None,
            envs: Vec::new(),
            shares: Vec::new(),
            data_disks: Vec::new(),
            snapshot: false,
            restore_from: None,
            extra_supervisor_args: Vec::new(),
        })
    }

    /// L2 baseline-tier config: a **stock 4 KiB** Fedora that **autologins to a seated GNOME
    /// session**, EFI-booted on the GOP firmware. The vehicle for baseline-3D (virgl) tests that
    /// need a real GL session over SSH — this release's seated stock desktop, the same
    /// `Fedora-Workstation-<REL>.stock.test.raw` the other stock tests use (`accessible` autologins,
    /// so its `stock.test` snapshot is a seated session; see `docs/images.md`). Pair with
    /// [`with_coexist_display`](GuestConfig::with_coexist_display) +
    /// [`with_virgl_host_gl`](GuestConfig::with_virgl_host_gl) + [`with_net`](GuestConfig::with_net).
    ///
    /// Overrides: `LIMINA_TEST_DISK_BASELINE` (default = this release's `stock.test`),
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
            Err(_) => fedora_image("stock.test"),
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
            ssh_port: None,
            net_mac: None,
            supervisor_log: false,
            control_socket: false,
            balloon_control: false,
            memory: None,
            reclaim: None,
            envs: Vec::new(),
            shares: Vec::new(),
            data_disks: Vec::new(),
            snapshot: false,
            restore_from: None,
            extra_supervisor_args: Vec::new(),
        })
    }

    /// Minimal config for a bare-metal `--kernel` Image with no guest userspace — the harness
    /// just boots it and observes how the VM ends (e.g. `spikes/hvf-trap-probe`, which probes
    /// libkrun's HVF PSCI handling and then powers itself off). `rootfs` is a throwaway dir the
    /// guest never mounts (libkrun still exports it over virtio-fs); we point it at the Image's
    /// own directory so it always exists.
    pub fn raw_kernel(kernel: PathBuf) -> Result<GuestConfig> {
        let rootfs = kernel
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(repo_root);
        Ok(GuestConfig {
            limina_bin: resolve_bin("limina", "LIMINA_BIN")?,
            vmm_bin: resolve_bin("limina-vmm", "LIMINA_VMM_BIN")?,
            boot: Boot::Kernel {
                kernel,
                rootfs,
                cmdline: "console=ttyAMA0".to_string(),
            },
            vsock: None,
            display: None,
            cpus: 1,
            ram_mib: 512,
            shutdown_grace: Duration::from_secs(grace_from_env()),
            console_input: false,
            console_channel: ConsoleChannel::Virtio,
            net: false,
            ssh_port: None,
            net_mac: None,
            supervisor_log: false,
            control_socket: false,
            balloon_control: false,
            memory: None,
            reclaim: None,
            envs: Vec::new(),
            shares: Vec::new(),
            data_disks: Vec::new(),
            snapshot: false,
            restore_from: None,
            extra_supervisor_args: Vec::new(),
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
            Err(_) => fedora_image("stock.test"),
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
            ssh_port: None,
            net_mac: None,
            supervisor_log: false,
            control_socket: false,
            balloon_control: false,
            memory: None,
            reclaim: None,
            envs: Vec::new(),
            shares: Vec::new(),
            data_disks: Vec::new(),
            snapshot: false,
            restore_from: None,
            extra_supervisor_args: Vec::new(),
        })
    }

    /// Like [`GuestConfig::enhanced_fedora_from_env`], but booting the **seated ENHANCED test
    /// golden** (`Fedora-Workstation-<REL>.enhanced.test.raw`, override `LIMINA_TEST_DISK_ENH`): the
    /// RPM-delivered enhanced image (16k kernel + mesa 26.2 venus at `/usr` + patched mutter) with
    /// gdm autologin to a gnome-shell-on-venus session, plus the test tooling baked in —
    /// `apitrace`/`eglretrace` and `/opt/gfxreconstruct/bin/gfxrecon-replay` (see
    /// `docs/images.md`). This is the vehicle for tests that need a *running graphical session*
    /// (Xwayland, the zink→venus GL stack) rather than just venus enumeration. (Replaces the retired
    /// source-built `dev-enh.raw`; mesa moved `/opt/mesa-zink` → `/usr`, so `ZINK_ENV` in the tests
    /// drops the loader-path vars but keeps the driver-selection knobs — env-trap still applies.)
    ///
    /// The host Vulkan backend (KosmicKrisp) is wired automatically by [`Guest::boot`] for any
    /// coexist/venus display — see [`kosmickrisp_icd`]; a venus-requiring test should still SKIP
    /// up front when KK is absent (so it doesn't silently run on the software-2D fallback).
    /// Returns an error (the test should SKIP) if the 16 KiB kernel or the enhanced disk is missing.
    pub fn seated_fedora_from_env() -> Result<GuestConfig> {
        let mut cfg = GuestConfig::enhanced_fedora_from_env()?;
        let disk = match std::env::var("LIMINA_TEST_DISK_ENH") {
            Ok(p) => PathBuf::from(p),
            Err(_) => fedora_image("enhanced.test"),
        };
        anyhow::ensure!(
            disk.exists(),
            "seated enhanced disk not found at {disk:?} (set LIMINA_TEST_DISK_ENH); this is the \
             machine-local enhanced test golden, see docs/images.md"
        );
        if let Boot::KernelDisk { disk: d, .. } = &mut cfg.boot {
            *d = disk;
        }
        Ok(cfg)
    }

    /// Like [`GuestConfig::seated_fedora_from_env`], but **EFI-booting** the seated enhanced
    /// golden through the GOP firmware — the guest's OWN installed kernel + initrd, enforcing
    /// SELinux, exactly as production boots it (`cargo xtask run --disk`). This is the vehicle
    /// for **classic-vrend (GL ladder) session tests**: the injected-6.12 seated path never
    /// produces classic `CmdSubmit3d` traffic even on a live desktop (its shell composites
    /// without classic submits — kms_swrast-like), while the EFI-booted session runs the real
    /// vrend world (task #19). Overrides: `LIMINA_GOP_FIRMWARE`, `LIMINA_TEST_DISK_ENH`, plus
    /// the usual `LIMINA_BIN`/`LIMINA_VMM_BIN`. Returns an error (the test should SKIP) if the
    /// GOP firmware or the enhanced disk is missing.
    pub fn seated_efi_fedora_from_env() -> Result<GuestConfig> {
        let firmware = std::env::var("LIMINA_GOP_FIRMWARE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| repo_root().join(DEFAULT_GOP_FIRMWARE));
        anyhow::ensure!(
            firmware.exists(),
            "GOP firmware not found at {firmware:?}; build it with `scripts/build-krun-efi.sh` \
             (or set LIMINA_GOP_FIRMWARE)"
        );
        let mut cfg = GuestConfig::seated_fedora_from_env()?;
        let disk = match &cfg.boot {
            Boot::KernelDisk { disk, .. } => disk.clone(),
            other => anyhow::bail!("seated_fedora_from_env built an unexpected boot {other:?}"),
        };
        cfg.boot = Boot::Firmware {
            firmware,
            disk,
            read_only: true, // with_net cow-clones to a writable disk; the source stays pristine
        };
        Ok(cfg)
    }

    /// Like [`GuestConfig::seated_efi_fedora_from_env`], but booting the **synoik** enhanced image
    /// — the same enhanced userspace with synoik, our Vulkan compositor, in place of
    /// gnome-shell/mutter.
    ///
    /// The disk is **pinned to the F44 family** (`Fedora-Workstation-44.enhanced.synoik.raw`), the
    /// same way [`bench::tier_config`](crate::bench::tier_config) pins the enhanced golden. The pin
    /// predates the 2026-08-15 default flip (`LIMINA_FEDORA_REL` used to default to 43, and synoik
    /// is only produced for F44, so honouring the env would have SKIPped the guard out of the
    /// default suite); it is now redundant, and kept because synoik genuinely exists for one
    /// release only. `LIMINA_TEST_DISK_SYNOIK` still
    /// overrides for a deliberate different disk (e.g. an older clone, to check the test
    /// discriminates).
    ///
    /// **EFI is load-bearing here, not stylistic.** The injected-kernel seated path
    /// ([`seated_fedora_from_env`](GuestConfig::seated_fedora_from_env)) boots the test
    /// `Image-16k`, a 6.12 binary built *before* the 2026-08-04 drop of our two virtio-gpu plane
    /// commits (`74ae69adc645` advertise `DRM_FORMAT_MOD_LINEAR`, `1f4c2049b30b` widen the primary
    /// plane format list). That kernel still advertises LINEAR, so synoik's format negotiation
    /// succeeds on it whatever the compositor does — a test on that path would be green today and
    /// blind to the whole failure class. Only the guest's **own installed kernel**, booted through
    /// the GOP firmware, meets a stock virtio-gpu plane. See `docs/images.md` §KNOWN DRIFT.
    ///
    /// Overrides: `LIMINA_TEST_DISK_SYNOIK`, `LIMINA_GOP_FIRMWARE`, plus the usual
    /// `LIMINA_BIN`/`LIMINA_VMM_BIN`. Returns an error (the test should SKIP) if the GOP firmware
    /// or the synoik image is missing — it is a machine-local golden, see `docs/images.md`.
    pub fn seated_efi_synoik_from_env() -> Result<GuestConfig> {
        let disk = match std::env::var("LIMINA_TEST_DISK_SYNOIK") {
            Ok(p) => PathBuf::from(p),
            Err(_) => repo_root().join("Fedora-Workstation-44.enhanced.synoik.raw"),
        };
        anyhow::ensure!(
            disk.exists(),
            "synoik enhanced disk not found at {disk:?} (set LIMINA_TEST_DISK_SYNOIK); this is a \
             machine-local golden, see docs/images.md"
        );
        let mut cfg = GuestConfig::seated_efi_fedora_from_env()?;
        match &mut cfg.boot {
            Boot::Firmware { disk: d, .. } => *d = disk,
            other => anyhow::bail!("seated_efi_fedora_from_env built an unexpected boot {other:?}"),
        }
        Ok(cfg)
    }

    /// L2 config for the **≥7.1-kernel virtiofs share guard** (task #36): the same injected-kernel
    /// enhanced path as [`enhanced_fedora_from_env`](GuestConfig::enhanced_fedora_from_env), but
    /// booting a **≥7.1** 16 KiB test kernel instead of the venus tests' 6.12 `Image-16k`.
    ///
    /// Linux ≥7.1 added `virtio_fs_verify_response`, which rejects any FUSE reply whose virtio
    /// used-ring length doesn't cover the out-header (`-EIO` → latches `fc->conn_error` → surfaces
    /// as `ECONNREFUSED` at `mount(2)`); libkrun 0090 reports the reply byte count as the used
    /// length. No automated test ran a share on a ≥7.1 guest before this — L1 (`l1_share`) uses
    /// libkrunfw's 6.12 kernel, the enhanced/seated L2 inject the 6.12 `Image-16k`, and only the
    /// un-tested EFI path runs the real 7.1.4 — so the fix shipped without a guard. Kept on a
    /// **distinct** kernel file so the venus suite still runs on its validated 6.12 kernel.
    ///
    /// Build the kernel first:
    /// `KVER=v7.1.8 PAGESIZE=16k KIMAGE_NAME=Image-16k-71 scripts/build-test-kernel.sh`.
    /// Overrides: `LIMINA_TEST_KERNEL_71` (default `target/test-guest/kernel/Image-16k-71`),
    /// `LIMINA_TEST_DISK` (default this release's `stock.test`), plus the usual
    /// `LIMINA_BIN`/`LIMINA_VMM_BIN`. Returns an error (the test should SKIP) if the ≥7.1 kernel or
    /// the disk is missing. Pair with [`with_net`](GuestConfig::with_net) +
    /// [`with_share`](GuestConfig::with_share)/[`with_share_ro`](GuestConfig::with_share_ro); no
    /// display is needed (this exercises virtio-fs, not venus).
    pub fn enhanced_share_from_env() -> Result<GuestConfig> {
        let kernel = std::env::var("LIMINA_TEST_KERNEL_71")
            .map(PathBuf::from)
            .unwrap_or_else(|_| repo_root().join("target/test-guest/kernel/Image-16k-71"));
        anyhow::ensure!(
            kernel.exists(),
            "≥7.1 test kernel not found at {kernel:?}; build it with \
             `KVER=v7.1.8 PAGESIZE=16k KIMAGE_NAME=Image-16k-71 \
             scripts/build-test-kernel.sh` (or set LIMINA_TEST_KERNEL_71)"
        );
        let disk = match std::env::var("LIMINA_TEST_DISK") {
            Ok(p) => PathBuf::from(p),
            Err(_) => fedora_image("stock.test"),
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
            ssh_port: None,
            net_mac: None,
            supervisor_log: false,
            control_socket: false,
            balloon_control: false,
            memory: None,
            reclaim: None,
            envs: Vec::new(),
            shares: Vec::new(),
            data_disks: Vec::new(),
            snapshot: false,
            restore_from: None,
            extra_supervisor_args: Vec::new(),
        })
    }

    /// Attach a user-mode NAT NIC. The supervisor spawns a gvproxy gateway and captures its
    /// `-debug` log; assert on it via [`Guest::wait_for_gateway_log`] (DHCP lease, outbound).
    /// Append a verbatim extra flag to the `limina` supervisor command line.
    pub fn with_supervisor_arg(mut self, arg: &str) -> GuestConfig {
        self.extra_supervisor_args.push(arg.to_string());
        self
    }

    pub fn with_net(mut self) -> GuestConfig {
        self.net = true;
        self
    }

    /// Like [`with_net`](GuestConfig::with_net) but pin gvproxy's inbound SSH forward to a specific
    /// host `port` (`--ssh-port`) instead of the default 2222. Give two concurrent VMs distinct
    /// ports so they can run in parallel without colliding on the host port;
    /// [`Guest::wait_for_ssh_banner`]/[`Guest::ssh_exec`] then target that VM's own port.
    pub fn with_ssh_port(mut self, port: u16) -> GuestConfig {
        self.net = true;
        self.ssh_port = Some(port);
        self
    }

    /// Like [`with_net`](GuestConfig::with_net) but give the guest NIC a custom MAC
    /// (`--net-mac`) — the managed-VM path where the supervisor rebinds gvproxy's static
    /// .2 lease to the VM's persistent MAC via a generated config file.
    pub fn with_net_mac(mut self, mac: &str) -> GuestConfig {
        self.net = true;
        self.net_mac = Some(mac.to_string());
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
            windowed: false,
            pool: 1,
        });
        self
    }

    /// Configure a scanout **pool** of `pool` displays instead of one. Slot 0 boots connected
    /// at the size the display builder set; every other slot boots disconnected, waiting for a
    /// [`Guest::update_display`] to give it an identity and connect it. `num_scanouts` is
    /// virtio-gpu config-space state, so this is the only place the count can be chosen — a
    /// display cannot be added to a running device. Call after a display builder.
    pub fn with_display_pool(mut self, pool: u32) -> GuestConfig {
        let display = self
            .display
            .as_mut()
            .expect("with_display_pool needs a display builder first");
        display.pool = pool;
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
            windowed: false,
            pool: 1,
        });
        // A coexist display runs virglrenderer's `vrend` half (host GL via zink-on-KK), whose
        // `virgl_renderer_init` dlopens libEGL at startup — so a coexist boot ALWAYS needs the
        // host-GL worker env, or the worker aborts with "Couldn't open libEGL.dylib" before the
        // guest even comes up. Wire it here so every coexist test (venus, venus_replay, the stock
        // render test) gets it automatically; `with_virgl_host_gl` is idempotent, so an explicit
        // call afterwards is a harmless no-op. (KK absent → `Guest::boot` degrades to software-2D,
        // which doesn't touch libEGL, so the env is set-but-unused and still harmless.)
        self.with_virgl_host_gl()
    }

    /// A coexist display shown in a **real native window** (`--window`) at a pinned
    /// resolution, instead of captured to a PNG.
    ///
    /// Use this — and only this — when the subject under test is the SUPERVISOR's own
    /// frame-apply path. `--display-capture` boots never enter `window::run`, so a capture
    /// boot exercises none of the window's per-scanout state and would pass green against a
    /// window that leaks a framebuffer per frame (which is exactly what limina `8e00d94`
    /// fixed). Pinning `--display-resolution` to `width`x`height` keeps the scanout size
    /// independent of whichever host screen the window opens on, so byte thresholds and
    /// surface counts mean the same thing on every rig.
    ///
    /// A window really does open during the run, so a test using this belongs in the
    /// EXCLUSIVE set in `.config/nextest.toml` — it should not overlap other guests.
    pub fn with_windowed_coexist_display(mut self, width: u32, height: u32) -> GuestConfig {
        self.display = Some(DisplayCfg {
            width,
            height,
            software_2d: false,
            windowed: true,
            pool: 1,
        });
        self.with_virgl_host_gl()
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
        // Since the MTL4 rebase, mesa's zink dlopens "@rpath/libvulkan.1.dylib" and
        // the installed libgallium carries no matching LC_RPATH (meson strips build rpaths at
        // install), so the dlopen fails → `virgl_renderer_init` fails → the worker silently
        // degrades to software-2D and every seated/venus test dies downstream (caught live:
        // a full suite run failed this way). DYLD_LIBRARY_PATH intercepts by leaf
        // name BEFORE rpath resolution — but pointing it at all of /opt/homebrew/lib would
        // shadow every Homebrew leaf name for the whole process tree, so use a shim dir
        // holding ONLY the Vulkan loader symlink (same shim as boot-enhanced-efi-kk.sh).
        let shim = prefix.join("vulkan-rpath");
        let _ = std::fs::create_dir_all(&shim);
        let shim_loader = shim.join("libvulkan.1.dylib");
        if !shim_loader.exists() {
            let _ = std::os::unix::fs::symlink("/opt/homebrew/lib/libvulkan.1.dylib", &shim_loader);
        }
        let shim = shim.display().to_string();
        for (k, v) in [
            ("DYLD_LIBRARY_PATH", shim.as_str()),
            ("DYLD_FALLBACK_LIBRARY_PATH", dyld.as_str()),
            ("MESA_LOADER_DRIVER_OVERRIDE", "zink"),
            ("GALLIUM_DRIVER", "zink"),
            ("LIBGL_DRIVERS_PATH", drivers.as_str()),
            ("EGL_PLATFORM", "surfaceless"),
        ] {
            // Idempotent: `with_coexist_display` already applies these, and a test may also call
            // this explicitly — don't double-push (a caller's own override of a key also wins).
            if !self.envs.iter().any(|(ek, _)| ek == k) {
                self.envs.push((k.to_string(), v.to_string()));
            }
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

    /// Wire the vsock bridge for the **M7 USB/IP mock-attach** test: same plumbing as
    /// [`with_vsock`](GuestConfig::with_vsock) (host listens on a unix socket, guest connects to
    /// `CID_HOST:port`), but the init runs the USB/IP *client* (`limina.usb_attach=<port>`) instead
    /// of the control agent. The test then [`accept_usbip_mock`](Guest::accept_usbip_mock)s the
    /// connection and serves the hardware-free CDC-ACM mock on it. Kernel boot only.
    pub fn with_usbip_vsock(mut self, port: u32) -> GuestConfig {
        let socket_path = std::env::temp_dir().join(format!(
            "limina-usbip-{}-{}.sock",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        if let Boot::Kernel { cmdline, .. } = &mut self.boot {
            cmdline.push_str(&format!(" limina.usb_attach={port}"));
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

    /// Enable host-side VM snapshot/suspend (M9): the worker gets `--snapshot-file
    /// <scratch>/snapshot`, so [`Guest::snapshot`] can trigger a suspend + auto-resume cycle.
    pub fn with_snapshot(mut self) -> GuestConfig {
        self.snapshot = true;
        self
    }

    /// Resume from a VM snapshot (M9): arms `--snapshot-file` at the given path so the
    /// supervisor's auto-resume consumes it (single-use rename to `.consumed`) and the worker
    /// restores RAM + vCPUs + GIC instead of cold-booting. Pass the snapshot file a previous
    /// suspended [`Guest`] wrote (from [`Guest::snapshot_path`]); keep that Guest alive until this
    /// one has booted, since the file lives in its scratch dir. Use the SAME cpus/ram/boot config.
    pub fn restore_from(mut self, snapshot: impl Into<PathBuf>) -> GuestConfig {
        self.restore_from = Some(snapshot.into());
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

    /// Bind a runtime balloon control socket (M6 dynamic memory) at a known scratch path, so the
    /// harness can drive the target with [`Guest::set_balloon_target`] and read [`Guest::balloon_stats`].
    pub fn with_balloon_control(mut self) -> GuestConfig {
        self.balloon_control = true;
        self
    }

    /// Set the dynamic-memory range `MIN..MAX` MiB (M6): the supervisor allocates `MAX` RAM and runs
    /// the PSI autoballoon policy. Pair with [`with_control_socket`](GuestConfig::with_control_socket)
    /// (to inject `MemPressure` as a peer) and [`with_balloon_control`](GuestConfig::with_balloon_control)
    /// (to read back the target the policy commands).
    pub fn with_memory(mut self, min_mib: usize, max_mib: usize) -> GuestConfig {
        self.memory = Some((min_mib, max_mib));
        self.ram_mib = max_mib;
        self
    }

    /// Select the autoballoon `--reclaim` mode (`disabled`/`light`/`moderate`/`aggressive`;
    /// the supervisor defaults to moderate). Only meaningful with [`GuestConfig::with_memory`].
    pub fn with_reclaim(mut self, mode: &str) -> GuestConfig {
        self.reclaim = Some(mode.to_string());
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

    /// Attach a blank, writable data disk of `size_bytes` after the boot disk (M10). The harness
    /// creates a sparse raw in the scratch dir; the guest sees it as the next `vdX` (e.g. `vdb`),
    /// unformatted. Repeatable; disks attach in call order.
    pub fn with_blank_data_disk(mut self, size_bytes: u64) -> GuestConfig {
        self.data_disks.push(DataDisk::Blank { size_bytes });
        self
    }

    /// Attach a blank, writable **qcow2** data disk of `size_bytes` *virtual* size after the boot
    /// disk (M10 Phase 4). Created via `qemu-img` in scratch; the physical file is tiny, so the
    /// guest sees the full size only if the worker auto-detects the qcow2 format.
    pub fn with_qcow2_data_disk(mut self, size_bytes: u64) -> GuestConfig {
        self.data_disks.push(DataDisk::BlankQcow2 { size_bytes });
        self
    }

    /// Attach an existing image as a data disk after the boot disk (M10). Read-write images are
    /// cow-cloned into scratch first (the shared source is never mutated); `read_only` attaches
    /// the source in place as `:ro`.
    pub fn with_data_disk(mut self, path: &Path, read_only: bool) -> GuestConfig {
        self.data_disks.push(DataDisk::Existing {
            path: path.to_path_buf(),
            read_only,
        });
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
pub fn repo_root() -> PathBuf {
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
    /// Runtime balloon control socket (inside `scratch`), bound by the worker when
    /// [`GuestConfig::balloon_control`] is set. See [`Guest::set_balloon_target`].
    balloon_socket: Option<PathBuf>,
    /// Write handle to the guest console input FIFO, if an interactive console was enabled.
    console_in: Option<fs::File>,
    /// Path to the gvproxy gateway's `-debug` log (inside `scratch`), if net was enabled.
    gateway_log: Option<PathBuf>,
    /// Host port of gvproxy's inbound `127.0.0.1:<port> → guest:22` forward (the `--ssh-port`
    /// the supervisor was launched with; default [`DEFAULT_SSH_PORT`]). Distinct per VM lets
    /// several run in parallel; [`Guest::wait_for_ssh_banner`]/[`Guest::ssh_exec`] target it.
    ssh_port: u16,
    /// Path to the captured supervisor stderr (inside `scratch`), if enabled.
    supervisor_log: Option<PathBuf>,
    /// Path to the pinned supervisor-owned control socket (inside `scratch`), if enabled.
    control_socket: Option<PathBuf>,
    /// The armed VM snapshot path: `<scratch>/snapshot` if [`GuestConfig::snapshot`] was set,
    /// or the [`GuestConfig::restore_from`] path. A [`Guest::snapshot`] writes it; a later boot
    /// arming the same path auto-resumes from it (consuming it — rename to `.consumed`).
    snapshot_path: Option<PathBuf>,
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
        for px in self.rgba.as_chunks::<4>().0 {
            set.insert([px[0], px[1], px[2], px[3]]);
        }
        set.len()
    }

    /// The most common RGBA pixel (the presumed background) and its share of all pixels.
    pub fn dominant_color(&self) -> ([u8; 4], f64) {
        let mut counts = std::collections::HashMap::new();
        let total = (self.rgba.len() / 4).max(1);
        for px in self.rgba.as_chunks::<4>().0 {
            *counts.entry([px[0], px[1], px[2], px[3]]).or_insert(0u64) += 1;
        }
        counts
            .into_iter()
            .max_by_key(|&(_, n)| n)
            .map(|(c, n)| (c, n as f64 / total as f64))
            .unwrap_or(([0, 0, 0, 0], 1.0))
    }
}

/// One `stats` reply from the worker's balloon control socket, all in bytes. `target` is the
/// last commanded balloon size, `actual` the guest driver's self-reported size, `reclaimed`
/// the cumulative host memory returned via `MADV_FREE_REUSABLE`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BalloonStats {
    pub target: u64,
    pub actual: u64,
    pub reclaimed: u64,
    /// Stage-2 heal faults taken (guest touches on released ranges), cumulative.
    pub heals: u64,
    /// Bytes stage-2 unmapped by release, cumulative.
    pub released: u64,
    /// Bytes re-validated and re-mapped (heals + deflate reclaims), cumulative.
    pub remapped: u64,
    /// Stage-2 translation faults outside every released range (should stay 0).
    pub strays: u64,
    /// Ledger settle sweeps completed (task-pmap double-billing debit), cumulative.
    pub sweeps: u64,
    /// Bytes the last settle sweep debited off the worker's phys_footprint.
    pub sweep_debited: u64,
    /// Wall-clock duration of the last settle sweep, in milliseconds.
    pub sweep_ms: u64,
    /// Worker-thread guest-RAM touches fielded by the sweep fault handler, cumulative.
    pub sweep_faults: u64,
    /// The worker's task-wide compressor-billed bytes (TASK_VM_INFO).
    pub compressed: u64,
    /// The worker's phys_footprint as the worker itself reads it (TASK_VM_INFO).
    pub footprint: u64,
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
        // Fail fast (and legibly) if a concurrent build stripped the worker's hypervisor
        // entitlement — otherwise every boot dies as a cryptic VmSetup(VmCreate).
        ensure_hypervisor_entitlement(&cfg.vmm_bin)?;

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
                // The guest serves this directory rw as its virtio-fs root, so give each guest
                // its own APFS clone (per-file cow, the tree is small) — concurrent tests must
                // never share a writable rootfs, and even sequential runs stop dirtying the
                // built golden. Same philosophy as the disk clones above.
                let rootfs_clone = scratch.join("rootfs");
                cow_clone_dir(rootfs, &rootfs_clone)
                    .with_context(|| format!("cow-cloning rootfs {rootfs:?} for this guest"))?;
                cmd.arg("--kernel")
                    .arg(kernel)
                    .arg("--rootfs")
                    .arg(&rootfs_clone)
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
        // Extra data disks (M10): one --disk per data disk AFTER the boot disk, so the guest
        // enumerates them as vdb, vdc, … in declared order (attach order = device order).
        for (i, dd) in cfg.data_disks.iter().enumerate() {
            let arg = match dd {
                DataDisk::Blank { size_bytes } => {
                    let p = scratch.join(format!("data{i}.raw"));
                    let f = fs::File::create(&p)
                        .with_context(|| format!("creating blank data disk {p:?}"))?;
                    f.set_len(*size_bytes)
                        .with_context(|| format!("sizing blank data disk {p:?} to {size_bytes}"))?;
                    p.to_str()
                        .with_context(|| format!("data disk path not UTF-8: {p:?}"))?
                        .to_string()
                }
                DataDisk::BlankQcow2 { size_bytes } => {
                    let p = scratch.join(format!("data{i}.qcow2"));
                    let status = std::process::Command::new("qemu-img")
                        .args(["create", "-q", "-f", "qcow2"])
                        .arg(&p)
                        .arg(format!("{size_bytes}"))
                        .status()
                        .with_context(|| "running qemu-img create (is qemu installed?)")?;
                    anyhow::ensure!(status.success(), "qemu-img create failed for {p:?}");
                    p.to_str()
                        .with_context(|| format!("data disk path not UTF-8: {p:?}"))?
                        .to_string()
                }
                DataDisk::Existing {
                    path,
                    read_only: true,
                } => {
                    let s = path
                        .to_str()
                        .with_context(|| format!("data disk path not UTF-8: {path:?}"))?;
                    format!("{s}:ro")
                }
                DataDisk::Existing {
                    path,
                    read_only: false,
                } => {
                    let p = scratch.join(format!("data{i}.raw"));
                    cow_clone(path, &p)
                        .with_context(|| format!("cow-cloning data disk {path:?}"))?;
                    p.to_str()
                        .with_context(|| format!("data disk path not UTF-8: {p:?}"))?
                        .to_string()
                }
            };
            cmd.arg("--disk").arg(arg);
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

        // VM snapshot file (M9 suspend/resume): the worker writes the snapshot here on a SIGUSR1
        // trigger, then exits "snapshotted" (126) and the supervisor stops (no relaunch). Resume
        // is AUTOMATIC and one-shot (M9.4): arming a path where a snapshot already exists makes
        // the supervisor consume it (rename `.consumed`) and boot the worker restoring — there is
        // no `--restore` flag anymore, so `restore_from` is just arming a pre-existing snapshot.
        let snapshot_path = cfg
            .restore_from
            .clone()
            .or_else(|| cfg.snapshot.then(|| scratch.join("snapshot")));
        if let Some(path) = &snapshot_path {
            cmd.arg("--snapshot-file").arg(path);
        }

        // Display: capture the scanout into the scratch dir (auto-cleaned on Drop). Whenever a
        // display is attached we also wire a runtime resize control socket so tests can drive
        // window-resize via [`Guest::resize_display`] (the worker binds it; we connect per call).
        let resize_socket = cfg.display.as_ref().map(|_| scratch.join("resize.sock"));

        // Balloon control socket (M6 dynamic memory): bound by the worker when requested, driven by
        // the harness via [`Guest::set_balloon_target`] / [`Guest::balloon_stats`].
        let balloon_socket = cfg.balloon_control.then(|| scratch.join("balloon.sock"));
        if let Some(path) = &balloon_socket {
            cmd.arg("--balloon-control-socket").arg(path);
        }
        // Dynamic-memory range (M6): starts the supervisor's PSI autoballoon policy.
        if let Some((min, max)) = cfg.memory {
            cmd.arg("--memory").arg(format!("{min}..{max}"));
        }
        if let Some(mode) = &cfg.reclaim {
            cmd.arg("--reclaim").arg(mode);
        }
        let capture_png = match &cfg.display {
            Some(d) => {
                // A windowed display opens a REAL NSWindow and runs the supervisor's
                // frame-apply path (`window::run`) — the only way to exercise host-side
                // per-scanout state, since `--display-capture` never enters that code at
                // all. `--display-resolution WIDTHxHEIGHT` pins the guest mode so the
                // surface size doesn't vary with whichever screen the window lands on.
                // The two flags are mutually exclusive in the CLI, so there is no capture
                // PNG for a windowed boot.
                let png = scratch.join("scanout.png");
                if d.windowed {
                    cmd.arg("--window")
                        .arg("--display-resolution")
                        .arg(format!("{}x{}", d.width, d.height))
                        .arg("--display-control-socket")
                        .arg(
                            resize_socket
                                .as_ref()
                                .expect("resize_socket set with a display"),
                        );
                } else {
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
                }
                // The scanout pool. Omitted at 1 so every existing test still spawns the exact
                // argv it always did, and a pool test is visibly the only one asking for more.
                if d.pool > 1 {
                    cmd.arg("--display-pool").arg(d.pool.to_string());
                }
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
                // No capture path for a windowed boot: the pixels go to a real window, so
                // `wait_for_capture` must fail loudly rather than wait on a PNG nothing writes.
                (!d.windowed).then_some(png)
            }
            None => None,
        };

        // Filled in by the net block below with the port the supervisor is TOLD to use
        // (explicit cfg.ssh_port or a pre-allocated ephemeral one).
        let mut allocated_ssh_port: Option<u16> = None;

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
            // Per-VM SSH-forward port — ALWAYS passed explicitly. When the flag is absent the
            // supervisor AUTO-ALLOCATES from 2222 up, so with another VM holding 2222 the forward
            // silently lands elsewhere while a harness that assumes 2222 ssh'es into the BYSTANDER
            // VM's guest (identical test creds — every check "works", against the wrong guest;
            // cost an evening of phantom venus failures). Pre-allocating an ephemeral
            // port here keeps the port known without parsing the supervisor log.
            let port = match cfg.ssh_port {
                Some(p) => p,
                None => std::net::TcpListener::bind((FORWARDED_SSH_HOST, 0))
                    .context("probing a free ssh-forward port")?
                    .local_addr()
                    .context("reading the probed ssh-forward port")?
                    .port(),
            };
            allocated_ssh_port = Some(port);
            cmd.arg("--ssh-port").arg(port.to_string());
            // Per-VM guest MAC → the supervisor's gvproxy-config (static-lease rebind) path.
            if let Some(mac) = &cfg.net_mac {
                cmd.arg("--net-mac").arg(mac);
            }
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

        for arg in &cfg.extra_supervisor_args {
            cmd.arg(arg);
        }

        for (k, v) in &cfg.envs {
            cmd.env(k, v);
        }

        // Several L1 tests assert on info-level supervisor log lines (e.g. "guest agent
        // connected"). Since the supervisor's logger now defaults to warn (quiet production
        // logs), those lines are suppressed unless RUST_LOG opts in — which silently broke the
        // control-plane tests. Default the spawned supervisor to info here, respecting an
        // explicit level from the caller's env or cfg.envs.
        let rust_log_chosen = std::env::var_os("RUST_LOG").is_some()
            || cfg.envs.iter().any(|(k, _)| k.as_str() == "RUST_LOG");
        if !rust_log_chosen {
            cmd.env("RUST_LOG", "info");
        }

        // Supervisor/worker logs: flow to the test's stderr by default (visible with
        // --nocapture); captured to a scratch file when the test asserts on them. Capture
        // BOTH streams: worker-side log lines have been observed on the supervisor's stdout
        // (not just stderr) in capture-display boots, and an assertion needle that lands on
        // the un-captured stream times out invisibly (venus_fence_present found this —
        // its [FENCEPRESENT] worker oracle fired on stdout while the file got stderr only).
        cmd.stdin(Stdio::null());
        let supervisor_log = if cfg.supervisor_log {
            let path = scratch.join("supervisor.log");
            let file = fs::File::create(&path)
                .with_context(|| format!("creating supervisor log {path:?}"))?;
            let stdout_file = file
                .try_clone()
                .with_context(|| format!("cloning supervisor log handle {path:?}"))?;
            cmd.stderr(Stdio::from(file));
            cmd.stdout(Stdio::from(stdout_file));
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
            balloon_socket,
            console_in,
            gateway_log,
            ssh_port: allocated_ssh_port.unwrap_or(DEFAULT_SSH_PORT),
            supervisor_log,
            control_socket,
            snapshot_path,
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
        self.send_display_command(&DisplayCommand::Resize { width, height })
    }

    /// Push a full display update — identity, mode list, refresh range, connection state — the
    /// way the supervisor does when the window moves to another host display. See
    /// `docs/design/stable-edid-hotplug.md`.
    pub fn update_display(&self, control: DisplayControl) -> Result<()> {
        self.send_display_command(&DisplayCommand::Display(control))
    }

    fn send_display_command(&self, command: &DisplayCommand) -> Result<()> {
        use std::io::Write;
        let path = self
            .resize_socket
            .as_ref()
            .context("display commands require a display in the GuestConfig")?;
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
        let line = command.to_wire();
        writeln!(stream, "{line}").with_context(|| format!("sending {line:?} to {path:?}"))?;
        stream.flush().ok();
        Ok(())
    }

    /// Connect to the worker's balloon control socket (M6), retrying for a few seconds because the
    /// worker binds it partway through boot. Requires [`GuestConfig::with_balloon_control`].
    fn connect_balloon(&self) -> Result<UnixStream> {
        let path = self
            .balloon_socket
            .as_ref()
            .context("balloon control requires GuestConfig::with_balloon_control")?;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match UnixStream::connect(path) {
                Ok(s) => return Ok(s),
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("connecting to the balloon-control socket {path:?}")
                    })
                }
            }
        }
    }

    /// Set the balloon target in bytes: the worker forwards it to the live virtio-balloon (which
    /// inflates/deflates toward it). M6 dynamic memory.
    pub fn set_balloon_target(&self, bytes: u64) -> Result<()> {
        use std::io::Write;
        let mut stream = self.connect_balloon()?;
        writeln!(stream, "target {bytes}").context("sending balloon target")?;
        stream.flush().ok();
        Ok(())
    }

    /// Read balloon stats from the worker. The `target`/`actual` gap is the oscillation /
    /// "Out of puff" signature — a held nonzero gap means the guest is chasing a target it
    /// cannot fill. M6 dynamic memory.
    pub fn balloon_stats(&self) -> Result<BalloonStats> {
        use std::io::{BufRead, BufReader, Write};
        let mut stream = self.connect_balloon()?;
        writeln!(stream, "stats").context("requesting balloon stats")?;
        stream.flush().ok();
        let mut line = String::new();
        BufReader::new(&stream)
            .read_line(&mut line)
            .context("reading balloon stats reply")?;
        // Reply: `target=<bytes> actual=<bytes> reclaimed=<bytes> heals=<n> released=<bytes>
        // remapped=<bytes> strays=<n>`.
        let mut stats = BalloonStats::default();
        for tok in line.split_whitespace() {
            let Some((k, v)) = tok.split_once('=') else {
                continue;
            };
            let v: u64 = v.parse().unwrap_or(0);
            match k {
                "target" => stats.target = v,
                "actual" => stats.actual = v,
                "reclaimed" => stats.reclaimed = v,
                "heals" => stats.heals = v,
                "released" => stats.released = v,
                "remapped" => stats.remapped = v,
                "strays" => stats.strays = v,
                "sweeps" => stats.sweeps = v,
                "sweep_debited" => stats.sweep_debited = v,
                "sweep_ms" => stats.sweep_ms = v,
                "sweep_faults" => stats.sweep_faults = v,
                "compressed" => stats.compressed = v,
                "footprint" => stats.footprint = v,
                _ => {}
            }
        }
        Ok(stats)
    }

    /// Request a ledger settle sweep (task-pmap double-billing debit; no reply — poll
    /// [`Guest::balloon_stats`] for `sweeps` to advance). M6 dynamic memory / hv-ledger-gap.
    pub fn settle_sweep(&self) -> Result<()> {
        use std::io::Write;
        let mut stream = self.connect_balloon()?;
        writeln!(stream, "settle").context("sending settle sweep command")?;
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

    /// Accept the guest's USB/IP connection (M7) and serve the hardware-free **CDC-ACM mock** on it
    /// in a background thread. The guest (`limina.usb_attach`) connects, imports the mock, and hands
    /// the fd to `vhci_hcd`; `limina_usbip::serve` answers the import + every URB the kernel submits,
    /// so the mock device enumerates as `/dev/ttyACM0`. The serve thread runs until the guest detaches
    /// (the test asserts the guest's `RESULT: ttyACM0 …` marker, then powers off). Returns once the
    /// connection is accepted and the server thread is running.
    pub fn accept_usbip_mock(&mut self, timeout: Duration) -> Result<()> {
        let listener = self
            .vsock_listener
            .as_ref()
            .context("no vsock configured (use GuestConfig::with_usbip_vsock)")?;
        listener
            .set_nonblocking(true)
            .context("set_nonblocking on vsock listener")?;

        let deadline = Instant::now() + timeout;
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).ok();
                    break stream;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if let Some(status) = self.child.try_wait().context("polling supervisor")? {
                        bail!(
                            "supervisor exited ({status}) before the guest USB/IP client connected"
                        );
                    }
                    if Instant::now() >= deadline {
                        bail!("timed out after {timeout:?} waiting for the guest USB/IP client");
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(e).context("accepting guest USB/IP connection"),
            }
        };

        // serve() blocks in the URB loop until the guest detaches; run it off the test thread.
        std::thread::spawn(move || {
            let backend = limina_usbip::MockBackend::new();
            if let Err(e) = limina_usbip::serve(stream, &backend) {
                eprintln!("usbip mock server ended: {e}");
            }
        });
        Ok(())
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

    /// The host port of this VM's SSH forward (`127.0.0.1:<port> → guest:22`) — the explicit
    /// [`GuestConfig::with_ssh_port`] or the ephemeral port the harness pre-allocated at boot.
    pub fn ssh_port(&self) -> u16 {
        self.ssh_port
    }

    /// Block until the guest's SSH server answers through gvproxy's inbound port-forward
    /// (`127.0.0.1:<ssh_port> → guest:22`, where `ssh_port` is this VM's [`GuestConfig::with_ssh_port`]
    /// or 2222), returning its banner (e.g. `SSH-2.0-OpenSSH_10.0`).
    /// Proves the inbound NAT path end-to-end (host → gvproxy forward → guest sshd) — what
    /// makes `ssh -p 2222 user@127.0.0.1` work. Requires [`GuestConfig::with_net`] and a guest
    /// running sshd. gvproxy listens on 2222 immediately but only yields a banner once it can
    /// dial the guest, so an empty/short read just means "not ready yet" — keep polling.
    pub fn wait_for_ssh_banner(&mut self, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        let forward = format!("{FORWARDED_SSH_HOST}:{}", self.ssh_port);
        let addr: std::net::SocketAddr = forward.parse().expect("valid forward addr");
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
                bail!("no SSH banner from {forward} within {timeout:?}");
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Run `remote_cmd` in the guest over SSH (through gvproxy's `127.0.0.1:<ssh_port> → guest:22`
    /// forward, this VM's [`GuestConfig::with_ssh_port`] or 2222) and return its stdout. Logs in as
    /// the in-image `claude` user via the host's default key (passwordless — see memory
    /// `limina-fedora-access`); host-key checks are disabled (the per-VM forward reuses
    /// `127.0.0.1:<ssh_port>` across boots). Requires [`GuestConfig::with_net`] and a booted guest
    /// running sshd — call [`Guest::wait_for_ssh_banner`] first. Errors if ssh exits non-zero
    /// (stderr is included in the message).
    pub fn ssh_exec(&self, remote_cmd: &str) -> Result<String> {
        self.ssh_exec_timeout(remote_cmd, SSH_CMD_TIMEOUT)
    }

    /// [`Guest::ssh_exec`] with an explicit deadline, for steps legitimately longer (or
    /// tighter) than the default cap. On expiry the local ssh is killed and the call fails;
    /// the remote command may keep running in the guest (fine for tests — the VM is torn
    /// down with the `Guest`).
    pub fn ssh_exec_timeout(&self, remote_cmd: &str, timeout: Duration) -> Result<String> {
        let port = self.ssh_port.to_string();
        let login = format!("claude@{FORWARDED_SSH_HOST}");
        let mut cmd = Command::new("ssh");
        cmd.args([
            "-p",
            &port,
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
            &login,
            remote_cmd,
        ]);
        let out = run_capped(cmd, timeout).with_context(|| {
            format!("ssh `{remote_cmd}` to the guest ({FORWARDED_SSH_HOST}:{port})")
        })?;
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

    /// The pid of the `limina` supervisor this guest is running under. Signal it to drive the
    /// real stop ladder (SIGTERM = the graceful path, a second one = force) while keeping the
    /// `Guest` alive to read its supervisor log — [`Guest::shutdown`] consumes the guest and
    /// takes its scratch dir, and with it the log, so a test that asserts on *how* the VM
    /// stopped needs this plus [`Guest::wait_for_exit`].
    pub fn supervisor_pid(&self) -> libc::pid_t {
        self.pid
    }

    /// The pid of the `limina-vmm` worker — the supervisor's child whose executable is
    /// [`vmm_bin`](GuestConfig::vmm_bin). With `--net` the supervisor also has gvproxy + a reaper
    /// child, so we match on the executable path rather than taking the first child.
    pub fn worker_pid(&self) -> Result<libc::pid_t> {
        // Size the buffer, then enumerate the supervisor's direct children.
        let cap = unsafe { libc::proc_listchildpids(self.pid, std::ptr::null_mut(), 0) };
        anyhow::ensure!(
            cap > 0,
            "supervisor {} reports no children yet (worker not up?)",
            self.pid
        );
        let mut pids = vec![0i32; cap as usize];
        let n = unsafe {
            libc::proc_listchildpids(
                self.pid,
                pids.as_mut_ptr() as *mut libc::c_void,
                std::mem::size_of_val(pids.as_slice()) as libc::c_int,
            )
        };
        anyhow::ensure!(
            n > 0,
            "proc_listchildpids(supervisor={}) returned {n}",
            self.pid
        );
        pids.truncate(n as usize);

        let want = self
            .vmm_bin
            .canonicalize()
            .unwrap_or_else(|_| self.vmm_bin.clone());
        for &pid in pids.iter().filter(|&&p| p > 0) {
            let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
            let len = unsafe {
                libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32)
            };
            if len <= 0 {
                continue; // process already gone, or path unreadable
            }
            buf.truncate(len as usize);
            let path = PathBuf::from(String::from_utf8_lossy(&buf).into_owned());
            let path = path.canonicalize().unwrap_or(path);
            if path == want {
                return Ok(pid);
            }
        }
        bail!(
            "no child of supervisor {} has path {want:?} (children: {pids:?})",
            self.pid
        )
    }

    /// Trigger a host-side VM snapshot/suspend (M9): send `SIGUSR1` to the running worker, which
    /// quiesces the vCPUs, serializes vCPU + GIC + RAM state to the `--snapshot-file`, and exits
    /// with the "snapshotted" disposition (126); the supervisor reports the VM suspended and stops
    /// (suspend = teardown, NOT relaunch). Requires [`GuestConfig::with_snapshot`]. Returns once the
    /// signal is sent — await the exit via [`Guest::wait_supervisor_exit`], then resume in a fresh
    /// boot with [`GuestConfig::restore_from`] the [`Guest::snapshot_path`].
    pub fn snapshot(&self) -> Result<()> {
        anyhow::ensure!(
            self.snapshot_path.is_some(),
            "no snapshot file configured (use GuestConfig::with_snapshot)"
        );
        let worker = self.worker_pid()?;
        let rc = unsafe { libc::kill(worker, libc::SIGUSR1) };
        anyhow::ensure!(
            rc == 0,
            "kill(worker={worker}, SIGUSR1) failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }

    /// Trigger the M9.2 **suspend bracket**: send `SIGTSTP` to the worker, which pulses the guest
    /// suspend button, polls the quiesce oracle, and — only if the guest fully s2idle-quiesces —
    /// snapshots + exits 126. On a guest that CANNOT quiesce (e.g. a virtiofs rootfs, whose
    /// `virtio_fs_freeze` returns -EOPNOTSUPP so s2idle aborts in-guest) the bracket times out, wakes
    /// the guest, and the worker keeps running — the fail-safe abort path this exercises. Requires
    /// [`GuestConfig::with_snapshot`] (which arms the bracket thread).
    pub fn suspend_bracket(&self) -> Result<()> {
        anyhow::ensure!(
            self.snapshot_path.is_some(),
            "no snapshot file configured (use GuestConfig::with_snapshot)"
        );
        let worker = self.worker_pid()?;
        let rc = unsafe { libc::kill(worker, libc::SIGTSTP) };
        anyhow::ensure!(
            rc == 0,
            "kill(worker={worker}, SIGTSTP) failed: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }

    /// Path to the VM snapshot file (inside the scratch dir), if [`GuestConfig::with_snapshot`]
    /// was set. Exists only after a [`Guest::snapshot`] has been taken.
    pub fn snapshot_path(&self) -> Option<&Path> {
        self.snapshot_path.as_deref()
    }

    /// The EFFECTIVE virtio-fs rootfs directory this guest serves — the per-guest APFS clone
    /// inside the scratch dir, NOT the shared tree named in [`GuestConfig`]. Tests that use the
    /// rootfs as a host↔guest channel must write their fixtures into the config tree BEFORE
    /// [`Guest::boot`] (the clone captures them) and read the guest's output HERE afterward.
    pub fn rootfs_dir(&self) -> PathBuf {
        self.scratch.join("rootfs")
    }

    /// The `limina-vmm` worker's `phys_footprint` in bytes — the page count macOS bills the process
    /// (Activity Monitor's "Memory", `proc_pid_rusage`'s `ri_phys_footprint`). The **worker** owns
    /// the guest-RAM `MAP_ANON` (the supervisor doesn't), so this is the number that must DROP when
    /// the guest returns memory through the balloon's free-page reporting / inflate. The go-to
    /// signal for M6 dynamic-memory tests.
    pub fn worker_phys_footprint(&self) -> Result<u64> {
        let worker = self.worker_pid()?;
        // SAFETY: zeroed POD; proc_pid_rusage fills it. The buffer arg is `rusage_info_t`
        // (`*mut c_void`) per Apple's API — we pass a pointer to our v2 struct.
        let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::proc_pid_rusage(
                worker,
                libc::RUSAGE_INFO_V2,
                &mut info as *mut libc::rusage_info_v2 as *mut libc::rusage_info_t,
            )
        };
        anyhow::ensure!(rc == 0, "proc_pid_rusage(worker={worker}) failed: rc={rc}");
        Ok(info.ri_phys_footprint)
    }

    /// scp options matching [`Guest::ssh_exec`]'s ssh invocation, targeting THIS VM's forward port
    /// (scp's port flag is `-P`, capitalized). Built per-guest so a multi-VM test copies to the
    /// right VM instead of always 2222.
    fn scp_opts(&self) -> Vec<String> {
        [
            "-P",
            &self.ssh_port.to_string(),
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "BatchMode=yes",
            "-o",
            "LogLevel=ERROR",
            "-q",
        ]
        .map(String::from)
        .to_vec()
    }

    /// Copy a local file into the guest at `remote_path` via scp (same forward and login
    /// as [`Guest::ssh_exec`]).
    pub fn scp_to_guest(&self, local: &Path, remote_path: &str) -> Result<()> {
        let mut cmd = Command::new("scp");
        cmd.args(self.scp_opts())
            .arg(local)
            .arg(format!("claude@{FORWARDED_SSH_HOST}:{remote_path}"));
        let out = run_capped(cmd, SSH_CMD_TIMEOUT).context("scp to the guest")?;
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
        let mut cmd = Command::new("scp");
        cmd.args(self.scp_opts())
            .arg(format!("claude@{FORWARDED_SSH_HOST}:{remote_glob}"))
            .arg(local_dir);
        let out = run_capped(cmd, SSH_CMD_TIMEOUT).context("scp from the guest")?;
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
    /// power off, climbing the rungs of its stop ladder), then wait up to `timeout` for the
    /// supervisor to exit. A guest that ignores every rung is *not* killed by that first
    /// signal — nothing in limina kills on a timer — so on overrun this escalates the way a
    /// user would, with a second SIGTERM (the force path), and only SIGKILLs if that fails too.
    pub fn shutdown(mut self, timeout: Duration) -> Result<Outcome> {
        let outcome = self.terminate(timeout)?;
        Ok(outcome)
    }

    /// Tear down a guest that **cannot power itself off** — one still in firmware/GRUB, or a
    /// stock image with neither `limina-agent` nor a reachable `qemu-guest-agent`. limina
    /// deliberately never kills such a guest on a timer (an ordinary stop is a request and
    /// nothing more), so asking politely would just burn `timeout` and come back `forced`.
    ///
    /// This asks the way a user would: the **force** path (two stop signals, as
    /// `limina stop --force` sends). The supervisor still does its own clean teardown, so a
    /// `forced: true` outcome from here means the *supervisor* is wedged — which keeps that
    /// flag meaningful as an oracle instead of merely restating what the guest cannot do.
    pub fn force_shutdown(mut self, timeout: Duration) -> Result<Outcome> {
        // `terminate` sends the second one, which is what makes the pair a force. The gap is
        // load-bearing: standard signals do not queue, so two SIGTERMs sent back to back
        // collapse into one delivery and the supervisor's counter never reaches 2 — the same
        // trap `vmlib::runtime::signal_stop` re-delivers around.
        unsafe {
            libc::kill(self.pid, libc::SIGTERM);
        }
        std::thread::sleep(Duration::from_millis(250));
        self.terminate(timeout)
    }

    /// Wait for the supervisor (and thus the VM) to exit **on its own** — for guests that
    /// self-terminate, e.g. one that powers itself off. Sends no signal while waiting, so the
    /// returned [`Outcome`] reflects how the guest ended (clean power-off → `code: Some(0)`; a
    /// crashed worker → non-zero / signal). On timeout it force-tears-down so no VM is left
    /// holding HVF, and the outcome is marked `forced`.
    pub fn wait_for_exit(mut self, timeout: Duration) -> Result<Outcome> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().context("polling supervisor")? {
                self.torn_down = true;
                return Ok(self.outcome_from(status, false));
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Didn't self-exit in time — force it down (no orphaned VM).
        self.terminate(Duration::from_secs(0))
    }

    /// Wait for the supervisor to exit **on its own**, returning its [`Outcome`], **without**
    /// consuming the `Guest` or removing its scratch dir. Use this (not [`Guest::wait_for_exit`],
    /// which consumes `self` and so drops the scratch) when the run leaves an artifact behind that
    /// a follow-up needs — e.g. a suspend's `--snapshot-file`, which a second `Guest` restores
    /// from: the file lives in *this* Guest's scratch, so this Guest must stay alive (in scope)
    /// until the restoring boot has read it. Reaps the child so `Drop` won't signal a dead pid; on
    /// timeout it errors and leaves teardown to `Drop`.
    pub fn wait_supervisor_exit(&mut self, timeout: Duration) -> Result<Outcome> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().context("polling supervisor")? {
                self.torn_down = true; // reaped; Drop must not signal a now-dead pid
                return Ok(self.outcome_from(status, false));
            }
            if Instant::now() >= deadline {
                bail!(
                    "supervisor did not exit within {timeout:?}.\n--- console tail ---\n{}",
                    tail(&self.console(), 40)
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
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

        // Overran. An ordinary stop never kills the guest — the supervisor asks and then waits
        // — so a guest that ignores every rung is still running here, by design. Escalate the
        // way a user would: a SECOND SIGTERM is the force path, and the supervisor then tears
        // its own worker and gateway down cleanly instead of being killed out from under them.
        unsafe {
            libc::kill(self.pid, libc::SIGTERM);
        }
        let forced_deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < forced_deadline {
            if let Some(status) = self.child.try_wait().context("polling supervisor")? {
                return Ok(self.outcome_from(status, true));
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        // Even force didn't take: the supervisor itself is wedged. Kill it and net any
        // orphaned worker, so a test run can never leave a live VM holding HVF.
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
        }
        self.kill_stray_workers();
        self.kill_stray_gateway();
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

    /// Best-effort: SIGKILL this supervisor's gvproxy on the forced path. After we SIGKILL the
    /// supervisor it can't run its own gateway teardown, so its gvproxy would be orphaned (the
    /// next `limina --net` would sweep it, but don't leave a stray after a test run). Targeted by
    /// the supervisor pid embedded in gvproxy's socket name (`limina-gvproxy-<pid>.sock`, see
    /// limina/src/gateway.rs), so it can't touch a concurrently-running VM's gateway.
    fn kill_stray_gateway(&self) {
        let _ = Command::new("pkill")
            .args(["-9", "-f", &format!("limina-gvproxy-{}.sock", self.pid)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        // Don't let a panicking test leak a live VM. Give the graceful path a short,
        // bounded window, then the forced path nets everything.
        let grace = Duration::from_secs(8);
        let _ = self.terminate(grace);
        // LIMINA_TEST_KEEP_SCRATCH=1 preserves the scratch dir (console/supervisor
        // logs, snapshot) for post-mortem — the logs are otherwise unrecoverable
        // when a multi-generation test fails past its first restore.
        if std::env::var_os("LIMINA_TEST_KEEP_SCRATCH").is_none() {
            let _ = fs::remove_dir_all(&self.scratch);
        }
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
/// Public: suspend/restore tests use it to preserve a suspended guest's disk past its
/// scratch teardown (the restore must resume against the exact suspended filesystem).
pub fn cow_clone(src: &Path, dst: &Path) -> Result<()> {
    let status = Command::new("cp")
        .arg("-c")
        .arg(src)
        .arg(dst)
        .status()
        .with_context(|| format!("running cp -c {src:?} {dst:?}"))?;
    anyhow::ensure!(status.success(), "cp -c {src:?} {dst:?} failed ({status})");
    Ok(())
}

/// APFS copy-on-write clone of a whole DIRECTORY (`cp -cR`): per-file clones, so a guest can
/// get its own writable virtio-fs rootfs tree without copying the bytes. `dst` must not exist.
fn cow_clone_dir(src: &Path, dst: &Path) -> Result<()> {
    let status = Command::new("cp")
        .arg("-cR")
        .arg(src)
        .arg(dst)
        .status()
        .with_context(|| format!("running cp -cR {src:?} {dst:?}"))?;
    anyhow::ensure!(status.success(), "cp -cR {src:?} {dst:?} failed ({status})");
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
/// Venus rings that stopped consuming, in the worker's own words.
///
/// A vkr ring that goes FATAL stops draining its command stream, and the guest's
/// `vn_ring_wait_seqno` then waits for a seqno that will never arrive. A compositor that hits
/// this while allocating a buffer parks there forever: the desktop freezes with every process
/// alive, nothing drawing, and no error anywhere in the guest. Before it was contained, one
/// stale reference in a restored context did exactly that to a whole synoik session.
///
/// The host notices the stall and says so; that line is the oracle. It is worth asserting
/// separately from any pixel comparison, because a frozen desktop still holds a *correct*
/// picture — the last frame presented before the ring died — so a landmark diff can pass on a
/// session that is comprehensively dead.
/// Quiet the things a desktop does on its own, so a test comparing two captures is
/// comparing its own workload rather than the machine's background life.
///
/// Two sources, both of which have produced a failure that looks exactly like a rendering
/// bug: PackageKit waking up to check for updates, which ends in a notification banner drawn
/// over the desktop, and the shell's own notification banners generally. Both appear at the
/// top of the screen, both are transient, and neither has anything to do with what these
/// tests measure.
///
/// The commands are best-effort on purpose. A guest without PackageKit, or with no GNOME
/// session behind the compositor under test, should not fail here -- the point is to remove
/// a disturbance where one exists, not to require it.
///
/// This does NOT stop a clock. A panel clock advancing between two captures is a *correct*
/// desktop, and a test whose landmarks cover the panel has to account for that itself.
pub fn quiesce_desktop(guest: &Guest) {
    let cmds = [
        // Stopping alone is not enough: the service is socket- and timer-activated, so the
        // next update check would start it again mid-test.
        "sudo systemctl stop packagekit.service packagekit-offline-update.service 2>/dev/null || true",
        "sudo systemctl mask packagekit.service 2>/dev/null || true",
        "sudo systemctl stop dnf-makecache.timer 2>/dev/null || true",
        // Do Not Disturb, for a session that has a GNOME shell to listen.
        "export XDG_RUNTIME_DIR=/run/user/1000          DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus;          gsettings set org.gnome.desktop.notifications show-banners false 2>/dev/null || true",
        "export XDG_RUNTIME_DIR=/run/user/1000          DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus;          gsettings set org.gnome.desktop.screensaver idle-activation-enabled false 2>/dev/null || true",
        "export XDG_RUNTIME_DIR=/run/user/1000          DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus;          gsettings set org.gnome.desktop.session idle-delay 0 2>/dev/null || true",
    ];
    for c in cmds {
        let _ = guest.ssh_exec(c);
    }
}

pub fn ring_stalls(log: &str) -> Vec<&str> {
    log.lines()
        .filter(|l| l.contains("wait_ring_seqno STUCK"))
        .collect()
}

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

/// Like [`set_pasteboard_text`], but with a deliberate delay between `clearContents` and
/// `setString` — a magnified version of the window every real writer opens (AppKit bumps
/// `changeCount` on the clear, NOT on the subsequent write). Lets a test force the
/// clipboard poller to observe the pasteboard mid-write deterministically.
pub fn set_pasteboard_text_slowly(name: &str, text: &str, midwrite_delay: Duration) {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    use objc2_foundation::NSString;
    unsafe {
        let pb = NSPasteboard::pasteboardWithName(&NSString::from_str(name));
        pb.clearContents();
        std::thread::sleep(midwrite_delay);
        pb.setString_forType(&NSString::from_str(text), NSPasteboardTypeString);
    }
}

#[cfg(test)]
mod run_capped_tests {
    use super::*;

    #[test]
    fn quick_command_returns_output() {
        let mut cmd = Command::new("/bin/echo");
        cmd.arg("hello");
        let out = run_capped(cmd, Duration::from_secs(10)).expect("echo runs");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
    }

    #[test]
    fn wedged_command_is_killed_at_the_deadline() {
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("3600");
        let start = Instant::now();
        let err = run_capped(cmd, Duration::from_millis(300)).expect_err("must time out");
        assert!(start.elapsed() < Duration::from_secs(10), "killed promptly");
        assert!(err.to_string().contains("did not finish"), "{err}");
    }

    #[test]
    fn failing_command_reports_status_not_timeout() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "echo oops >&2; exit 3"]);
        let out = run_capped(cmd, Duration::from_secs(10)).expect("sh runs to completion");
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), "oops");
    }
}
