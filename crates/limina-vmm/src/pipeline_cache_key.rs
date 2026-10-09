//! The key the renderer signs the guest's pipeline-cache data with.
//!
//! virglrs hands a guest `vkGetPipelineCacheData` output with an HMAC tag, and forwards a guest's
//! `pInitialData` to the driver only when the tag verifies under the same key, so a guest cannot
//! feed the host driver's cache parser bytes the host did not write. A key that lives only as long
//! as the worker makes every boot's caches cold; one kept in a file and passed on every launch
//! keeps them warm. It is host-only: never logged, never shown to the guest.

use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// The key's length in bytes (virglrs's `PipelineCacheKey::LEN`).
pub const LEN: usize = 32;

/// Read the key in `path`, or make one there from the OS random source when there is none.
///
/// A new key is written whole to a private (0600) file beside `path` and only then linked into
/// place, so `path` never exists half-written: a crash leaves at most a stray temporary, and two
/// workers racing on one path all end up with the key whose link landed first. A file that is not
/// exactly [`LEN`] bytes is an error, not something to replace: a key that changes makes every
/// saved cache cold.
pub fn load_or_create(path: &Path) -> Result<[u8; LEN]> {
    match std::fs::read(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        Ok(bytes) => return exact(path, bytes),
    }
    let mut key = [0u8; LEN];
    getrandom::fill(&mut key)
        .map_err(|e| anyhow::anyhow!("no OS randomness for {}: {e}", path.display()))?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!(
        ".{name}.{}.{:?}.tmp",
        std::process::id(),
        std::thread::current().id()
    ));
    // The name is this thread's alone, so one left by a crashed process with our pid is stale.
    let _ = std::fs::remove_file(&tmp);
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(&key).and_then(|()| f.sync_all()));
    // hard_link refuses an existing target, which is what makes the publish atomic.
    let linked = written.and_then(|()| std::fs::hard_link(&tmp, path));
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(key),
        // Another worker published first: use theirs, which is whole by construction.
        Err(e) if e.kind() == ErrorKind::AlreadyExists => exact(
            path,
            std::fs::read(path).with_context(|| format!("reading {}", path.display()))?,
        ),
        Err(e) => {
            Err(e).with_context(|| format!("creating the pipeline-cache key {}", path.display()))
        }
    }
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
    fn workers_racing_on_one_path_all_get_the_one_key() {
        let d = Dir::new("race");
        for round in 0..50 {
            let p = d.0.join(format!("k{round}"));
            let keys: Vec<_> = std::thread::scope(|s| {
                let hs: Vec<_> = (0..8).map(|_| s.spawn(|| load_or_create(&p))).collect();
                hs.into_iter().map(|h| h.join().unwrap()).collect()
            });
            let first = keys[0].as_ref().expect("no worker may lose the race");
            for k in &keys {
                assert_eq!(k.as_ref().expect("no worker may lose the race"), first);
            }
            assert_eq!(std::fs::read(&p).unwrap(), first.to_vec());
        }
        let stray: Vec<_> = std::fs::read_dir(&d.0)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(stray.is_empty(), "temporaries left behind: {stray:?}");
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
