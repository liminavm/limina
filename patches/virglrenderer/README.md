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
  checkout (commit on the `limina/*` branch first — currently `limina/macos-venus`; the old
  `gkvm/*` branch is the pre-2026-07-01 history, kept for archaeology), output here.

## Apply onto a fresh checkout

```sh
scripts/apply-virgl-patches.sh
```

This checks out `third_party/virglrenderer` at `UPSTREAM_BASE` and `git am`s the series.
`cargo xtask vendor` runs it automatically after cloning the checkout when absent.

## Add / update a patch

1. Edit `third_party/virglrenderer` directly, commit on the `limina/*` branch (one logical change
   per commit) with a `Co-Authored-By` trailer.
2. Re-export the whole series: `rm patches/virglrenderer/*.patch && git -C third_party/virglrenderer
   format-patch <UPSTREAM_BASE>..HEAD -o "$PWD/patches/virglrenderer"`.
3. Verify it reconstructs HEAD (throwaway worktree): `git -C third_party/virglrenderer worktree add
   --detach /tmp/vg <BASE> && git -C /tmp/vg am patches/virglrenderer/*.patch` → the tree must equal
   HEAD's (`git -C /tmp/vg diff <branch>` empty — **this check exists because a 65k-line editor-index
   blob once rode a patch unnoticed**); then `git -C third_party/virglrenderer worktree remove
   --force /tmp/vg`.
4. Commit the regenerated `.patch` files to the limina repo.

## The series — themes

(Theme list below covers the 2026-07-01 shape; the series has since grown through 0050 —
themes for the later spans:)

- **Ring wakeup/latency (0041, 0043-0044, 0049):** one-sleep-per-rung relax backoff, the
  adaptive plateau + per-ring regime classifier, and the graduated responsive ladder
  (0049: 12x10 → 8x20 → 8x40 → hold 80 µs, never a 640 µs sleep in the responsive regime;
  vkmark 2148 → 2304 vs 2365 relax-off ceiling, idle wakes unchanged).
- **Wake diagnostics + attribution (0042, 0046-0048, 0050):** `LIMINA_WAKE_TRACE`,
  `LIMINA_RING_WAKE_PROFILE`, and `VKR_JOURNAL=norecord` (skip the per-vkCmd RECORDING
  lane — measurement mode only, snapshots deliberately broken under it).
- **M9.3/M9.4 snapshot journal (0033-0040):** re-creation journal, device-memory content
  capture, sync fast-forward, create-arg closure pinning.

### The 2026-07-01 series shape (21 patches) — themes

- **macOS/venus enablement (0001-0002):** `shm_open` O_CLOEXEC + host-unsupported-ext filtering +
  `get_map_ptr`; kqueue eventfd emulation + fence-eventfd-by-value in same-process mode.
- **Zero-copy IOSurface scanout (0003-0009, 0011, 0013-0015, 0017-0018):** IOSurface-backed
  `MTLTexture`/exportable scanout memory, fix-A "external" scanout image backing + modifier
  normalization, render-server proxy IOSurface-id threading, `#28` share-HOST_VISIBLE-by-pointer,
  KosmicKrisp winsys, cross-context dmabuf import, capture-sink readback.
- **Correctness fixes (0010, 0016, 0019):** the `__APPLE__` index_type_uint8 hide (Metal-backend
  uint8 index conversion corrupts quads — KK also emulates it, never re-validated: probe before
  dropping); `#8/#31` present fences on reserved ring 63; the seq_cst idle-check tail load that
  lets the ring block at idle with ~0 host wakeups (0019, see
  `docs/design/venus-ring-idle-wakeups.md`).
- **vrend/GL tier (0020-0021):** WebRender/Firefox tile-displacement tear fix
  (`GL_MAP_INVALIDATE_BUFFER_BIT` for guest orphan refills); no-GBM surfaceless EGL winsys so the
  virgl/GL tier comes up on GBM-less hosts (zink-on-KK).

## 2026-07-01 series cleanup (26 → 21)

Re-exported from the rebuilt `limina/macos-venus` branch (full review memo,
`docs/reviews/2026-07-01-full-review.md` Part III). End tree verified identical to the old
series except two deliberate removals:

- **Squashed:** the `virgl_context_blob` zero-init fixup into its parent (old 0008→0007); the
  index_type_uint8 hide + `__APPLE__` gate + root-cause docs into one patch (old 0012–0014 →
  new 0010) — **also stripping 262 accidental `.cache/clangd/index/*.idx` editor blobs
  (~65k patch lines) old 0014 carried**; the interim #30 2 ms timed-wait (old 0006) folded
  into its root-cause replacement (new 0019 — the seq_cst fix reverts it anyway).
- **Dropped:** the `GKVM_RING_RELAX_US` backoff-cap experiment (old 0020) — exonerated by its
  own A/B ("no change at 10k aquarium"); nothing outside the patch referenced the env var.
- **Still MoltenVK-era but deliberately KEPT** (entangled with the live KK path; retire only
  with re-validation, ideally during upstreaming): old 0004/0005 (fix-A backing — introduced
  the `mtl_iosurface` tracking KK's 0011 uses), old 0010 (now 0008, MVK `useIOSurface`), old
  0018 (now 0014, the IFP2 synthesize gate), and the uint8 hide (KK advertises the extension
  too — `spikes/venus-draw-probe/RESULTS.md:375`).
