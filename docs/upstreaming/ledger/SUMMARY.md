# Patch-audit — cross-series summary (2026-08-03)

The per-patch upstreaming audit of every series limina carries, completed 2026-08-03.
Method, schema, and research protocol: `README.md`. Per-patch verdicts and reasoning:
the `<series>.md` files. This file is the roll-up across all of them.

**Standing constraints of the audit:** no builds were run (a full HVF suite was live on
another machine); every "needed-on-tip" verdict is a source read against the named upstream
SHA, not a compile. Research agents were **read-only** — the audit records where an issue/MR
*should* be filed as an action item; **it never posted to any tracker**. All commits are
docs-only.

## Scope

| series | patches | upstream (checked SHA) | tracker |
|---|---|---|---|
| linux | 3 *(was 6; migrated to the fork model 2026-08-03 — base `v7.1.6`; one patch left for the DKMS tree, one replaced by an upstream backport, and the blob-scanout fence **dropped 2026-08-04** after the rig measured it costing 86% of frames)* | `gregkh/linux` (stable mirror) + dri-devel lore | lore.kernel.org |
| kosmickrisp (KK) | 19 | mesa `84acd848` | gitlab.fd.o (Anubis) |
| mesa (guest) | 15 | mesa `c9e4f184e593` | gitlab.fd.o (Anubis) |
| imago | 2 | `hreitz/imago` | gitlab.com |
| mutter | ~~1~~ RETIRED | — | series removed 2026-08-03 (own compositor) |
| virglrenderer | 58 | `956b034f` | gitlab.fd.o (Anubis) |
| libkrun | 126 | `c652b56` (main) | github.com/libkrun/libkrun |
| edk2 | (script) | — | prose section, no rows |
| **total** | **226** | | (was 227; mutter's 1 retired) |

## Headline: what changes hands, what stays

### 1. DROP — upstream already fixed it, retire on the next base bump (8)

Independently rediscovered and fixed upstream while we carried our version. These are pure
wins: delete on rebase, no work owed.

- **linux 0005** — balloon free-page-reporting across suspend → upstream `0b45f69` in
  `mm/page_reporting.c` (a file we don't patch; the fix landed elsewhere — see method lesson).
  *Resolved 2026-08-03: `0b45f69` had NOT reached our new base `v7.1.6`, so it is carried as a
  cherry-pick rather than simply dropped — "fixed upstream" and "fixed in your base" are
  different questions. Self-retires at the next base bump.*
- **KK 0008-arc** (timestamp queries, 0008+0010–0013) → LunarG's Metal-4 impl (`ed807097`, MR !42864).
- **mesa 0017** (venus submit freelist) → upstream `09fb7ca8` / MR !43229 (bounded cache, reaches
  Fedora 26.1.5). Closes memory `limina-venus-submit-freelist`.
- **mesa 0009** (rect-clone hunks) → upstream !42528.
- **KK 0016 / mesa PBO** (FS-sampler-views invalidate) → upstream `479773c7e42` / MR !43151. Closes
  memory `limina-zink-pbo-kk`.
- **virgl 0001 shm_open hunk** → upstream !1634.
- **libkrun 0038** (virtio-blk serial) → `df85b8b`; **libkrun 0090** (virtio-fs used-ring len) →
  `4e01b23`; **libkrun 0044** (blob-map overflow) → `checked_blob_map_addr`, PR #790.

**Lesson embedded here (README):** actively-developed upstreams make "unchanged since base"
verdicts perishable — LunarG reinvented four KK patches in five weeks. Re-verify at MR/rebase time.

### 2. UPSTREAM-NOW — clean, guest-visible bug on shared code; file + PR (the send queue)

Ranked roughly by blast radius / reviewer-readiness. None are blocked on disclosure.

**Flagship DoS-hardening (guest-triggerable host abort on STOCK upstream):**
- **libkrun 0028** — unhandled-PSCI `panic!` → NOT_SUPPORTED; umbrella for the whole panic surface;
  cite open #577. **Send first.**
- **libkrun 0125** — console port-reopen `expect()` abort (same class, ships a test).
- **virgl 0019** — seq_cst idle-check lost-wakeup that aborts a guest in `vn_relax` (sibling to
  merged !1610).
- **virgl 0022 / 0030 / 0032 / 0045** — four guest-triggerable DoS/exhaustion fixes with clean
  `Fixes:` targets (0030 strongest — upstream landed the leaking construct itself in MR 1602).
- **libkrun 0058** — stale vsock muxer threads write into a freed guest RX ring (memory corruption).

**Trivial correctness (small, self-evident):**
- libkrun: **0004** (halfword-MMIO write arm, +0005), **0040** (vtimer multiply-first), **0093**
  (timesync saturating_sub), **0037**/**0039** (input reset/epoll), **0014**+**0024** (GPU depanic),
  **0041** (macOS blob unmap balance), **0031** (display -2 enum arm), **0122** (EDID digital),
  **0119**-generator-subset (aspect shift + u16 clock wrap), **0027** (de-shear, strip DIAG),
  **0008**+**0015** (cursor depanic), **0061**-carve (KEY_POWER not KEY_RESTART).
- mesa: **0014** (zink lost-wakeup deadlock — byte-for-byte on main, + the trywait timespec has
  silently never waited), **0013** (venus ICD TLS-destructor pin), **0002** (fbobject NULL guard),
  **0003+0004** as one MR.
- KK: **0009** (vk_meta empty rects — broad audience), the monolith's **dm nil-check** and
  **2DArray→2D demotion**.
- virgl: **0020 / 0021 / 0023 / 0057** (vrend correctness + macOS enablement), **0002** (kqueue
  eventfd macOS hole).

**Directly wanted — an OPEN upstream issue already asks for it:**
- **libkrun 0029** → OPEN **#565** (capset count) + in-flight PR #560.
- **libkrun 0087** → OPEN **#707** (balloon deflate-on-oom toggle).

### 3. CONVERGENT — an in-flight upstream PR overlaps; review/contribute the delta, don't re-submit

- **libkrun #762** (HVF snapshot/restore) overlaps our entire M9 snapshot cluster. Ours is broader
  on three axes worth contributing: **0053** (multi-vCPU restore — #762 explicitly rejects it),
  **0081**/**0084** (parallel-lz4+zero-hole compression + atomic publish #762 lacks). **0055/0056/0058**
  add the `reset()` path #762 omits.
- **libkrun #794** (balloon reclaim) converges with **0033**'s MADV_FREE_REUSABLE switch — the
  maintainer thread favors our approach. Offer the switch; the 16k coalescer rides as an add-on.
- **virgl !1617** (typed Metal `VIRGL_RESOURCE_METAL_HEAP`) is upstream's chosen alternative to our
  raw-`map_ptr` macOS model (0009/0011/0013). Convergence work item: **add VK_EXT_external_memory_metal
  (MTLHEAP) to KosmicKrisp, then align on !1617** — not an MR of our diff.
- **mesa 0013** class already met upstream in #13571; **KK depth_clip** appeared as a Draft MR the
  day of the audit.

### 4. CARRY — limina/macOS-shaped, no upstream home (the bulk)

The macOS GPU coexist/venus/IOSurface stack, the M9.3 GPU snapshot-replay journal (libkrun
0071–0086), the reset lifecycle, the venus WSI residual (mesa 0010/0011/0015 — shrinks with M15
device-advertised modifiers, not with MRs), the KK monolith's Metal-bridge core, all log-taste and
DIAG-probe rows. **mutter is no longer in scope** — the series was retired 2026-08-03 (limina is
writing a drop-in gnome-shell/mutter replacement), so the ext-data-control patch and its two retired
robustness fixes are moot. The `clipboard@limina` shell-extension bridge remains the GNOME clipboard
path until the replacement compositor subsumes it.

### 5. NEW-FEATURE RFC — no upstream prior art or demand; offer as RFC, expect a passthrough objection

- **libkrun USB/xHCI** (0095–0105, 0126) — upstream has zero USB prior art; emulated xHCI + gadgets.
  0101 (hostile-ring TD DoS guard) **must** ship folded with 0096/0097.
- **libkrun C8 new devices** — virtio-i2c/SBS battery (0042), virtio-snd/CoreAudio (0047–0049),
  gpio lifecycle buttons (0060–0062). Guest side is stock Linux for every one. PR #749 shows upstream
  moving *away* from sound, so our in-VMM virtio-snd is a new shape, not a supersession.

## Disclosure outcome — GATE CLEARED

The audit's one embargo question (libkrun 0044 / 0012, blob-map `offset+size` overflow) is
**resolved with no embargo**: an independent contributor already fixed it publicly on upstream tip
(`checked_blob_map_addr`, PR #790), and libkrun handles this DoS class in the open (#577). 0044
downgrades to DROP; 0012 downgrades to a benign compat/enablement delta tied to our virgl fork.
**No patch in any series is blocked-on-disclosure.** (virgl's one guest-OOB, 0025, is likewise not
disclosure-blocked.)

## Cross-cutting blockers before ANY freedesktop MR

1. **`gkvm`/`GKVM_` naming** in virgl subjects, comments, and env vars (0003–0008, 0011, 0012, 0015,
   0016, 0019, 0024, 0027, 0028, 0030, 0032) — rename-before-MR, verdict-neutral. libkrun and C8 are
   already clean; mesa/KK clean.
2. **DIAG hooks** — strip `/tmp/limina-*` probe hooks before upstreaming (libkrun 0017/0025/0027/0032/
   0110/0112/0113; virgl per its DIAG rows). Env-gated probes (no /tmp) are lower-risk but still
   `strip-diag-first`.
3. **Fold-before-send** — several sends must carry a later fixup in the same commit (never submit the
   parent alone): virgl 0041+0043+0044+0049 relax squash; KK XFB + 0007 clamp; libkrun 0101 with
   0096/0097; libkrun 0045 panic-guard into 0033.

## Action items for the user (the audit never posts — these are yours to file)

- **File** libkrun #-issues + PRs for the send queue, led by 0028 / 0125 (DoS hardening, cite #577).
- **File** an imago issue at `gitlab.com/hreitz/imago` for 0001 (discard-truncate on capacity-bearing
  backing files) with the `spikes/m10-disk-durability/` repro, offering both fix shapes.
- **Attach** libkrun 0029 to open #565; **0087** to open #707.
- **Review-not-resubmit**: libkrun #762 and #794, virgl !1617 (drive the MTLHEAP convergence).
- **Work items** (limina-side, not MRs): advertise VIRTIO_GPU_F_BLOB_ALIGNMENT + verify guest-Mesa
  rounding (retires linux 0004 + DKMS); add VK_EXT_external_memory_metal to KK; plan the KK series as
  its own rebase *milestone* (Metal-4 rewrite, not a rebase) — timestamp arc + 0005 + 0016 drop, the
  monolith loses its bind-cache premise.
- **401-gated re-reads at MR time** (need a logged-in browser): mesa !37115 stall reason, !42501
  pushback; any GitLab MR-note rationale the anonymous REST/GraphQL pass couldn't reach.

## Memory items this audit closed or updated

Closed: `limina-zink-pbo-kk`, `limina-venus-submit-freelist` (both fixed upstream independently).
Updated: `limina-patch-audit` (progress → done), `limina-upstreaming-triage` (the 2026-07-04
inventory is stale — predates libkrun 0095–0126 entirely; superseded by these ledgers).
