// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! L1: a managed VM that cannot start is refused by the shipped `limina` binary *before* it
//! spawns anything, and says why.
//!
//! The unit tests in `vmlib::preflight` cover the predicate; this covers the wiring — that
//! `limina start` actually consults it, ahead of the run lock, and that `limina check` reports
//! the same verdict. Deliberately NOT HVF-gated: every case here fails pre-flight, so no VM is
//! ever booted and no hypervisor entitlement is needed. A VM that got as far as HVF would be
//! this test failing.

use std::path::{Path, PathBuf};
use std::process::Command;

fn limina(bin: &Path, library: &Path) -> Command {
    let mut cmd = Command::new(bin);
    cmd.env("LIMINA_VM_LIBRARY", library);
    cmd
}

struct Fixture {
    bin: PathBuf,
    library: PathBuf,
    bundle: PathBuf,
    disk: PathBuf,
}

/// A managed VM with a blank disk, then that disk removed — the shape of a `.liminavm` copied
/// between machines without its image.
fn vm_with_a_missing_disk(tag: &str) -> Option<Fixture> {
    let Ok(bin) = limina_test::limina_bin() else {
        eprintln!("skipping {tag}: limina not built (cargo build -p limina)");
        return None;
    };
    let library = std::env::temp_dir().join(format!("limina-pfcli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&library);
    std::fs::create_dir_all(&library).unwrap();

    let out = limina(&bin, &library)
        .args(["create", "Orphan", "--blank", "64M"])
        .output()
        .expect("spawning limina create");
    assert!(
        out.status.success(),
        "limina create failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let bundle = library.join("Orphan.liminavm");
    let disk = bundle.join("disks/root.raw");
    assert!(disk.is_file(), "create should have made {}", disk.display());
    std::fs::remove_file(&disk).unwrap();

    Some(Fixture {
        bin,
        library,
        bundle,
        disk,
    })
}

#[test]
fn start_refuses_a_missing_disk_before_taking_the_run_lock() {
    let Some(f) = vm_with_a_missing_disk("start") else {
        return;
    };

    let out = limina(&f.bin, &f.library)
        .args(["start", "Orphan"])
        .output()
        .expect("spawning limina start");

    assert!(!out.status.success(), "start must fail, not boot");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("cannot start"), "{err}");
    // The resolved absolute path, so the message is actionable on sight.
    assert!(err.contains(f.disk.to_str().unwrap()), "{err}");
    assert!(err.contains(":create=SIZE"), "{err}");

    // Refused ahead of `runtime::acquire` (design §3.4): no pidfile, and the run lock -- if
    // the directory exists at all -- was never taken.
    assert!(
        !f.bundle.join("run/supervisor.pid").exists(),
        "a refused start must not write a pidfile"
    );

    std::fs::remove_dir_all(&f.library).ok();
}

#[test]
fn check_reports_the_same_verdict_without_starting_anything() {
    let Some(f) = vm_with_a_missing_disk("check") else {
        return;
    };

    let out = limina(&f.bin, &f.library)
        .args(["check", "Orphan"])
        .output()
        .expect("spawning limina check");

    assert!(
        !out.status.success(),
        "check must exit non-zero when blocked"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("BLOCKER"), "{stdout}");
    assert!(stdout.contains(f.disk.to_str().unwrap()), "{stdout}");

    // And it passes once the disk is back, so the verdict tracks reality rather than
    // latching on the definition.
    std::fs::write(&f.disk, vec![0u8; 1024]).unwrap();
    let out = limina(&f.bin, &f.library)
        .args(["check", "Orphan"])
        .output()
        .expect("spawning limina check");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "check should pass once the disk exists: {stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("can start"), "{stdout}");

    std::fs::remove_dir_all(&f.library).ok();
}
