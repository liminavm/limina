// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva

//! Raw libgbm, for M3c: a client buffer allocated as a **classic vrend resource** instead of a
//! venus one.
//!
//! Why this exists at all. The venus↔venus import M3a/M3b measure is *symmetric* — both the
//! owning reference and the borrowed `+1` sit on the venus side, so one sweep frees both. The
//! case `spikes/venus-churn-retention/buffer-lifetime-matrix.md` §3 is worried about is
//! **asymmetric**: vrend *owns* the IOSurface (allocated in `vrend_resource_iosurface_init`,
//! freed at resource destroy) while venus holds only a borrowed `+1` in `mem->imported_iosurface`.
//! If the venus side ever dies without running `vkr_device_memory_release`, the surface outlives
//! **both** renderers with nobody left to free it. Reaching that needs a gbm-allocated client
//! buffer; a Vulkan-allocated one cannot express it.
//!
//! It is also the *realistic* shape. Since the 2026-08-04 `MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu`
//! flip, an ordinary GL client's window buffer is exactly this: gbm → classic vrend.
//!
//! **Loaded with `dlopen`, not linked.** testcomp cross-builds in the `limina-build:fc43`
//! container; linking libgbm would put a `-devel` package in that image for eight symbols, and a
//! link-time resolution is one more thing that can silently bind to the wrong library. ctypes is
//! how `crates/limina-test/guest/vkclassicimport.py` — the proven oracle for this exact import
//! path — reaches the same API, and this module deliberately mirrors its call sequence.

use anyhow::{Context, Result};
use std::ffi::{c_char, c_int, c_void, CStr};

/// `GBM_BO_USE_RENDERING` **without** `GBM_BO_USE_SCANOUT`, which is the whole point: a
/// SCANOUT-bound buffer was IOSurface-backed by vrend from 2026-08-05, but the far more numerous
/// class — a *client's* window buffer — only became importable on 2026-08-06 with the
/// `VIRGL_BIND_SHARED` gate. Non-scanout IS the class under test, so adding SCANOUT here would
/// quietly test the easier path. Values verified against `vkclassicimport.py:63-65`.
const GBM_BO_USE_RENDERING: u32 = 1 << 2;
const GBM_BO_TRANSFER_WRITE: u32 = 1 << 1;

/// The libgbm entry points, resolved once. Only what the allocation path needs.
pub struct Gbm {
    _handle: *mut c_void,
    create_device: unsafe extern "C" fn(c_int) -> *mut c_void,
    device_destroy: unsafe extern "C" fn(*mut c_void),
    bo_create: unsafe extern "C" fn(*mut c_void, u32, u32, u32, u32) -> *mut c_void,
    bo_destroy: unsafe extern "C" fn(*mut c_void),
    bo_get_fd: unsafe extern "C" fn(*mut c_void) -> c_int,
    bo_get_stride: unsafe extern "C" fn(*mut c_void) -> u32,
    bo_get_offset: unsafe extern "C" fn(*mut c_void, c_int) -> u32,
    bo_get_modifier: unsafe extern "C" fn(*mut c_void) -> u64,
    bo_get_plane_count: unsafe extern "C" fn(*mut c_void) -> c_int,
    bo_map: unsafe extern "C" fn(
        *mut c_void,
        u32,
        u32,
        u32,
        u32,
        u32,
        *mut u32,
        *mut *mut c_void,
    ) -> *mut c_void,
    bo_unmap: unsafe extern "C" fn(*mut c_void, *mut c_void),
    device: *mut c_void,
    node_fd: c_int,
}

/// One gbm buffer object, plus the layout the Wayland `linux-dmabuf` `add` request needs.
pub struct Bo {
    bo: *mut c_void,
    pub fd: c_int,
    pub stride: u32,
    pub offset: u32,
    pub modifier: u64,
}

macro_rules! sym {
    ($handle:expr, $name:literal) => {{
        let name = concat!($name, "\0");
        // SAFETY: `name` is NUL-terminated by construction; `$handle` came from a successful
        // dlopen and stays open for the process lifetime.
        let p = unsafe { libc::dlsym($handle, name.as_ptr() as *const c_char) };
        anyhow::ensure!(!p.is_null(), "libgbm is missing {}", $name);
        // SAFETY: the signature is the one in gbm.h; a mismatch here is a build-time authoring
        // error, not something the running program can detect.
        unsafe { std::mem::transmute(p) }
    }};
}

