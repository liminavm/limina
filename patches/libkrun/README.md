# libkrun patch series

limina vendors libkrun under `third_party/libkrun` (gitignored — a from-source checkout we
build against via its internal Rust crates, decision D2.1). Our changes to libkrun live
here as a `git format-patch` series so they survive a re-clone and stay reviewable across
rebases onto upstream.

- **`UPSTREAM_BASE`** — the upstream libkrun commit the series applies onto.
- **`NNNN-*.patch`** — our patches, in order. Author with `git format-patch` from the
  vendored checkout (commit on a `limina/*` branch first), output here.

## Apply onto a fresh checkout

```sh
scripts/apply-libkrun-patches.sh
```

This checks out `third_party/libkrun` at `UPSTREAM_BASE` and `git am`s the series.

## Add / update a patch

1. Edit `third_party/libkrun` directly, commit on a `limina/*` branch (one logical change
   per commit) with a `Co-Authored-By` trailer.
2. Re-export: `git -C third_party/libkrun format-patch <base>.. -o "$PWD/patches/libkrun"`.
3. Commit the regenerated `.patch` files to the limina repo.

## Current patches

- **0001 — software 2D virtio-gpu scanout for GL-less hosts.** libkrun maps
  `RESOURCE_CREATE_2D` onto a virgl GL render target, which has no host context on macOS,
  so 2D resource creation fails and nothing reaches the display. Shadows 2D resources in
  host CPU memory (create/attach-backing/transfer/set-scanout/flush) without touching
  rutabaga — a working software scanout baseline (fbcon, EFI GOP, simpledrm). The
  accelerated Venus/blob/3D path is unchanged. This is limina's Tier-1 display floor.
