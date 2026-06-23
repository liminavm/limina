// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Bundle-relative venus/GL environment for the worker.
//!
//! When limina runs from an assembled `limina.app`, the host venus/GL dylib closure lives
//! under `Contents/Frameworks` and the KosmicKrisp ICD under `Contents/Resources/vulkan`
//! (see `scripts/build-app.sh`). The worker (`limina-vmm`, a sibling of the supervisor in
//! `Contents/MacOS`) then needs the Vulkan loader pointed at that ICD and Mesa's zink-on-KK
//! selectors set — the same variables `spikes/venus-draw-probe/boot-seated-kk.sh` exports in
//! dev, but resolved relative to the bundle instead of `/Volumes/mesa-cs`.
//!
//! The linked closure (virglrenderer → epoxy → the Vulkan loader, and gallium → LLVM) is
//! already relocated to `@rpath` by the build script, so dyld resolves it without help. This
//! env only covers what's loaded *by path at runtime*: the KK ICD (Vulkan loader) and the
//! zink-on-KK Mesa driver, plus epoxy's bare-soname `libEGL`/`libGLESv2` dlopens
//! (`DYLD_FALLBACK_LIBRARY_PATH`). The worker carries the
//! `com.apple.security.cs.allow-dyld-environment-variables` entitlement so the DYLD var
//! survives exec.

use std::path::Path;

/// The bundle-relative venus env, or `None` when not running from a bundle.
///
/// `supervisor_exe_dir` is the directory of the running `limina` binary
/// (`…/limina.app/Contents/MacOS`); the worker is its sibling. `exists` probes the
/// filesystem (injected for testing). Returning `None` for a non-bundle layout leaves the
/// inherited dev env (from `boot-seated-kk.sh`) untouched.
pub fn bundle_venus_env(
    supervisor_exe_dir: &Path,
    exists: impl Fn(&Path) -> bool,
) -> Option<Vec<(String, String)>> {
    // Contents/MacOS/limina → Contents/Resources/vulkan/kosmickrisp_icd.json
    let icd = supervisor_exe_dir.join("../Resources/vulkan/kosmickrisp_icd.json");
    if !exists(&icd) {
        return None;
    }
    let frameworks = supervisor_exe_dir.join("../Frameworks");
    let icd_s = icd.to_string_lossy().into_owned();
    let fw_s = frameworks.to_string_lossy().into_owned();
    Some(vec![
        // The Vulkan loader → the relative-path KK ICD → the bundled KK driver.
        ("VK_ICD_FILENAMES".to_string(), icd_s.clone()),
        ("VK_DRIVER_FILES".to_string(), icd_s),
        // epoxy/zink dlopen libEGL/libGLESv2 by bare soname; Mesa's dri driver is loaded
        // by path. Point both at Frameworks.
        ("DYLD_FALLBACK_LIBRARY_PATH".to_string(), fw_s.clone()),
        ("LIBGL_DRIVERS_PATH".to_string(), fw_s),
        // Select the zink-on-KK host GL path (matches boot-seated-kk.sh).
        (
            "MESA_LOADER_DRIVER_OVERRIDE".to_string(),
            "zink".to_string(),
        ),
        ("GALLIUM_DRIVER".to_string(), "zink".to_string()),
        ("EGL_PLATFORM".to_string(), "surfaceless".to_string()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;

    #[test]
    fn none_outside_a_bundle() {
        // A dev/cargo layout: no Resources/vulkan/icd next to the exe.
        let dir = Path::new("/repo/target/debug");
        assert!(bundle_venus_env(dir, |_| false).is_none());
    }

    #[test]
    fn sets_bundle_relative_env_when_icd_present() {
        let dir = Path::new("/Apps/limina.app/Contents/MacOS");
        let icd = dir.join("../Resources/vulkan/kosmickrisp_icd.json");
        let present: HashSet<PathBuf> = [icd.clone()].into_iter().collect();
        let env = bundle_venus_env(dir, |p| present.contains(p)).expect("bundle env");

        let get = |k: &str| {
            env.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(get("VK_ICD_FILENAMES"), icd.to_string_lossy());
        assert_eq!(get("VK_DRIVER_FILES"), icd.to_string_lossy());
        assert_eq!(get("GALLIUM_DRIVER"), "zink");
        assert_eq!(get("MESA_LOADER_DRIVER_OVERRIDE"), "zink");
        assert_eq!(get("EGL_PLATFORM"), "surfaceless");
        let fw = dir.join("../Frameworks");
        assert_eq!(get("DYLD_FALLBACK_LIBRARY_PATH"), fw.to_string_lossy());
        assert_eq!(get("LIBGL_DRIVERS_PATH"), fw.to_string_lossy());
    }
}
