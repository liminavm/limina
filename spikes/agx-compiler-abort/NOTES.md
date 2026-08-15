# AGX Metal compiler `bitcode_url` abort — investigation notes

The worker died with SIGABRT on 2026-08-14 while the user ran the basemark web benchmark on
`Fedora-Workstation-44.enhanced.synoik.raw`. This is the working file for chasing it; nothing here
is fixed yet.

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

**Leading hypothesis:** the number is per-pixel **bytes** of the render pass's tile layout — 1, 2,
4, 8, 16 are the standard colour sizes and 5 is D32S8. Summed across attachments, unsupported
totals are easy to reach (RGBA8 + D32S8 = 9; also 3, 6, 7, 10, 12). If that holds, KosmicKrisp is
configuring an attachment combination Apple's compiler has no background shader for, which puts
this in the guest-triggerable host abort class (see the `limina-kk-empty-clear-rect` memory) and
the fix belongs at the KK/vkr trust boundary.

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

- Reproduce, with `sample-worker.sh` armed.
- Get the actual filename: either `sudo log config --mode "private_data:on"` (system-wide and
  reversible, so **ask first**) or instrument KK to log the render-pass attachment formats at the
  failing compile — the better option, per "instrument the stack you own".
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
