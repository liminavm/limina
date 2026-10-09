//! The key the renderer signs the guest's pipeline-cache data with.
//!
//! virglrs hands a guest `vkGetPipelineCacheData` output with an HMAC tag, and forwards a guest's
//! `pInitialData` to the driver only when the tag verifies under the same key, so a guest cannot
//! feed the host driver's cache parser bytes the host did not write. A key that lives only as long
//! as the worker makes every boot's caches cold; one kept in a file and passed on every launch
//! keeps them warm. It is host-only: never logged, never shown to the guest.

use std::fs::OpenOptions;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// The key's length in bytes (virglrs's `PipelineCacheKey::LEN`).
pub const LEN: usize = 32;

/// Read the key in `path`, or make one there from the OS random source when there is none.
///
/// A new key is written with mode 0600 and `create_new`, so two workers racing on one path end
/// up with the same key rather than each keeping its own. A file that is not exactly [`LEN`]
/// bytes is an error, not something to replace: a key that changes makes every saved cache cold.
pub fn load_or_create(path: &Path) -> Result<[u8; LEN]> {
    match read(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        Ok(bytes) => return exact(path, bytes),
    }
    let mut key = [0u8; LEN];
    getrandom::fill(&mut key)
        .map_err(|e| anyhow::anyhow!("no OS randomness for {}: {e}", path.display()))?;
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut f) => {
            f.write_all(&key)
                .and_then(|()| f.sync_all())
                .with_context(|| format!("writing the pipeline-cache key {}", path.display()))?;
            Ok(key)
        }
        // Another worker made it between the read and the create: use theirs.
        Err(e) if e.kind() == ErrorKind::AlreadyExists => exact(
            path,
            read(path).with_context(|| format!("reading {}", path.display()))?,
        ),
        Err(e) => {
            Err(e).with_context(|| format!("creating the pipeline-cache key {}", path.display()))
        }
    }
}

fn read(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(LEN + 1);
    std::fs::File::open(path)?
        .take(LEN as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn exact(path: &Path, bytes: Vec<u8>) -> Result<[u8; LEN]> {
    match <[u8; LEN]>::try_from(bytes.as_slice()) {
        Ok(key) => Ok(key),
        Err(_) => bail!(
            "{} is not a pipeline-cache key: {} bytes where {LEN} were expected",
            path.display(),
            bytes.len()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    struct Dir(PathBuf);
    impl Dir {
        fn new(tag: &str) -> Dir {
            let d = std::env::temp_dir().join(format!("limina-pck-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Dir(d)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_missing_key_is_made_private_and_then_kept() {
        let d = Dir::new("make");
        let p = d.0.join("pipeline-cache.key");
        let first = load_or_create(&p).unwrap();
        let meta = std::fs::metadata(&p).unwrap();
        assert_eq!(meta.len(), LEN as u64);
        assert_eq!(
            meta.permissions().mode() & 0o777,
            0o600,
            "the key is host-only"
        );
        assert_eq!(
            load_or_create(&p).unwrap(),
            first,
            "the same key on every launch"
        );
    }

    #[test]
    fn two_vms_get_different_keys() {
        let d = Dir::new("two");
        let a = load_or_create(&d.0.join("a.key")).unwrap();
        let b = load_or_create(&d.0.join("b.key")).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn a_key_of_the_wrong_length_is_refused_and_left_alone() {
        let d = Dir::new("bad");
        for len in [0, LEN - 1, LEN + 1] {
            let p = d.0.join(format!("k{len}"));
            std::fs::write(&p, vec![7u8; len]).unwrap();
            assert!(load_or_create(&p).is_err(), "{len} bytes");
            assert_eq!(std::fs::read(&p).unwrap(), vec![7u8; len], "not replaced");
        }
    }
}
