// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! The host trackpad's physical surface, which the guest touchpad is sized to.
//!
//! libinput reads a touchpad's size once, when the device is probed, and derives its scroll
//! distances, palm zones and gesture thresholds from it — so the guest device should report the
//! real trackpad's millimetres. AppKit only reveals a trackpad's size in points, and only once a
//! touch arrives; the private `MultitouchSupport.framework` answers it up front, in the 0.01 mm
//! units the device reports in (`spikes/mt-raw-capture/RESULTS.md`: 12480 × 7680 on the M1 Max
//! built-in). It is bound with `dlopen`, so a vanished symbol degrades to the built-in size
//! rather than failing to launch.

use std::ffi::{c_char, c_void};
use std::sync::OnceLock;

use limina_input::touchpad::TouchpadGeometry;

const FRAMEWORK: &[u8] =
    b"/System/Library/PrivateFrameworks/MultitouchSupport.framework/MultitouchSupport\0";

/// The default trackpad's surface (the built-in one when there is one), or `None` if the
/// framework, a symbol or the device is missing.
fn query() -> Option<TouchpadGeometry> {
    type CreateDefault = unsafe extern "C" fn() -> *mut c_void;
    type SurfaceDimensions = unsafe extern "C" fn(*mut c_void, *mut i32, *mut i32) -> i32;
    type Release = unsafe extern "C" fn(*mut c_void);

    // SAFETY: plain dlopen/dlsym on a system framework; the symbols' signatures are the ones
    // the spike measured, and a missing one returns None before any call.
    unsafe {
        let fw = libc::dlopen(FRAMEWORK.as_ptr() as *const c_char, libc::RTLD_NOW);
        if fw.is_null() {
            return None;
        }
        let sym = |name: &[u8]| libc::dlsym(fw, name.as_ptr() as *const c_char);
        let create = sym(b"MTDeviceCreateDefault\0");
        let dims = sym(b"MTDeviceGetSensorSurfaceDimensions\0");
        let release = sym(b"MTDeviceRelease\0");
        if create.is_null() || dims.is_null() || release.is_null() {
            return None;
        }
        let create: CreateDefault = std::mem::transmute(create);
        let dims: SurfaceDimensions = std::mem::transmute(dims);
        let release: Release = std::mem::transmute(release);

        let dev = create();
        if dev.is_null() {
            return None;
        }
        let (mut w, mut h) = (0i32, 0i32);
        dims(dev, &mut w, &mut h);
        release(dev);
        (w > 0 && h > 0).then_some(TouchpadGeometry {
            width: w as u32,
            height: h as u32,
        })
    }
}

/// The surface to advertise to the guest: the host trackpad's, else the built-in default.
/// Read once per process, so the guest device and the host's position scaling can never
/// disagree about it.
pub fn geometry() -> TouchpadGeometry {
    static GEOMETRY: OnceLock<TouchpadGeometry> = OnceLock::new();
    *GEOMETRY.get_or_init(read)
}

fn read() -> TouchpadGeometry {
    match query() {
        Some(g) => {
            log::info!(
                "touchpad: host trackpad surface {:.1} x {:.1} mm",
                f64::from(g.width) / 100.0,
                f64::from(g.height) / 100.0
            );
            g
        }
        None => {
            let g = TouchpadGeometry::default();
            log::info!(
                "touchpad: no host trackpad size available; advertising the built-in {}",
                g.to_arg()
            );
            g
        }
    }
}
