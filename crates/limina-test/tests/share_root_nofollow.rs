// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Guard: libkrun opens a virtio-fs share root with `O_NOFOLLOW`.
//!
//! limina refuses a symlinked share root itself (`parse_share`, and `[[share]] path` must be
//! absolute — `docs/design/vm-start-preflight.md` §3.5), so this flag is defence in depth
//! rather than the only defence. It is still load-bearing, and it lives in a vendored tree we
//! rebase onto upstream regularly, where a silent drop would restore the exposure with nothing
//! to notice it. Our own refusal happens before the worker runs, so no boot test can observe
//! this flag any more — the source is the only place left to check it.
//!
//! Not HVF-gated: it reads a file. Skips when `third_party/` has not been vendored.

use std::path::PathBuf;

fn passthrough_rs() -> PathBuf {
    limina_test::repo_root()
        .join("third_party/libkrun/src/devices/src/virtio/fs/macos/passthrough.rs")
}

#[test]
fn the_share_root_is_opened_nofollow() {
    let path = passthrough_rs();
    let Ok(src) = std::fs::read_to_string(&path) else {
        eprintln!(
            "skipping: {} not vendored (cargo xtask vendor)",
            path.display()
        );
        return;
    };

    // Both root opens — `PassthroughFs::new`'s probe and `FileSystem::init`'s traversal fd —
    // pass the share root to openat(AT_FDCWD, …). Without O_NOFOLLOW a symlink planted at the
    // root path is followed by the host and its target is mounted into the guest.
    let opens: Vec<&str> = src
        .split("libc::openat(")
        .skip(1)
        .filter(|body| body.contains("libc::AT_FDCWD") && body.contains("root.as_ptr()"))
        .collect();

    assert_eq!(
        opens.len(),
        2,
        "expected the two share-root openat calls in {}; the file was restructured, so \
         re-verify the root is still opened O_NOFOLLOW and update this guard",
        path.display()
    );
    for (i, body) in opens.iter().enumerate() {
        // Up to the end of the enclosing statement — the arg list spans several lines and
        // contains its own parens (`root.as_ptr()`), so `;` is the reliable terminator.
        let args = body.split(';').next().unwrap_or(body);
        assert!(
            args.contains("O_NOFOLLOW"),
            "share-root openat #{} in {} lost O_NOFOLLOW: a symlinked share root would be \
             followed by the host and its target mounted into the guest\n{args}",
            i + 1,
            path.display()
        );
    }
}
