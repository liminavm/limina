# virgl: don't wait for a read-only map of a clean resource

**Bug.** On virgl, `glTexSubImage2D` from a bound `GL_PIXEL_UNPACK_BUFFER` waits for the host on
every call. virgl does not set `prefer_blit_based_texture_transfer`, so `st_TexSubImage` never takes
the GPU PBO path (`try_pbo_upload`): it falls back to `_mesa_store_texsubimage`, which maps the PBO
for reading on the CPU. The PBO is busy — the transfer of the data the application just wrote into
it is still queued — and `virgl_resource_transfer_prepare` waits for every synchronized map of a
busy resource. A client-memory upload takes `texture_subdata` instead and never waits.

The wait is unnecessary when the map is read-only and the resource is clean: every command that
writes a resource on the host (streamout, SSBO/image stores, copies, blits, clears, queries, staging
writes) marks it dirty, and a dirty resource takes the readback path. A clean resource's guest
storage is current, and whatever is still in flight can only read it.

Found by the Firefox-on-limina perf work on an M1 host (its notes live outside this repo):
Firefox's accelerated canvas streams every path mask and image through one PBO and spent 6.8 s of a
15 s Basemark Canvas run blocked in these waits; skipping the PBO in Firefox gained +50% on that
test. The waits are the same on an upstream stack, so the fix belongs in Mesa.

**Reproducer.** `virgl-pbo-upload-wait.c`: 300 iterations of draw-with-atlas → `glBufferSubData` a
64x64 tile into a 1 MiB `STREAM_DRAW` PBO → `glTexSubImage2D` from the PBO into the atlas → draw,
flushing every 10 iterations; `cpu` mode passes a client pointer instead. `VIRTGPU_WAIT` is counted
and timed by an `ioctl()` interposer; every tile is read back at the end to check the contents.

    cc -O2 -o virgl-pbo-upload-wait virgl-pbo-upload-wait.c -lEGL -lGLESv2 -ldl
    ./virgl-pbo-upload-wait pbo
    ./virgl-pbo-upload-wait cpu

## Results

QEMU guest, virgl on Intel Iris Plus G7, virglrenderer 1.3.0; 3 runs each.

| Mesa | Mode | Time / iteration | `VIRTGPU_WAIT` in the loop | Contents |
|---|---|---|---|---|
| `main` b39d173ca93 | client memory | 0.18–0.19 ms | 3 calls, ~0 ms | all correct |
| `main` | PBO | 1.11–1.14 ms | 32 calls, ~305 ms, max 12–14 ms | all correct |
| `main` + fix | client memory | 0.16–0.19 ms | 3 calls, ~0 ms | all correct |
| `main` + fix | PBO | 0.16–0.19 ms | 3 calls, ~0 ms | all correct |

Measured 2026-10-05. Every wait on `main` is the PBO read map
(`virgl_drm_resource_wait` ← `virgl_resource_transfer_prepare` ← `_mesa_bufferobj_map_range` ←
`_mesa_validate_pbo_teximage` ← `st_TexSubImage`); the destination texture never waits.

**piglit, main vs fix.** piglit c3aa5b9, `quick` profile filtered to every group a read-only map of
a buffer or texture can reach: PBOs, buffer mapping and storage, copy/clear buffer, VBOs, UBOs,
SSBOs, image load/store, TBOs, query buffers, transform feedback, indirect draws, DSA, and the
teximage / texsubimage / getteximage / readpixels tests. 1871 tests (10373 with subtests),
`PIGLIT_PLATFORM=surfaceless_egl`. Summary: no regressions; the 43 crashes and the remaining
failures are identical on both. 125 of the 1871 never reached the driver: 82 could not find their
`.shader_test`/compiler files (piglit built out of tree, so build it in tree next time) and the 43
"crashes" are piglit aborting in `run_test` for want of a default framebuffer on the surfaceless
platform. The 13 real failures are in `docs/hardening-backlog.md`. The summary's two "fixes", `max-ssbo-size@vs` and
`arb_texture_buffer_object@max-size` (128 MiB buffers), are flaky: re-run 3× each, main failed the
TBO one once and the fix passed every run.

**Still owed before filing:** a Firefox Canvas Test A/B with Firefox's PBO path re-enabled (the pass
criteria in the firefox-perf note).

## Opting virgl into blit-based transfers instead

virgl sets `caps->texture_transfer_modes = false`, overriding the Gallium default
(`PIPE_TEXTURE_TRANSFER_BLIT`), so `st_TexSubImage` never takes the GPU PBO path. Turning it on in
main removes the waits too (0.18–0.26 ms per iteration, no `VIRTGPU_WAIT`), but the uploads are
lost: 255 of 256 tiles read back as zeros, with nothing in the host log. It needs that bug found
first and a much wider conformance run, since the cap also changes texture downloads and
`ReadPixels`; the fix here does not conflict with it.

## MR description (draft)

> **virgl: don't wait for a read-only map of a clean resource**
>
> `glTexSubImage2D` from a PBO on virgl waits for the host on every call: with no blit-based
> transfers, the state tracker maps the PBO for reading on the CPU, and `transfer_prepare` waits
> because the PBO is busy with the queued transfer of the data just written into it. Nothing in
> flight can change a clean resource's guest storage — every host-side write marks it dirty — so a
> read-only map that needs no readback can skip the flush and the wait. Blob resources still wait.
>
> Reproducer attached: a draw / BufferSubData / TexSubImage-from-PBO / draw loop. Under QEMU 10.2 +
> virglrenderer 1.3.0 on an Intel host, the PBO loop goes from 1.1 ms to 0.18 ms per iteration —
> the same as the client-memory loop — and its 32 waits totalling ~305 ms disappear. Every uploaded
> tile is verified. Found as Firefox's accelerated canvas spending most of its time in these waits.

The commit is on `wip/virgl-pbo-wait` in `/Volumes/mesa-cs/mesa-upstream`, on top of
`upstream/guest-2026-10`.
