# AGX Metal compiler `bitcode_url` abort — investigation notes

The worker died with SIGABRT on 2026-08-14 while the user ran the basemark web benchmark on
`Fedora-Workstation-44.enhanced.synoik.raw`. This is the working file for chasing it.

## SOLVED 2026-08-15 — read this section, the rest is the trail

The abort is a render pass with **no attachments** whose **`defaultRasterSampleCount` is still 0**.
`mtlrp-min.m` reproduces it in ~40 lines of pure Metal, no Vulkan and no VM, on both the classic
API and MTL4: sample count 0 aborts, 1 and 2 are green. It is **invalid input** — `MTL_DEBUG_LAYER=1`
says *"no sampleCount for color and raster available, either set defaultColorSampleCount or set
defaultRasterSampleCount or set appropriate attachments"* — but the release driver asserts inside
`MTLCompilerService` instead of erroring, so the client dies uncatchably.

How KosmicKrisp got there:

1. venus imports the scanout IOSurface as **BGRA8Unorm** (fmt 80); the guest binds a
   **BGRA8Unorm_sRGB** image (fmt 81) to it.
2. `kk_image_plane_bind` **rejected** that adoption, leaving the plane with a **NULL texture**.
3. The attachment therefore had a nil Metal texture ⟹ **zero-attachment descriptor**, while
   `no_framebuffer` was false ⟹ sample count never set ⟹ **AGX aborts**.

Fixed on `limina-kk` in three parts, folded into their feature commits: accept the sRGB/linear
pair through a texture view (root cause), clamp sample count at the encoder boundary (so this
class can never kill the process again), and judge "attachment-less" on the descriptor rather than
on the Vulkan iviews. Verified: the 0-second repro survives 240 s and the full four-app recipe
survives 300 s, with **zero** rejections, **zero** attachment-less passes, and vkmark completing
every scene at ~2200 FPS.

**Still owed: the Radar to Apple**, and confirming whether it is G13-only (see below).

## Status 2026-08-15: reproduced deterministically, and bisected to one flag

`repro.sh` reproduces it on demand, and the trigger is **`LIMINA_KK_MTLTEXTURE_SCANOUT`** — the
imported-MTLTexture scanout path that task #26 turned on by default:

| arm | runs | result |
|---|---|---|
| default (scanout path **on**) | 2 | **aborted, both at exactly 79 s** after load start |
| `LIMINA_KK_MTLTEXTURE_SCANOUT=0` | 2 | survived 300 s |

Two things make this differential trustworthy rather than a coincidence of timing:

- The two reproductions aborted at *identical* 79 s marks — this is deterministic, not a race.
- **The off-arm was verified to be doing real GPU work**, which is the way an "off" arm passes for
  free: sampled mid-run it had venus live (`Device Name: Virtio-GPU Venus (Apple M1 Max)`),
  vkmark at ~2200 FPS and glmark2 running. A silent degrade to software-2D would mean no Metal
  compiles at all and a meaningless pass.

**The benchmark is not required.** The first reproduction aborted while the gpuscore page merely
sat on its Start button, so the trigger is in ordinary compositor/venus traffic, not in the
benchmark's own draws. That widens the blast radius: any enhanced-tier session can hit this.

## Minimal reproducer: `vkmark -b effect2d`

Leave-one-out over the four-app workload (`WORKLOAD=` in `repro.sh`) isolates it to **vkmark**:

| dropped | result |
|---|---|
| firefox | aborted at 79 s |
| glmark2 | aborted at 79 s |
| vkcube | aborted at 79 s |
| **vkmark** | **survived 240 s** |

The invariant 79 s was itself the clue and should have been read sooner: an abort that lands at
the *same second* under three quite different workloads is not being driven by the workload. It is
vkmark's scene schedule — seven scenes at ~10 s each, and the abort falls exactly at the
`shading=cel` → `effect2d` transition. `effect2d` renders offscreen and then convolves, which is
the shape the anomalous configuration suggested.

```sh
WORKLOAD=vkmark VKMARK_ARGS="-b effect2d" spikes/agx-compiler-abort/repro.sh effect2d 180
```

