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
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

/// Stable location of the krunkit EFI firmware blob (an EDK2 `.fd`). Overridable with
/// `LIMINA_FIRMWARE`. This is the same firmware the M1 boot spikes used.
const DEFAULT_FIRMWARE: &str = "/opt/homebrew/share/krunkit/KRUN_EFI.silent.fd";

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
            Err(_) => repo_root().join("Fedora-Workstation-43.raw"),
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
        })
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
        })
    }

    /// Attach a virtio-gpu display at `width`x`height` and capture presented frames. Use
    /// [`Guest::display_capture_path`]/[`Guest::wait_for_capture`] after boot to read the
    /// captured PNG. The L1 init draws a deterministic pattern to `/dev/fb0` and forces a
    /// flush, so the present is explicit rather than relying on fbcon's deferred I/O.
    pub fn with_display(mut self, width: u32, height: u32) -> GuestConfig {
        self.display = Some(DisplayCfg { width, height });
        self
    }

    /// Enable the guest vsock agent on `port`: the host listens on a UNIX socket and the
    /// kernel cmdline gets `limina.agent_port=<port>` so the init runs the agent. Kernel
    /// boot only (the L1 guest).
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
            .arg("--console")
            .arg(&console_path)
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
                cmd.arg("--firmware").arg(firmware).arg("--disk").arg(disk);
                if *read_only {
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
        }
        // Display: capture the scanout into the scratch dir (auto-cleaned on Drop).
        let capture_png = match &cfg.display {
            Some(d) => {
                let png = scratch.join("scanout.png");
                cmd.arg("--display-capture")
                    .arg(&png)
                    .arg("--display-size")
                    .arg(format!("{}x{}", d.width, d.height));
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

        // Let supervisor/worker logs flow to the test's stderr (visible with --nocapture).
        cmd.stdin(Stdio::null());

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
            torn_down: false,
        })
    }

    /// Path to the captured-scanout PNG (inside the scratch dir), if a display was
    /// configured. The file appears once the guest presents its first frame and is
    /// overwritten with each subsequent frame (latest wins). Removed on Drop.
    pub fn display_capture_path(&self) -> Option<&Path> {
        self.capture_png.as_deref()
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
    /// returning a line-protocol channel. Errors if no vsock was configured, the guest
    /// never connects within `timeout`, or the supervisor exits first.
    pub fn vsock_accept(&mut self, timeout: Duration) -> Result<VsockConn> {
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
                    return Ok(VsockConn {
                        reader: BufReader::new(stream),
                    });
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
        let status = self.child.wait().context("waiting on supervisor after SIGKILL")?;
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
pub struct VsockConn {
    reader: BufReader<UnixStream>,
}

impl VsockConn {
    /// Read one newline-terminated line from the agent (trailing newline trimmed),
    /// honoring `timeout`. Errors on timeout or EOF.
    pub fn read_line(&mut self, timeout: Duration) -> Result<String> {
        self.reader
            .get_ref()
            .set_read_timeout(Some(timeout))
            .context("set_read_timeout")?;
        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .context("reading from guest agent (timeout?)")?;
        anyhow::ensure!(n > 0, "guest agent closed the connection (EOF)");
        Ok(line.trim_end().to_string())
    }

    /// Send a command line to the agent (a newline is appended).
    pub fn write_line(&mut self, line: &str) -> Result<()> {
        let stream = self.reader.get_mut();
        stream
            .write_all(line.as_bytes())
            .and_then(|_| stream.write_all(b"\n"))
            .context("writing to guest agent")
    }
}

/// Last `n` lines of `s`.
fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
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
