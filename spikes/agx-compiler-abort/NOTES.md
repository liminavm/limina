# AGX Metal compiler `bitcode_url` abort — investigation notes

The worker died with SIGABRT on 2026-08-14 while the user ran the basemark web benchmark on
`Fedora-Workstation-44.enhanced.synoik.raw`. This is the working file for chasing it.

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

Working hypothesis for *why*: the scanout path has KK adopt an imported **linear**
(IOSurface-backed) MTLTexture as a render target — the log's last lines before every abort are
`[LIMINA-KK-IMPORT] adopted MTLTexture … linear=1`. A render pass that clears into a linear,
non-tiled target plausibly asks Metal for a background-object variant Apple does not ship. Not yet
proven — the next step is to log the render-pass configuration and read the last one before the
abort.

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

- Reproduce, with `sample-worker.sh` armed. The reporter's recipe: `https://web.gpuscore.com/run`
  in Firefox with `vkmark`, `glmark2-wayland` and `vkcube` all running alongside.
- Get the actual filename. **`sudo log config --mode "private_data:on"` no longer exists** —
  macOS 26's `log config` accepts only `level`, `persist`, `stream`, `signpost-*` and
  `oversize-enabled`; un-redacting now needs a `com.apple.system.logging` configuration profile
  with `Enable-Private-Data`, which is a manual install on the user's Mac. So the practical route
  is the one that was preferable anyway: **instrument KK** to log the render-pass attachment
  formats at the failing compile, per "instrument the stack you own".
- A/B once with `LIMINA_VREND_SHARED_TRANSFER_SYNC=0` to retire by measurement the question of
  whether the same-day dmabuf coherency fix is implicated. Reasoning says no (nothing in the stack
  touches it, and a `glFinish` after a texture upload has no path into Apple's shader compiler),
  but that is reasoning, not a measurement.

## Unrelated finding, worth its own look

The kernel logs `EXC_GUARD AST: type=0x5 flavor=0x1` against `limina-vmm` **routinely** — many
events under load, hours before this crash, across different VMs and pids, repeating at a small
set of subcode addresses (`0x286a48000`, `0x286a6c000`, `0x286b6c000`, …). That is
`GUARD_TYPE_VIRT_MEMORY` / `kGUARD_EXC_DEALLOC_GAP`: a `vm_deallocate` spanning an unmapped gap.
It is delivered as a soft AST so nothing dies, but it means some unmap path is over-broad.
Suspects: guest-memory teardown, the venus ring shmem unmap (`limina-venus-host-exhaustion`), blob
and IOSurface mappings. **Do not assume it causes the abort** — it predates it by hours.