**Aborts in 0 s**, with **41 render passes in 10 distinct configurations** — down from 404,630
passes and 76 configurations. Use this for all further work; the four-app recipe is now only
history.

**~~Do not read the truncated last block as an attachment-less pass.~~ That was wrong, and it was
the trigger.** For a whole round this file said the final `LIMINA-KK-RP` header having no
attachment lines after it was a truncation artifact — the abort firing on another thread mid-dump
— and told the next reader not to build a theory on it. It was the log printing honestly: that
pass really had no attachments. Two tells were sitting in the data:

- The cut landed **between the header and the first attachment line in every single run**.
  Truncation by a concurrent abort would wander.
- The final header carried a **different thread id** than the stream before it. Truncation of one
  thread's dump cannot produce that.

Kept here rather than deleted because the shape of the mistake is the reusable part: *absence of
output was read as evidence of an interrupted write, when it was evidence of an empty list.* The
fix was to stop inferring and **count** — the header now prints `natt=`, so "no attachments" and
"dump cut short" can never look alike again.

## It may be M1-only

The user reports the dogfood Mac — an **M4 Pro** — has run this stack for a long time without ever
aborting, while the dev Mac (**M1 Max**) aborts deterministically at 79 s. That fits the failing
call: the worker's error comes from `AGXMetalG13X`, the G13 driver, and several `ds` resource
families in `AGXCompilerCore.framework` are explicitly **GPU-generation-keyed**
(`compute_control_flow_predicate_g13`, `_g14`, `_g15*`, `_g16p_*`, `_hal200`, `_hal300`, and the
same suffixes on `late_latched_vrr_*`). A background-object variant that exists for later
generations but not for G13 would produce exactly this: a NULL bundle lookup on one machine and
silence on another.

Not confirmed — the two machines differ in more than GPU generation, and the dogfood Mac is not
ours to run experiments on. But it reframes the search: **the trigger may be a G13-specific gap**,
in which case the fix is avoidance on G13 rather than a universally-wrong render pass.

## What the abort actually is

`MTLCompilerService`'s own crash report names the failing call precisely — this is worth more
than the worker's, which only shows the downstream retry loop:

```
AGCLLVMBackgroundObjectFragmentShader::AGCLLVMBackgroundObjectFragmentShader(
    AGCLLVMCtx&, llvm::LLVMContext&, _AGCDrawBufferState const*, _AGCBackgroundObjectState const*)
  → AGCLLVMBackgroundObjectFragmentShader::buildStateless(...)
  → AGCLLVMObject::readBitcode(...)          [Failed assertion "bitcode_url"]
```

So it is the **background object fragment shader** — the program Metal runs at tile load to
honour the load actions — and it is built from `_AGCDrawBufferState`, i.e. it *is* keyed on the
render pass's attachment layout. The request type in the log (`MTLBuildOpaqueRequest - opaque`)
is the driver's internal-library path, not app shader compilation.

Read it from any future instance with:

```sh
ls -t ~/Library/Logs/DiagnosticReports/MTLCompilerService-*.ips | head -1
```

## Instrumentation and what it found

`kk-rplog-instrument.patch` (against the `limina-kk` branch; the checkout is gitignored) adds
`LIMINA_KK_RPLOG=1` to KosmicKrisp: it dumps the fully-resolved render pass — per attachment
format, storage, usage, size, sample count, load/store, plus `imageblockSampleLength` and the
tile config — immediately *before* the encoder is created, and flushes.

Two things had to be got right, both of which cost a run:

- **Thread correlation does not work.** The abort is reported on a Metal-internal
  compiler-connection thread that never appears in the dump, because Metal issues the compile off
  the thread that created the encoder. `rplog-firstseen.py` therefore uses the property the
  compile does have: Metal builds a background object **lazily, on first sight** of a
  configuration, so the trigger is one first seen near the end.
- **`tex.buffer` does not detect IOSurface-backed textures** (they report `buffer == nil`), so the
  first version logged `linear=0` for every scanout attachment and made them look absent from
  every render pass. The dump now logs `iosurf=` separately.

With that, one configuration stands out — **unique in 404,630 render passes**, first and only
sighting ~9,300 passes before the abort:

