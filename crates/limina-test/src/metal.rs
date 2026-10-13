// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! What a guest with limina's vendor GPU drivers is running on.
//!
//! The metal test image carries the vendor drivers as a guest gets them
//! (`scripts/provision/deliver-metal-transport.sh`, docs/design/metal-transport-delivery.md §1):
//! a private Mesa under `/opt/limina/<wire>/` that the system's loaders find, and the extended
//! virtio-gpu module through DKMS. Nothing in the environment selects them. Booted with the
//! transport, gnome-shell and its clients take them; booted without, every entry point declines
//! and the guest is stock.
//!
//! Every oracle that says which side won has a look-alike on the other side, so [`Stack`] reads
//! each one where the two differ:
//!
//! - **GL.** Stock virgl on this host reports `virgl (zink Vulkan 1.4(Apple M1 Max
//!   (MESA_KOSMICKRISP)))`: the host's zink on KosmicKrisp, behind vrend. A check for
//!   "KosmicKrisp" passes on both; the vendor driver's renderer is zink *itself*.
//! - **Vulkan.** venus names the same GPU (`Virtio-GPU Venus (Apple M1 Max)`), so a device name
//!   containing "Apple M1 Max" passes on both; the driver name does not.
//! - **The compositor.** gnome-shell maps the system's libgallium on both sides, since glvnd
//!   initialises every vendor; only the transport's side also maps ours.

use anyhow::{Context, Result};

use crate::Guest;

/// Where the vendor drivers for this build's protocol live in the guest.
pub fn prefix() -> String {
    format!("/opt/limina/{}", limina_gpu_wire::VERSION)
}

/// The vendor Vulkan driver's ICD, where the installer links it into the loader's path.
pub fn icd() -> String {
    format!("/etc/vulkan/icd.d/limina-{}.json", limina_gpu_wire::VERSION)
}

/// Runs what follows in the seated user's session.
pub const SESSION: &str = "env XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 \
                           DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus";

/// What one boot of the metal image is running on, as read from inside it.
#[derive(Debug, Clone)]
pub struct Stack {
    /// The file `virtio_gpu` resolves to.
    pub module: String,
    /// Whether the module found the limina extension on the device.
    pub extension: bool,
    /// The GPU libraries gnome-shell has mapped (ours and the system's).
    pub shell_maps: Vec<String>,
    /// A GL client's `GL_RENDERER` in the seated session.
    pub gl_renderer: String,
    /// Every Vulkan driver the loader enumerates a device for, by driver name.
    pub vk_drivers: Vec<String>,
}

impl Stack {
    /// Read it. Needs gnome-shell up.
    pub fn probe(g: &Guest) -> Result<Stack> {
        let module = g.ssh_exec("modinfo -n virtio_gpu")?.trim().to_string();
        let extension = g
            .ssh_exec("sudo dmesg | grep -c 'limina extension:' || true")?
            .trim()
            != "0";
        let shell_maps = g
            .ssh_exec(
                "p=$(pgrep -u 1000 -xo gnome-shell) && grep -o \
                 '/[^ ]*\\(libEGL\\|libgallium\\|gbm\\|vulkan\\)[^ ]*' /proc/$p/maps | sort -u",
            )
            .context("reading gnome-shell's maps")?
            .lines()
            .map(str::to_string)
            .collect();
        // glmark2 prints the context's renderer before its first scene; one short scene is enough.
        let gl = g.ssh_exec(&format!(
            "{SESSION} timeout 60 glmark2-es2-wayland -b build:duration=1 --size 64x64 2>&1 \
             | grep GL_RENDERER || true"
        ))?;
        let gl_renderer = gl
            .split_once("GL_RENDERER:")
            .map(|(_, r)| r.trim().to_string())
            .unwrap_or_default();
        let vk_drivers = g
            .ssh_exec("timeout 30 vulkaninfo --summary 2>/dev/null | grep driverName || true")?
            .lines()
            .filter_map(|l| l.split_once('=').map(|(_, d)| d.trim().to_string()))
            .collect();
        Ok(Stack {
            module,
            extension,
            shell_maps,
            gl_renderer,
            vk_drivers,
        })
    }

    /// The module is the one DKMS built, not the kernel's own.
    pub fn module_is_ours(&self) -> bool {
        self.module.contains("/extra/virtio-gpu.ko")
    }

    /// gnome-shell runs on the vendor drivers: our EGL vendor and our GBM backend.
    pub fn shell_on_vendor_drivers(&self) -> bool {
        let p = prefix();
        let has = |lib: &str| {
            self.shell_maps
                .iter()
                .any(|m| m == &format!("{p}/lib/{lib}"))
        };
        has("libEGL_limina.so.0.0.0") && has("gbm/dri_gbm.so")
    }

    /// gnome-shell maps nothing of ours.
    pub fn shell_on_stock_drivers(&self) -> bool {
        !self
            .shell_maps
            .iter()
            .any(|m| m.starts_with("/opt/limina/"))
    }

    /// GL clients get the vendor driver: zink on KosmicKrisp in the guest, not virgl.
    pub fn gl_on_vendor_driver(&self) -> bool {
        self.gl_renderer.starts_with("zink ") && self.gl_renderer.contains("KOSMICKRISP")
    }

    /// GL clients get stock virgl.
    pub fn gl_on_virgl(&self) -> bool {
        self.gl_renderer.starts_with("virgl (")
    }

    /// The vendor Vulkan driver enumerates a device.
    pub fn vk_has_kosmickrisp(&self) -> bool {
        self.vk_drivers.iter().any(|d| d == "KosmicKrisp")
    }
}
