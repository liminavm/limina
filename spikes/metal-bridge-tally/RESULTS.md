# Metal-bridge tally: what a guest-side KosmicKrisp would put on the wire

The proposal under test: run KosmicKrisp in the guest and carry its Metal bridge calls
(`src/kosmickrisp/bridge/`, the only place KK touches Metal) over a new virtio-gpu context
type to a thin host replayer, instead of venus (Vulkan) and vrend (GL) carrying API calls to a
host KK. Step 1 of the measure-first plan: per frame, how many bridge calls and wire bytes
would that be, against the venus and vrend streams for the same frames. Step 2 is answered
here only as far as the tally's clock reaches.

Runs of record: `runs/r3/` (benchmarks) and `runs/r4/` (the browser); each `analysis.md` is the
full per-workload, per-function breakdown. `runs/r1`, `runs/r2` predate the instrument fixes
below and undercount; do not cite them.

## Method

- **KK side.** `gen_tally.py` instruments all 197 public bridge entry points (generated diff:
  `kk-bridge-tally.patch`, against the `[kosmickrisp]` pin `2315d532b3d`). Each call counts
  once (outermost only), with an estimated wire size: 8 B header, 4 B per object id, scalars at
  `sizeof`, structs by value, arrays at `count × elem`, MSL source and `replaceRegion` pixels at
  their length; guest-memory pointers as 8 B. Calls are classed async / create / sync / local
  (free in the guest: `mtl_get_contents`, descriptor sub-object getters) / diag. Built into its
  own build dir (`build-kk-tally.sh`), never the shared host ICD.
- **Stream side.** virglrs's existing venus and vrend tallies (`VIRGLRS_SUBMIT_STATS`), plus
  `virglrs-nested-stream-bytes.patch`: venus records command buffers into shared-memory streams
  that `vkExecuteCommandStreamsMESA` only references, and the stock tally counted the envelope
  alone (drawstorm read 1.4 M cmd/s in 43 KB/s). vrend bytes are submit dwords × 4.
- **Same frames.** One boot, both tallies on the same 5 s windows; `analyze.py` keeps a window
  only if it lies wholly inside one workload. GL legs print `GL_RENDERER` (all six verified on
  the intended path). F44 enhanced clone, kernel `7.1.13-limina16k`, guest mesa
  `26.2.3-3.limina`, 4 vCPU / 4 GiB, offscreen benchmarks.
- **Folded messages** merge descriptor-builder calls (`*_descriptor_set_*`, `*_descriptor_get_*`,
  commit-option handlers) into the one message that consumes them — what a real protocol would
  send. It is an upper bound: render-pass descriptor creates and their releases still count, and
  a protocol would fold those into encoder begin too. **Query round trips** are sync calls that return a Metal-chosen value; fence waits
  (`mtl_shared_event_wait_until_signaled_value`) are split out, since a guest KK would block on a
  host-signalled fence rather than ask a question.

Cost of the instrument: ~11 ns per bridge call armed (+8 % on a 10 k-draw host storm); unarmed
is within noise of the shared build. Counts are load-independent; the thread-time columns were
taken with another session's piglit VM running and are indicative only.

## Results (per frame at each benchmark's own fps)

| workload | fps | KK msgs/frame (folded) | stream cmds/frame | KK B/frame | stream B/frame | KK / stream bytes | query RTs/frame | KK bridge non-wait thread-s/s | venus busy thread-s/s |
|---|---|---|---|---|---|---|---|---|---|
| vkmark (venus) | 3,215 | 89 (38) | 7.0 | 1,604 | 377 | **4.26** | 0.00 | 0.15 | 0.21 |
| drawstorm, 1000 draws (venus) | 843 | 2,113 (2,068) | 2,047 | 58,782 | 94,104 | 0.62 | 0.01 | 0.10 | 0.52 |
| glmark2-es2 offscreen (zink→venus) | 4,712 | 139 (61) | 51.6 | 2,463 | 3,137 | 0.79 | **0.83** | 0.23 | 0.45 |
| glmark2-es2 offscreen (vrend) | 5,206 | 154 (65) | 13.2 | 2,732 | 4,536 | 0.60 | **1.33** | 0.21 | — |
| glmark2-build apitrace (zink→venus) | 52 | 137 (71) | 56.9 | 3,216 | 3,231 | 1.00 | 0.13 | 0.04 | 0.05 |
| glmark2-build apitrace (vrend) | 54 | 104 (43) | 8.6 | 1,839 | 4,398 | 0.42 | 0.05 | 0.02 | — |
| WebGL aquarium, 20k fish, Firefox (vrend) + gnome-shell (r4) | 48 | 35,661 (33,870) | 51,627 | 1,290,567 | 5,845,973 | **0.22** | **27** | 0.17 | — |

