// SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
// Copyright © 2026 Gustavo Noronha Silva

//! Guard: the guest and the host round a host-visible blob to the SAME size.
//!
//! The guest rounds host3d blob sizes up (mesa `vn_renderer_virtgpu.c`) so that the offsets it
//! picks inside the host-visible arena stay aligned to any host's page size — the guest packs
//! blobs back to back with no alignment, so one ragged size skews every offset after it and
//! `hv_vm_map` refuses those maps outright. The host pads the VkDeviceMemory to match
//! (virglrenderer `LIMINA_BLOB_SIZE_ALIGN`).
//!
//! The two values are load-bearing together, in two trees we rebase separately. Equal is
//! correct. Guest-rounds-larger is the dangerous direction: the VMM maps the BLOB's size from
//! the host pointer, so the guest would receive host memory past the end of the allocation.
//! virglrenderer refuses that at run time, but by then the guest has already lost the memory it
//! asked for, and the cause is a constant in another repository.
//!
//! Not HVF-gated: it reads two files. Skips when `third_party/` has not been vendored.

use std::path::PathBuf;

/// The `64 * 1024` in an `align64(blob_size, 64 * 1024)` / `#define … (64 * 1024)`, in bytes.
fn parse_kib_product(expr: &str) -> Option<u64> {
    let (lhs, rhs) = expr.split_once('*')?;
    let n: u64 = lhs.trim().parse().ok()?;
    let unit: u64 = rhs.trim().parse().ok()?;
    Some(n * unit)
}

fn host_granularity(src: &str) -> Option<u64> {
    let after = src.split("#define LIMINA_BLOB_SIZE_ALIGN").nth(1)?;
    let paren = after.split_once('(')?.1.split_once(')')?.0;
    parse_kib_product(paren)
}

fn guest_granularity(patch: &str) -> Option<u64> {
    // The exported series carries the added line with a leading '+'.
    let line = patch
        .lines()
        .find(|l| l.starts_with('+') && l.contains("align64(blob_size,"))?;
    let args = line.split_once("align64(blob_size,")?.1;
    parse_kib_product(args.split_once(')')?.0)
}

#[test]
fn the_guest_and_the_host_agree_on_the_blob_size_granularity() {
    let root = limina_test::repo_root();
    let host_path = root.join("third_party/virglrenderer/src/venus/vkr_device_memory.c");
    let Ok(host_src) = std::fs::read_to_string(&host_path) else {
        eprintln!(
            "skipping: {} not vendored (cargo xtask vendor)",
            host_path.display()
        );
        return;
    };

    let guest_path: PathBuf =
        root.join("patches/mesa-guest/0011-venus-round-host3d-blob-sizes-up-to-64-KiB.patch");
    let guest_src = std::fs::read_to_string(&guest_path).unwrap_or_else(|e| {
        panic!(
            "{} is missing ({e}); it is the guest half of this pair — re-export it with \
             scripts/export-mesa-guest-patches.sh, and if the commit was dropped from the fork, \
             drop LIMINA_BLOB_SIZE_ALIGN's use with it",
            guest_path.display()
        )
    });

    let host = host_granularity(&host_src).unwrap_or_else(|| {
        panic!(
            "no LIMINA_BLOB_SIZE_ALIGN in {} — the host stopped padding host-visible \
             allocations, so a rounded guest blob now maps past the end of one",
            host_path.display()
        )
    });
    let guest = guest_granularity(&guest_src).unwrap_or_else(|| {
        panic!(
            "no align64(blob_size, …) in {} — the guest stopped rounding, so its arena \
             offsets go ragged again and the host refuses to map them",
            guest_path.display()
        )
    });

    assert_eq!(
        guest, host,
        "guest rounds host3d blobs to {guest} bytes but the host pads allocations to {host}; \
         they must match — a guest rounding to more than the host allocates would be handed \
         host memory past the end of the allocation"
    );
    assert!(
        host >= 64 * 1024,
        "granularity {host} is below 64 KiB, the largest page size a host may use; a host \
         with larger pages than that would refuse these maps again"
    );
}