```
rt=800x600 arraylen=1 samples=0 imageblock=0 tile=0x0 tgmem=0
color[0] fmt=70 (RGBA8Unorm)   2048x2048 usage=0x17 load=Load  store=Store
depth[0] fmt=252 (Depth32Float) 800x600  usage=0x04 load=Clear store=Store
```

Anomalous in two ways: the colour attachment is **2048×2048 while the render target and the depth
attachment are 800×600**, and the depth usage is `0x4` where every other pass uses `0x7`. Both
formats are 4 bytes, so the per-pixel total is a perfectly ordinary 8.

**But no render-pass configuration reproduces in isolation.** `mtlrp.m` drives them straight at
Metal with no Vulkan and no VM, through **both** the classic API and **MTL4** (the one KosmicKrisp
actually encodes with). 26 cases, no abort:

- the candidate configuration verbatim, usages included, plus its neighbours — mismatch in the
  other direction, all three load actions, odd and tiny render-target windows;
- **IOSurface-backed** colour attachments, 2560×1440 BGRA8Unorm usage 0x17, all three load
  actions. This one mattered most to test, since the whole bug bisects to the scanout flag and a
  vehicle with no IOSurface attachment was not testing the configuration that matters. Still green;
- **attachment-less** passes (`renderTargetWidth/Height` set, no colour/depth/stencil), which
  would have a 0-byte tile layout. Metal accepts them; only a 0×0 render target is rejected, with
  a nil encoder rather than an abort.

So the render-pass shape alone is **necessary at most, not sufficient**. Something about the
surrounding state — driver state accumulated across the session, concurrent encoding on several
threads, or a resource property not visible in the descriptor — is part of the trigger.

`imageblockSampleLength` was 0 on every one of the 404,630 passes, so that field is not the index
into `blit_fast_clear_gen2_N` either.

## What actually happened

`MTLCompilerService` aborted four times in ~6 s and `limina-vmm` followed it down. The worker's
own crash report only shows the downstream effect — the abort is on
`com.apple.MTLCompilerConnectionQueue` inside `AGXMetalG13X`, after Metal retried the compile ten
times and gave up with `MTLCompilerErrorFatalError`. **The abort is inside Metal**, so we cannot
catch it; the only lever is not triggering it.

The cause is not in the `.ips` files at all — it is in the unified log:

```
MTLCompilerService (AGXCompilerCore)
  AGCLLVMObject::readBitcode(AGCLLVMCtx &, llvm::LLVMContext &, llvm::StringRef, bool)::...
  bitcode_url is NULL for bundle 'com.apple.AGXCompilerCore', filename '<private>',
  extension 'ds' and subdirectory '<private>'
```

Recover it for any future instance with (note `log show` needs a real bash — fish mis-parses the
quoted `--start`):

```sh
bash -c 'log show --start "2026-08-14 23:13:50" --end "2026-08-14 23:14:24" --style compact \
  | grep -E "MTLCompilerService\[|limina-vmm\[" | grep -viE "reportcrash|osanalytics"'
```

## What the message means

The resources exist: `/System/Library/PrivateFrameworks/AGXCompilerCore.framework/ds/*.ds`, 67
files. The background-object (fast-clear) family ships exactly **six** sizes —
`blit_fast_clear_gen2_{1,2,4,5,8,16}` plus `_meta` and `_meta_copy` — confirmed both on disk and as
string literals in the dyld shared cache. So the driver asked for a size Apple does not ship, the
bundle lookup returned NULL, and the assert fired.

**Hypothesis 1 (FALSIFIED): the number is the per-pixel byte total of the tile layout.** 1, 2, 4,
8, 16 are the standard colour sizes and 5 is D32S8, so summing across attachments would reach
unsupported totals easily (RGBA8 + D32S8 = 9; also 3, 6, 7, 10, 12). `rpcombo.c` sweeps render
passes through KK — 9 colour formats × 6 depth/stencil × 1–4 colour attachments × CLEAR/LOAD,
optionally × MSAA — printing each `TRY` line before executing so an abort names its own trigger:

```
swept 360 combinations (72 skipped as unsupported), no abort
swept 720 combinations (144 skipped as unsupported), no abort   # --msaa
```

