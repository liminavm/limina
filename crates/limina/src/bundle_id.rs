// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Which bundle this is, in terms someone can look up: the parking-lot name, and the CDHash.
//!
//! `cargo xtask app` parks every `.dmg` it makes as `Limina-<date>-<n>.dmg` and records the
//! revisions it was built from under that name in the lot's `INDEX.md`
//! (`scripts/park-bundle.sh`). `scripts/build-app.sh` reserves the name before the bundle is
//! signed and writes it to `Contents/Resources/bundle-id`, so the app can report the dmg it came
//! from. The identity carries it as `limina_bundle`:
//!
//! - the name, when the stamp is there and well formed;
//! - `none` outside a bundle (a cargo-built binary has no dmg to name);
//! - `unparked` for a bundle with no stamp (its build could not reserve a name);
//! - `malformed` for a stamp that is not a parking-lot name, rather than repeating whatever the
//!   file held: a value that might be a name and is not would send the reader to the wrong dmg.
//!
//! **The CDHash** (`limina_cdhash`) is the second field: the running supervisor's code-directory
//! hash, from the kernel (`csops(CS_OPS_CDHASH)`), so no `codesign` run. In a bundle the
//! supervisor is the main executable, whose signature seals `Info.plist`, `Resources` (the stamp
//! included) and the nested worker, so the hash changes when anything in the bundle does. It
//! also names a build that was never parked, a cargo binary included (the linker signs those
//! ad hoc), and park-bundle.sh records it in `INDEX.md` beside the name. It is the hash
//! `codesign -dvvv <Limina.app>` prints as `CDHash=`; `none` for an unsigned binary.

use std::path::Path;
use std::sync::OnceLock;

/// The stamp's file name in the bundle's Resources, as `scripts/build-app.sh` writes it.
pub const STAMP: &str = "bundle-id";

/// What this process reports as `limina_bundle`. Read once: the bundle does not change under a
/// running process, and a replaced one would not be the code that is running.
pub fn name() -> &'static str {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| {
        let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
        else {
            return "none".into();
        };
        name_in(&dir, |p| p.exists(), |p| std::fs::read_to_string(p).ok())
    })
}

/// `limina_bundle` for a supervisor running from `exe_dir`; `exists` and `read` reach the
/// filesystem (injected for tests).
///
/// A bundle is recognised by its `Contents/Info.plist` beside `Contents/MacOS`, the layout
/// `scripts/build-app.sh` assembles; `target/<profile>/` has no `../Info.plist`.
pub fn name_in(
    exe_dir: &Path,
    exists: impl Fn(&Path) -> bool,
    read: impl Fn(&Path) -> Option<String>,
) -> String {
    if !exists(&exe_dir.join("../Info.plist")) {
        return "none".into();
    }
    match read(&exe_dir.join("../Resources").join(STAMP)) {
        None => "unparked".into(),
        Some(text) => {
            let text = text.strip_suffix('\n').unwrap_or(&text);
            if is_lot_name(text) {
                text.to_string()
            } else {
                "malformed".into()
            }
        }
    }
}

/// `Limina-YYYY-MM-DD-<n>.dmg`, exactly the shape `park-bundle.sh --reserve` hands out.
fn is_lot_name(s: &str) -> bool {
    let Some(rest) = s
        .strip_prefix("Limina-")
        .and_then(|r| r.strip_suffix(".dmg"))
    else {
        return false;
    };
    let parts: Vec<&str> = rest.split('-').collect();
    let digits = |p: &str, len: Option<usize>| {
        !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && len.is_none_or(|l| p.len() == l)
    };
    matches!(parts.as_slice(), [y, m, d, n]
        if digits(y, Some(4)) && digits(m, Some(2)) && digits(d, Some(2)) && digits(n, None) && n.len() <= 6)
}

/// What this process reports as `limina_cdhash`.
pub fn cdhash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| own_cdhash().unwrap_or_else(|| "none".into()))
}