impl Gbm {
    /// Open the first render node and resolve libgbm against it.
    ///
    /// A **render** node rather than `card0`: a client has no business holding the primary node
    /// (it would need DRM master to be useful), and it is the node a real GL client's EGL/gbm
    /// stack opens. `vkclassicimport.py` picks it the same way.
    pub fn open() -> Result<Self> {
        // SAFETY: a NUL-terminated literal; RTLD_NOW surfaces a missing symbol here rather than
        // at the first call.
        let handle = unsafe { libc::dlopen(c"libgbm.so.1".as_ptr(), libc::RTLD_NOW) };
        anyhow::ensure!(!handle.is_null(), "dlopen libgbm.so.1: {}", dlerror());

        let mut node = None;
        for entry in std::fs::read_dir("/dev/dri").context("listing /dev/dri")? {
            let name = entry?.file_name();
            let name = name.to_string_lossy().into_owned();
            if name.starts_with("renderD") {
                // Sorted pick, so a multi-node guest is deterministic rather than
                // readdir-order-dependent.
                if node.as_ref().is_none_or(|cur: &String| name < *cur) {
                    node = Some(name);
                }
            }
        }
        let node = node.context("no renderD* node in /dev/dri")?;
        let path = format!("/dev/dri/{node}");

        let cpath = std::ffi::CString::new(path.clone()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        let node_fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        anyhow::ensure!(
            node_fd >= 0,
            "opening {path}: {}",
            std::io::Error::last_os_error()
        );

        let create_device: unsafe extern "C" fn(c_int) -> *mut c_void =
            sym!(handle, "gbm_create_device");
        // SAFETY: `node_fd` is an open DRM render node, which is what gbm_create_device wants.
        let device = unsafe { create_device(node_fd) };
        if device.is_null() {
            // SAFETY: an fd we opened and have not closed.
            unsafe { libc::close(node_fd) };
            anyhow::bail!("gbm_create_device on {path} failed");
        }

        log::info!("gbm device on {path}");
        Ok(Self {
            _handle: handle,
            create_device,
            device_destroy: sym!(handle, "gbm_device_destroy"),
            bo_create: sym!(handle, "gbm_bo_create"),
            bo_destroy: sym!(handle, "gbm_bo_destroy"),
            bo_get_fd: sym!(handle, "gbm_bo_get_fd"),
            bo_get_stride: sym!(handle, "gbm_bo_get_stride"),
            bo_get_offset: sym!(handle, "gbm_bo_get_offset"),
            bo_get_modifier: sym!(handle, "gbm_bo_get_modifier"),
            bo_get_plane_count: sym!(handle, "gbm_bo_get_plane_count"),
            bo_map: sym!(handle, "gbm_bo_map"),
            bo_unmap: sym!(handle, "gbm_bo_unmap"),
            device,
            node_fd,
        })
    }

    /// Allocate a rendering-only buffer and export its dmabuf fd.
    pub fn create(&self, width: u32, height: u32, fourcc: u32) -> Result<Bo> {
        // SAFETY: `self.device` is a live gbm_device; the rest are plain scalars.
        let bo =
            unsafe { (self.bo_create)(self.device, width, height, fourcc, GBM_BO_USE_RENDERING) };
        anyhow::ensure!(!bo.is_null(), "gbm_bo_create {width}x{height} failed");

        // SAFETY: `bo` is non-null and live for each of these.
        let planes = unsafe { (self.bo_get_plane_count)(bo) };
        if planes != 1 {
            // SAFETY: `bo` is live and not used again.
            unsafe { (self.bo_destroy)(bo) };
            anyhow::bail!("gbm gave {planes} planes; this vehicle handles single-plane only");
        }

        // SAFETY: `bo` is live.
        let fd = unsafe { (self.bo_get_fd)(bo) };
        if fd < 0 {
            // SAFETY: `bo` is live and not used again.
            unsafe { (self.bo_destroy)(bo) };
            anyhow::bail!("gbm_bo_get_fd: {}", std::io::Error::last_os_error());
        }

        // SAFETY: `bo` is live for all three.
        let (stride, offset, modifier) = unsafe {
            (
                (self.bo_get_stride)(bo),
                (self.bo_get_offset)(bo, 0),
                (self.bo_get_modifier)(bo),
            )
        };
        Ok(Bo {
            bo,
            fd,
            stride,
            offset,
            modifier,
        })
    }

    /// Paint through a CPU map.
    ///
    /// Unlike the venus arm — which paints on the GPU because the image is device-local — a
    /// classic gbm buffer's whole point here is the *host* allocation behind it, and `gbm_bo_map`
    /// is what a client uses when it has pixels rather than a render pass. It also matches
    /// `vkclassicimport.py`'s ALIAS check, which writes through exactly this path.
    pub fn paint(&self, bo: &Bo, width: u32, height: u32) -> Result<()> {
        let mut map_stride: u32 = 0;
        let mut map_data: *mut c_void = std::ptr::null_mut();
        // SAFETY: `bo.bo` is live; the two out-params are valid for the call.
        let ptr = unsafe {
            (self.bo_map)(
                bo.bo,
                0,
                0,
                width,
                height,
                GBM_BO_TRANSFER_WRITE,
                &mut map_stride,
                &mut map_data,
            )
        };
        anyhow::ensure!(!ptr.is_null(), "gbm_bo_map failed");

        let len = map_stride as usize * height as usize;
        // SAFETY: gbm just handed us `len` writable bytes at `ptr` (its own stride, which need
        // not equal the export stride — using the wrong one is how a map-write lands skewed).
        let buf = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, len) };
        crate::client::paint(buf, width, height, map_stride);

        // SAFETY: `map_data` is the cookie the matching map returned.
        unsafe { (self.bo_unmap)(bo.bo, map_data) };
        Ok(())
    }

    /// Destroy a buffer object. The exported fd is the caller's to close — the compositor may
    /// still be holding its own reference to the same storage, which is the ordering
    /// `buffer-lifetime-matrix.md` §4 lists as *not a bug*.
    pub fn destroy(&self, bo: Bo) {
        // SAFETY: `bo` is consumed, so this is the last use of the handle.
        unsafe { (self.bo_destroy)(bo.bo) };
    }
}

impl Drop for Gbm {
    fn drop(&mut self) {
        // SAFETY: both are live and not used again. The library handle itself is deliberately
        // left open: unloading it while any gbm-owned thread state survives buys nothing here.
        unsafe {
            (self.device_destroy)(self.device);
            libc::close(self.node_fd);
        }
        let _ = self.create_device;
    }
}

fn dlerror() -> String {
    // SAFETY: dlerror returns a NUL-terminated string or NULL.
    let p = unsafe { libc::dlerror() };
    if p.is_null() {
        return "unknown error".into();
    }
    // SAFETY: non-null and NUL-terminated per dlerror's contract.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}