The aquarium leg is the shipped desktop path: Firefox WebGL on the image's default GL (vrend),
composited by gnome-shell, whose traffic is in the same windows (the real desktop, not
subtracted). Its fps is read off the window capture (`fps: 48`, 1024×1024 canvas, display not
pinned). It is about 15 k draws per frame, each `mtl_set_address` + `mtl_draw_indexed_primitives`
(~64 B), against vrend's ~3.5 commands and ~390 B per draw, mostly inline uniform uploads.

vk-replay (gfxreconstruct vkcube, ~0.1 s per process) is left out of the table: it is device
and pipeline churn, not frames. There KK sends 4.0× venus's bytes, almost all MSL source in
`mtl_new_library` (15.6 MB/s), and ~780 query round trips/s.

## What it says

1. **Bytes are not the problem when commands are recorded per frame.** For streams that
   record every frame, a Metal-level wire is the same size or smaller: 0.6–1.0× venus, 0.4–0.6×
   vrend, and 0.22× vrend for the browser (1.3 MB against 5.8 MB per frame). A draw is `mtl_set_address` + `mtl_draw_primitives`, 56 B, against ~92 B for
   venus's push-constants + draw. Folded, KK sends about one message per venus command
   (1.0–1.25×). vrend commands are coarse, so there it is about 5 messages per command, in fewer bytes.
2. **Pre-recorded command buffers invert that.** vkmark records its command buffers once and
   resubmits them: venus carries 7 commands per frame (a submit and its sync), and the host KK
   re-encodes the Metal for every submit (Metal command buffers are one-shot). With KK in the
   guest, that re-encoding crosses the wire every frame: **4.3× the bytes, 5.5× the messages.**
   Apps that reuse command buffers are exactly where venus is cheapest and this design is most
   expensive. Reaching parity there would need replayable host-side recordings (indirect command
   buffers, or a host cache of encoded passes), which is a protocol of its own.
3. **Query round trips are the real hazard: 1–27 per frame, from object creation.** Two
   sources. Image views created per frame (zink) each ask `mtl_texture_get_gpu_resource_id`
   twice (`kk_image_view.c`): glmark2 0.8–1.3 per frame. The browser creates about 11
   buffers per frame, and each one asks `mtl_heap_buffer_size_and_align_with_length` and
   `mtl_buffer_get_gpu_address`: about 22 per frame, plus 2.6 texture ids. KK writes those values
   into argument buffers on the CPU, so in a split design each one waits on the host. At 27 per
   frame, any realistic round-trip latency gates the frame rate (venus today: 1.5 µs with the ring hot, 0.6–1.4 ms once it has parked;
   the cost in a busy frame is unmeasured). All three can be designed out (`spikes/metal-heap-addressing/`).
   Buffer size/align is a constant. A buffer placed in a heap has address `heap base + offset`.
   Texture ids are process-global and host-allocated, so they need a guest handle mapped through
   a table the host fills. That changes KK's allocator and descriptor path, not just the bridge.
4. **Host CPU would roughly halve, not vanish** (indicative). Time inside the bridge, excluding
   fence waits, is what a replayer would spend in Metal. It is 0.23 thread-s/s against venus's
   0.45 for glmark2, and 0.10 against 0.52 for drawstorm. The rest (decode, KK's
   Vulkan→Metal work) moves to guest vCPUs on the same cores. The structural win is the host
   shrinking to a replayer. Throughput is still unproven.
5. **The byte counts assume CPU-written state is memory both sides share.** KK writes
   descriptors, push constants and argument tables through `mtl_get_contents`, and the wire
   figures assume the host GPU reads that memory in place. That works in the host-owned
   direction: KK's memory is a shared placement heap plus a buffer aliasing all of it, and the
   venus blob path already maps such memory into guests. A guest driver can sub-allocate
   host-owned heaps mapped once into its physical space. The measurement is in
   `spikes/metal-heap-addressing/`.

## Verdict for step 1

The transport-volume question comes out favourable for per-frame recording, including the
browser (0.22× vrend's bytes). It comes out unfavourable for reuse-heavy Vulkan. Two
design prerequisites surfaced that the original proposal did not name: delivering Metal-chosen
values without round trips, and CPU-written state in memory the guest owns. Both are sketched
and measured in `spikes/metal-heap-addressing/` (`SKETCH.md`, `RESULTS.md`).

## Reproducing

```sh
python3 gen_tally.py <mesa worktree at the kosmickrisp pin>   # then build-kk-tally.sh
git -C third_party/virglrs apply spikes/metal-bridge-tally/virglrs-nested-stream-bytes.patch
cargo xtask build
cp -c Fedora-Workstation-44.enhanced.raw <clone>
spikes/metal-bridge-tally/run-tally.sh <clone> spikes/metal-bridge-tally/runs/<name>
python3 spikes/metal-bridge-tally/analyze.py spikes/metal-bridge-tally/runs/<name>
```

`run-tally.sh` pins its own ssh port (`SSH_PORT`, default 2291) and checks a per-boot token
in the guest before every command. On a host where other sessions' VMs come and go, the
auto-allocated 2222+ ports are reused by whichever VM boots next.
