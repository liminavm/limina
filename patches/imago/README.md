# imago patch series

`imago` is the storage/image-format crate libkrun's block device (`krun-devices`) uses to
back virtio-blk (Raw + qcow2). It is a normal crates.io dependency, but we need one small
behavioral change, so we vendor the pristine source under `third_party/imago` (gitignored —
a from-source checkout) and carry our change as a `git format-patch` series here, exactly
like `patches/libkrun/`.

limina's **root `Cargo.toml`** overrides the registry crate with the vendored path:

```toml
[patch.crates.io]
imago = { path = "third_party/imago" }
```

The `[patch]` lives in limina's workspace root (not libkrun's) because limina builds
libkrun's crates as path dependencies in its own build graph (third_party is *excluded*
from the workspace), so the top-level `[patch]` is what rewrites the transitive `imago`.

## Apply onto a fresh checkout

```sh
cargo xtask vendor          # applies every series (libkrun + imago); run this after a clone
# or, just imago:
scripts/apply-imago-patch.sh
```

This vendors pristine `imago-0.2.2` (from the cargo registry cache, or downloaded from
crates.io if it isn't there — `cargo fetch` can't run yet because the `[patch.crates-io]`
override below points at this not-yet-vendored path), commits it as the base, and `git am`s
the series.

## Add / update a patch

1. Edit `third_party/imago` directly, commit (one logical change per commit) with a
   `Co-Authored-By` trailer.
2. Re-export: `git -C third_party/imago format-patch <base>.. -o "$PWD/patches/imago"`.
3. Commit the regenerated `.patch` files to the limina repo.

## Current patches

- **0001 — discard preserves the backing file size (no truncate-to-EOF).** imago's
  `File::try_discard_by_truncate` truncated the backing file when a discard range reached
  EOF (to reclaim the tail). That is wrong for a **fixed-capacity virtio-blk backing file**:
  the device capacity is derived from the file size *at open* (`block/device.rs:304` →
  `DiskProperties::new`), so truncation shrinks the advertised capacity on the next open. A
  guest filesystem sized to the original capacity — e.g. `mkfs.ext4`, which discards the
  device tail past its last block group — then becomes **unmountable after a reboot**
  (`EXT4-fs (vdb): bad geometry: block count … exceeds size of device …`). The data is
  intact; only the geometry no longer matches. Root-caused in
  `spikes/m10-disk-durability/` (RESULTS.md). The patch disables the truncate path; every
  discard now falls through to the punch-hole path (`F_PUNCHHOLE` on macOS), which reclaims
  blocks while preserving the file's logical size, so the capacity is stable across opens.
  A no-op for `qcow2` (its own discard logic; the file path is the host image only).
