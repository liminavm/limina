// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Where the worker's Mesa keeps its disk cache (`--gpu-shader-cache`).
//!
//! A classic virgl context compiles every guest shader twice: once in the guest, and again on the
//! host, where vrend's GLSL goes through Mesa's whole GLSL front end and then KosmicKrisp. Cold,
//! that is seconds of a stalled device for a single GTK dialog; Mesa's disk cache turns the second
//! time into a lookup. Left at its default, the cache is `~/.cache/mesa_shader_cache`, shared by
//! every process on the host using Mesa, capped at 1 GiB and orphaned on every Mesa rebuild, so a
//! guest's shaders are routinely evicted by someone else's. One directory per VM keeps them.

use std::path::Path;

/// Mesa's cap for one VM's cache, the size its default allows a whole host.
const MAX_SIZE: &str = "1G";

/// The Mesa environment putting the disk cache in `dir`, skipping any variable the caller already
/// set: an explicit setting, such as a test disabling the cache, always wins.
pub fn env(dir: &Path, already_set: impl Fn(&str) -> bool) -> Vec<(&'static str, String)> {
    [
        ("MESA_SHADER_CACHE_DIR", dir.display().to_string()),
        ("MESA_SHADER_CACHE_MAX_SIZE", MAX_SIZE.to_string()),
    ]
    .into_iter()
    .filter(|(k, _)| !already_set(k))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_mesa_at_the_directory_with_a_cap() {
        assert_eq!(
            env(Path::new("/c/shader-cache/adhoc"), |_| false),
            vec![
                ("MESA_SHADER_CACHE_DIR", "/c/shader-cache/adhoc".to_string()),
                ("MESA_SHADER_CACHE_MAX_SIZE", "1G".to_string()),
            ]
        );
    }

    #[test]
    fn an_explicit_setting_wins() {
        let set = env(Path::new("/c"), |k| k == "MESA_SHADER_CACHE_DIR");
        assert_eq!(set, vec![("MESA_SHADER_CACHE_MAX_SIZE", "1G".to_string())]);
    }
}
