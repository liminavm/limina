# piglit on limina's classic-virgl path

piglit's buffer, PBO and texture-transfer groups run in a stock guest and an enhanced guest, against
the same selection the upstream rig runs (`spikes/upstream-repro/virgl-pbo-upload-wait/`), so the
three compare test for test.

    # in each guest, once (needs network):
    ./setup-guest.sh                     # builds piglit c3aa5b96d in tree (Vulkan/CL tests off)
    # on the host, one per guest; reboots and resumes after every host crash:
    spikes/piglit-virgl/drive.sh <clone>.raw <out-dir>
    spikes/piglit-virgl/compare.py <stock>/results.json.bz2 <enh>/results.json.bz2 [rig.tsv]

`run.sh` runs serially (`-1`) and fsyncs every result (`-s`), on `PIGLIT_PLATFORM=gbm`, with a
300 s timeout per test. gbm gives default-framebuffer tests a framebuffer, which surfaceless did
not. Serial runs plus fsync make the test that took the VM down the one recorded as `incomplete`,
and that record survives the crash. Without `-s`, the killer's placeholder dies with the guest's
page cache and every resume runs it again.

## Results

Measured 2026-10-06 on the M1 Max dev Mac. Host: virglrs `b91824b` (vrend, GLES flavour, over
zink-on-KosmicKrisp), KK `43152beb621`; 4 vCPUs, 8 GiB, EFI+venus coexist boot. GL_RENDERER in
both guests: `virgl (zink Vulkan 1.4(Apple M1 Max (MESA_KOSMICKRISP)))`.

| Guest | Mesa | pass | fail | timeout | guest crash | host crash | skip |
|---|---|---|---|---|---|---|---|
| stock (`stock.test` clone) | Fedora `26.2.3-1.fc44` | 1563 | 78 | 27 | 2 | 0 | 201 |
| enhanced (`enhanced.test` clone) | limina `26.2.3-3.limina.fc44` | 1562 | 80 | 27 | 1 | 0 | 201 |

`setup-guest.sh`'s `dnf install` moved the stock clone from the frozen image's Mesa `26.1.8` to
Fedora's `26.2.3-1`. The two arms therefore differ only by limina's guest patches, on one base.

**No host crashes.** Every test ran to a result in one boot per arm, and neither worker log has a
panic or abort. This selection is the regression check for guest-reachable host aborts on the GL
path: before KK `2315d532b3d` (a 3D texture's 2D_ARRAY alias kept the 3D mip count, and Metal
aborted) and virglrs's texture-buffer sampler-view fix, it took the VM down twelve times.

**Stock vs enhanced.** The two guests fail the same tests. Two flake between runs and arms, so
neither counts as a difference:
- `arb_get_texture_sub_image-get` and `-getcompressed` fail on both, and segfault the test process
  (SIGSEGV, in the guest) on some runs under either Mesa.
- `vbo-subdata-*` fail when a fence wait gives up ("waiting got error - 16, slow gpu or hang?"). Run
  alone, all five pass 3/3 on both guests.

**Rerun a list of tests in a booted guest.** `rep.sh <list> <n>` reads lines of
`<piglit test name>\t<command under bin/, without -auto -fbo>`, runs each test `n` times on gbm, and
marks the runs where a fence wait gave up `(stall)`.
`./piglit print-cmd --format "{name}\t{command}" gpu` in the guest's piglit tree gives every
test's command to build a list from. `rep.sh` changes into `~/piglit`, so pass the list's absolute
path.

**A/B two KK builds.** `ab.sh <disk.raw> <kk-icd.json> <regex> <out-prefix>` boots a piglit guest
on one KosmicKrisp ICD, runs the gpu tests matching the regex once, and keeps the list, results
and worker log; run it per build on the same disk clone and `join` the two result files (the
header has the line). The KK guest-input fixes were checked this way: the transform-feedback and
query tests, the tessellation-heavy selection and the indirect/multi-draw tests each gave
identical results on both builds.

