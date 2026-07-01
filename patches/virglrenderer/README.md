# virglrenderer patch series

limina vendors virglrenderer under `third_party/virglrenderer` (gitignored — a from-source
checkout built by `scripts/build-virglrenderer.sh` into `third_party/virgl-prefix`, which the
worker links; see the `limina-virgl-link-trap` memory). Our changes live here as a
`git format-patch` series so they survive a re-clone and stay reviewable across rebases onto
upstream. This is the host renderer for **both** accelerated tiers: venus (Vulkan→KosmicKrisp)
and vrend (GL via zink-on-KK).

- **`UPSTREAM_BASE`** — the upstream virglrenderer commit the series applies onto
  (`2048dfb`, from https://gitlab.freedesktop.org/virgl/virglrenderer.git).
- **`NNNN-*.patch`** — our patches, in order. Author with `git format-patch` from the vendored
  checkout (commit on the `gkvm/*` branch first), output here.

## Apply onto a fresh checkout

```sh
scripts/apply-virgl-patches.sh
```

This checks out `third_party/virglrenderer` at `UPSTREAM_BASE` and `git am`s the series.
`cargo xtask vendor` runs it automatically after cloning the checkout when absent.

## Add / update a patch

1. Edit `third_party/virglrenderer` directly, commit on the `gkvm/*` branch (one logical change
   per commit) with a `Co-Authored-By` trailer.
2. Re-export the whole series: `rm patches/virglrenderer/*.patch && git -C third_party/virglrenderer
   format-patch <UPSTREAM_BASE>..HEAD -o "$PWD/patches/virglrenderer"`.
3. Verify it reconstructs HEAD (throwaway worktree): `git -C third_party/virglrenderer worktree add
   --detach /tmp/vg <BASE> && git -C /tmp/vg am patches/virglrenderer/*.patch` → the tree must equal
   HEAD's; then `git -C third_party/virglrenderer worktree remove --force /tmp/vg`.
4. Commit the regenerated `.patch` files to the limina repo.

## The series (26 patches) — themes

- **macOS/venus enablement (0001-0002):** `shm_open` O_CLOEXEC + host-unsupported-ext filtering +
  `get_map_ptr`; kqueue eventfd emulation + fence-eventfd-by-value in same-process mode.
- **Zero-copy IOSurface scanout (0003-0011, 0015, 0017-0019, 0022-0023):** IOSurface-backed
  `MTLTexture`/exportable scanout memory, fix-A "external" scanout image backing + modifier
  normalization, render-server proxy IOSurface-id threading, `#28` share-HOST_VISIBLE-by-pointer,
  KosmicKrisp winsys, cross-context dmabuf import, capture-sink readback.
- **Correctness fixes (0006, 0012-0014, 0021, 0024):** venus ring idle/notify handshake — the
  `#30` timed-wait (0006) and its proper root-cause replacement, the seq_cst idle-check tail load
  that lets the ring block at idle with ~0 host wakeups (0024, see
  `docs/design/venus-ring-idle-wakeups.md`); MoltenVK uint8 index-conversion hide; `#8/#31` present
  fences on reserved ring 63.
- **vrend/GL tier (0025-0026):** WebRender/Firefox tile-displacement tear fix
  (`GL_MAP_INVALIDATE_BUFFER_BIT` for guest orphan refills); no-GBM surfaceless EGL winsys so the
  virgl/GL tier comes up on GBM-less hosts (zink-on-KK).
