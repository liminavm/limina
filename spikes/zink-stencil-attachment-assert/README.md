# zink-stencil-attachment-assert — the crash report, kept because the binary is not

A shipped-build zink assert SIGABRTed the dogfood worker. The dylib that crashed no longer
exists: the fix rebuilt it in place on every machine that had a copy. This directory keeps the
crash report and everything needed to read it without that binary.

## What crashed

`limina-vmm` pid 49248, 2026-09-04 22:51:14 -0300, after 29 h of session (launched 09-03 17:56).
`EXC_CRASH` / `SIGABRT`, faulting thread **167 `limina-v:gdrv0`** — a gallium threaded-context
worker, not a venus ring thread:

```
libsystem_c            __assert_rtn
libgallium-26.3.0-devel begin_rendering.cold.5      zink_context.c:0
libgallium-26.3.0-devel begin_rendering             zink_context.c:3335
libgallium-26.3.0-devel zink_batch_rp               zink_context.c:3636
libgallium-26.3.0-devel zink_draw<MULTIDRAW, DYNAMIC_STATE2, true, false>
libgallium-26.3.0-devel tc_batch_execute
libgallium-26.3.0-devel util_queue_thread_func
```

`zink_context.c:3335` is upstream's own valid-usage tripwire (`887f72990ed6`, Mike Blumenkrantz,
2023-03-17, `/* validate zs VUs: attachment must be null or format must be valid */`):

```c
assert(!ctx->dynamic_fb.info.pStencilAttachment ||
       ctx->gfx_pipeline_state.rendering_info.stencilAttachmentFormat);
```

The guest workload that drove the draw is unknown — a Dock-launched worker's stderr is not
captured, so nothing named it.

Analysis, the derived mechanism and the ruled-out alternative: `docs/hardening-backlog.md`,
§ *Guest-reachable aborts*. Why a shipped build could assert at all, and the tripwire that now
prevents it: `scripts/build-app.sh`, `docs/graphics.md` § 7.

## Provenance of the binary, and how to rebuild it

| | |
|---|---|
| image | `libgallium-26.3.0-devel.dylib`, **UUID `12A29076-6F9A-3A4C-9D31-9BC9B0436972`** |
| built from | `/Volumes/mesa-cs/mesa` branch `limina-kk` @ `552edc3f62f`, VERSION `26.3.0-devel` |
| build dir | `/Volumes/mesa-cs/build-zink-kk`, installed to `zink-kk-prefix` |
| meson | `-Dplatforms=macos -Dvulkan-drivers=kosmickrisp -Dgallium-drivers=zink -Dopengl=true -Dgles2=enabled -Degl=enabled -Dglx=disabled -Dglvnd=disabled -Dshared-llvm=enabled -Dzstd=disabled -Dprefer_static=true -Dbuildtype=debugoptimized -Dmoltenvk-dir=/opt/homebrew/opt/molten-vk -Degl-native-platform=surfaceless` |
| asserts | **`b_ndebug=if-release`** — i.e. on. That is the defect; the shipped value is `true` now. |

To symbolicate a frame this README does not already resolve, rebuild at that rev with
`b_ndebug=if-release` and `atos -o <dylib> -arch arm64 -l 0x100000000 <0x100000000 + imageOffset>`.
The result is functionally equivalent, not bit-identical, so treat an offset that lands
ambiguously as ambiguous. Frames already resolved above needed no such caveat: the crash-report
UUID matched the on-disk dylib exactly at the time they were read.

The rebuild needs mesa's full tool env or it silently reconfigures the build dir with the wrong
tools — brew `bison` (Apple's `/usr/bin/bison` 2.3 fails the glcpp grammar), brew `llvm-config`,
and `third_party/venv-mesa`. `ninja` re-runs meson, so a bare `meson configure && ninja` from a
plain shell is not enough.

## The report

`limina-vmm-2026-09-04-225114.ips`, reformatted (one JSON metadata line, then the body) and with
`crashReporterKey` / `bootSessionUUID` / `sleepWakeUUID` / `incident_id` redacted — per-machine
identifiers, no diagnostic content, and this repo is public. Nothing else is changed; it carries
no home-directory paths and every loaded image is under `/Applications` or `/System`.