**Trace KK's command stream.** `kk-cmdtrace.patch` applies to the `limina-kk` tree and adds a
`[KKCT]` line to stderr (the worker log) for each command a KK command buffer records: rendering
begin/end with the attachment and load op, every draw, barriers, copies, blits, each submit, and
the timer-query ops (reset, `vkCmdWriteTimestamp2` with its stage and whether a render encoder is
live, `vkCmdCopyQueryPoolResults`, and each counter-heap resolve), all tagged with the command
buffer. It is on only with `LIMINA_KK_CMDTRACE=1`. Build it in a
worktree with its own build dir and boot with `LIMINA_KK_ICD` set to that build's devenv ICD, so
the shared build stays untouched. Comparing the trace of a failing run with one a sync makes pass
is what found the meta push-set clobber behind `sgis_generate_mipmap@gen-teximage`.

## Against the upstream rig

The same 1871-test selection ran on the upstream rig, measured 2026-10-05: stock QEMU 10.2 +
virglrenderer 1.3.0 on an Intel host, guest Mesa `main` b39d173ca93, surfaceless, so 125 tests never
reached the driver there (`spikes/upstream-repro/virgl-pbo-upload-wait/`). The limina side is the
2026-10-06 run above.

**Fail on both stacks** (guest Mesa or vrend behaviour, so upstream candidates):
- `copyteximage 3d`.
- `arb_get_texture_sub_image-get`/`-getcompressed` on compressed 2D. With Fedora's Mesa,
  `-getcompressed` segfaults in the guest.
- `getteximage-targets 2d_array s3tc`: zeros from layer 6 on.
- `fbo-readpixels-depth-formats`: float depth reads 0.999985 for 1.0, a 24→32-bit expansion off by
  0x80.
- `arb_shader_image_load_store@early-z`: half the occlusion count.
- `arb_shader_image_load_store@invalid`: out-of-bounds and invalid-format image atomics return
  garbage instead of zero.
- `ext_transform_feedback2@draw-auto offset`.
- `arb_texture_buffer_object@max-size` (128 MiB, flaky on the rig).

**Fail only on the rig** (pass on limina):
- `arb_transform_feedback_overflow_query-basic` and `arb_query_buffer_object@qbo`.
- `getteximage-targets cube_array s3tc`.
- `teximage-colors` RGB 3_3_2, off by one LSB.
- `tessellation triangle_fan flat_first`.

**Fail only on limina**, about 100 tests, the same on the stock and enhanced guests. Running each in
the enhanced guest under zink-on-venus as well (`MESA_LOADER_DRIVER_OVERRIDE=zink`, which reaches
KK without vrend) splits them:
- **Pass under zink-on-venus, so they are in virglrs vrend.** Its GLES 3.1 flavour has no geometry
  shaders or tf3, and lacks image caps. That accounts for the image load/store, SSBO and most
  transform-feedback failures, plus BPTC float uploads, `getteximage-formats`, `pos-array` and DSA
  `transformfeedback-buffer*`.
  - Texture buffers reach vrend because KK emulates R32G32B32 texel buffers ("kk: emulate
    R32G32B32 uniform texel buffers", `80be4bf1b3b` on `limina-kk`), which gives host zink
    `OES_texture_buffer`; 22 texture-buffer tests pass under vrend. Still failing there: the legacy
    ALPHA, LUMINANCE, LUMINANCE_ALPHA and INTENSITY formats in
    `arb_texture_buffer_object@formats (fs|vs, arb)`.
  - All 27 timeouts (300 s) are indexed draws under transform feedback, which GLES refuses.
  - virglrs's desktop flavour passes 69 more of these and regresses 26 uploads.
- **Fail under zink-on-venus too, so they are in KK or zink:**
  `arb_draw_indirect-draw-elements-prim-restart-ugly` (passes on virglrs's desktop flavour),
  `arb_shader_image_load_store@host-mem-barrier`, `ext_transform_feedback2@counting with pause`
  and the five geometry-shader xfb tests.
- **Fence waits that give up.** "waiting got error - 16, slow gpu or hang?" after 15–40 s turns a
  pass into a fail on either guest. `vbo-subdata-*` fail this way in some full runs and pass 3/3 run
  alone (`rep.sh`), so treat a lone difference with that line as a host stall.

What is still owed on these is in `docs/hardening-backlog.md` (GPU correctness, and the KosmicKrisp
entries).