/// `CS_OPS_CDHASH` from xnu's `bsd/sys/codesign.h`: the running code's CDHash, 20 bytes.
const CS_OPS_CDHASH: libc::c_uint = 5;
const CDHASH_LEN: usize = 20;

unsafe extern "C" {
    /// In libSystem; the `libc` crate does not declare it.
    fn csops(
        pid: libc::pid_t,
        ops: libc::c_uint,
        useraddr: *mut libc::c_void,
        usersize: libc::size_t,
    ) -> libc::c_int;
}

/// This process's CDHash in lowercase hex, or `None` when it has no code signature.
fn own_cdhash() -> Option<String> {
    let mut hash = [0u8; CDHASH_LEN];
    // SAFETY: `hash` is CDHASH_LEN writable bytes, the size passed; csops writes at most that.
    let rc = unsafe {
        csops(
            libc::getpid(),
            CS_OPS_CDHASH,
            hash.as_mut_ptr().cast(),
            CDHASH_LEN,
        )
    };
    if rc != 0 || hash.iter().all(|&b| b == 0) {
        return None;
    }
    Some(hash.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const MACOS: &str = "/Applications/Limina.app/Contents/MacOS";

    fn bundle(stamp: Option<&str>) -> String {
        let plist = PathBuf::from(MACOS).join("../Info.plist");
        let res = PathBuf::from(MACOS).join("../Resources").join(STAMP);
        name_in(
            Path::new(MACOS),
            |p| p == plist,
            |p| (p == res).then(|| stamp.map(str::to_string)).flatten(),
        )
    }

    #[test]
    fn a_stamped_bundle_reports_its_parking_lot_name() {
        assert_eq!(
            bundle(Some("Limina-2026-10-11-3.dmg\n")),
            "Limina-2026-10-11-3.dmg"
        );
        assert_eq!(
            bundle(Some("Limina-2026-10-11-12.dmg")),
            "Limina-2026-10-11-12.dmg",
            "no newline is fine"
        );
    }

    #[test]
    fn a_dev_binary_is_not_a_bundle() {
        let got = name_in(Path::new("/src/limina/target/debug"), |_| false, |_| None);
        assert_eq!(got, "none");
        // A stray stamp next to a non-bundle binary does not make it one.
        let got = name_in(
            Path::new("/src/limina/target/debug"),
            |_| false,
            |_| Some("Limina-2026-10-11-0.dmg".into()),
        );
        assert_eq!(got, "none");
    }

    #[test]
    fn a_bundle_without_a_stamp_says_unparked() {
        assert_eq!(bundle(None), "unparked");
    }

    #[test]
    fn a_stamp_that_is_not_a_lot_name_is_reported_as_malformed() {
        for bad in [
            "",
            "\n",
            "Limina-2026-10-11.dmg",
            "Limina-2026-10-11-3",
            "Limina-26-10-11-3.dmg",
            "Limina-2026-10-11-3.dmg\nlimina_git_rev=forged",
            "Limina-2026-10-11-x.dmg",
            "Limina-2026-10-11-3.dmg ",
            "../Limina-2026-10-11-3.dmg",
            "Limina-2026-10-11-9999999.dmg",
        ] {
            assert_eq!(bundle(Some(bad)), "malformed", "{bad:?}");
        }
    }

    #[test]
    fn the_cdhash_is_the_one_codesign_reports_for_this_binary() {
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new("codesign")
            .arg("-dvvv")
            .arg(&exe)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stderr);
        let want = text
            .lines()
            .find_map(|l| l.strip_prefix("CDHash="))
            .map(str::to_lowercase);
        // arm64 binaries are always signed, ad hoc by the linker at the least.
        let want = want.unwrap_or_else(|| panic!("codesign printed no CDHash:\n{text}"));
        assert_eq!(own_cdhash().as_deref(), Some(want.as_str()));
        assert_eq!(cdhash(), want);
    }
}
