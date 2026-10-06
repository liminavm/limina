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

Measured 2026-10-05 on the M1 Max dev Mac. Host: limina `118ab2b6`, virglrs `b91824b` (vrend over
zink-on-KosmicKrisp), KK `2315d532b3d`; 4 vCPUs, 8 GiB, EFI+venus coexist boot. GL_RENDERER in
both guests: `virgl (zink Vulkan 1.4(Apple M1 Max (MESA_KOSMICKRISP)))`.

| Guest | Mesa | pass | fail | timeout | host crash | skip |
|---|---|---|---|---|---|---|
| stock (`stock.test` clone) | Fedora `26.2.3-1.fc44` | 1516 | 126 + 1 guest crash | 27 | 0 | 201 |
| enhanced (`enhanced.test` clone) | limina `26.2.3-3.limina.fc44` | 1516 | 127 | 27 | 0 | 201 |

`setup-guest.sh`'s `dnf install` moved the stock clone from the frozen image's Mesa `26.1.8` to
Fedora's `26.2.3-1`. The two arms therefore differ only by limina's guest patches, on one base.

**No host crashes.** Every test ran to a result in one boot per arm, and neither worker log has a
panic or abort. This selection is the regression check for guest-reachable host aborts on the GL
path: before KK `2315d532b3d` (a 3D texture's 2D_ARRAY alias kept the 3D mip count, and Metal
aborted) and virglrs's texture-buffer sampler-view fix, it took the VM down twelve times.

**Stock vs enhanced.** The two guests fail the same tests. The one difference is
`arb_get_texture_sub_image-getcompressed`, which fails on both but segfaults the test process
(SIGSEGV, in the guest) under Fedora's Mesa. limina's guest Mesa patches change nothing else this
selection measures.

The failure split against the upstream rig is in `docs/hardening-backlog.md`, in the two piglit
entries under GPU correctness.