Totals of 9, 12 and 13 all passed. **The naive byte-sum is not the parameter.** Keep the sweep —
it is the vehicle for the next hypothesis, and re-running it is wasted time.

Two follow-ups the falsification suggests, both cheap:

- A **24bpp / 96bpp attachment** would give a genuinely unsupported 3 or 12. Checked and **ruled
  out at the KK layer**: `src/kosmickrisp/vulkan/kk_format.c` has no `R8G8B8_*`, `R16G16B16_*` or
  `R32G32B32_*` entries at all, so KK never offers one as a colour attachment.
- **Size 0** — an attachment with a nil texture or a format KK maps to nothing — would also miss
  the table, and would fit the guest-triggerable host abort class (see `limina-kk-empty-clear-rect`).
  The sweep only ever used valid supported formats, which is exactly why it came back clean.

If the parameter really is the tile byte size, the fix belongs at the KK/vkr trust boundary.

## Already ruled out — do not re-run these

- **Resource exhaustion (fds/vnodes) in the compiler service.** The failing service was spawned at
  `23:14:21.668` and asserted at `23:14:21.689` — 21 ms into a brand-new process, on its first
  request. It cannot be fd-starved. (This was the first theory, from the `limina-fd-limit-crash`
  precedent; the timeline kills it.)
- **Host disk full.** 132 GB free at the time.
- **A generally broken lookup path.** A different request type (`MTLBuildFunctions - pipeline`)
  compiled successfully seconds earlier in the same run. The failing ones are
  `MTLBuildOpaqueRequest - opaque`, the driver's internal background-object path.

## Not yet done

- **Report it to Apple.** The trigger is now a 40-line self-contained Metal program
  (`mtlrp-min.m`), which is what the user was waiting for. Frame it as: *the release driver aborts
  the host process from inside `MTLCompilerService` on a render pass descriptor that its own
  validation layer rejects cleanly*. The right behaviour is the validation layer's — refuse the
  encoder — not an assert in a helper process that the client cannot catch.
- **Confirm or kill the G13-only theory.** One `mtlrp-min` run on an M4 Pro settles it, and its
  exit code is the whole answer (0 = survived, 134 = aborted). **Ask the user to run it** — the
  dogfood Mac is not ours to experiment on.

Resolved and no longer open:

- ~~Decide the mitigation~~ — moot. The root cause is fixed, so `LIMINA_KK_MTLTEXTURE_SCANOUT`
  stays on by default as task #26 intended; nothing has to be given up.
- ~~Work the 10 configurations / vary `mtlrp.m` further~~ — unnecessary. The missing **sample
  count**, not the pass *shape*, was the trigger, which is why every shape-based sweep came back
  green.
- ~~Get the filename~~ — never needed. The stack from `MTLCompilerService`'s own `.ips`
  (`AGCLLVMBackgroundObjectFragmentShader`) plus the validation-layer message identified it
  completely, so no `com.apple.system.logging` profile has to be installed on the user's Mac.
- ~~A/B `LIMINA_VREND_SHARED_TRANSFER_SYNC=0`~~ — exonerated by the actual cause; the dmabuf
  coherency fix has no path into any of this.
- `lldb` attach is not available non-interactively on this host ("cannot get permission to debug
  processes"), so the requesting stack has to come from instrumentation, not a debugger. **A
  `backtrace_symbols_fd` at the offending call was worth more than the debugger would have been**
  — it named the caller in one run, after a round of reasoning about which path it "must" be had
  already produced a wrong fix.

## Unrelated finding, worth its own look

The kernel logs `EXC_GUARD AST: type=0x5 flavor=0x1` against `limina-vmm` **routinely** — many
events under load, hours before this crash, across different VMs and pids, repeating at a small
set of subcode addresses (`0x286a48000`, `0x286a6c000`, `0x286b6c000`, …). That is
`GUARD_TYPE_VIRT_MEMORY` / `kGUARD_EXC_DEALLOC_GAP`: a `vm_deallocate` spanning an unmapped gap.
It is delivered as a soft AST so nothing dies, but it means some unmap path is over-broad.
Suspects: guest-memory teardown, the venus ring shmem unmap (`limina-venus-host-exhaustion`), blob
and IOSurface mappings. **Do not assume it causes the abort** — it predates it by hours.
