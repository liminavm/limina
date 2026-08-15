# CPU-written LINEAR dmabuf loses GPU visibility — root-caused 2026-08-14

Reported by synoik (`vmm-issue-dmabuf-cpu-write-coherency.md` in their tree): a
`gbm_bo_map` → write → `gbm_bo_unmap` on a LINEAR `Argb8888` dmabuf is not visible to a
subsequent GPU read of the same buffer through venus; the sample returns the buffer's
**previous** contents. Backlog entry: `docs/hardening-backlog.md`.

**Verdict: the transfer is issued and lands — it just runs too late.** The guest's write
reaches the host as a virtio-gpu control-queue command executed by libkrun's single gpu
worker thread, while the venus read is executed by virglrenderer's **own ring thread**
(`vkr_ring_start` → `thrd_create`, `src/venus/vkr_ring.c:811`) straight out of the shared
ring memory. Two host execution paths, no ordering between them, and the guest cannot
impose one: `DRM_IOCTL_VIRTGPU_EXECBUFFER` is fire-and-forget, so `gbm_bo_unmap()` returns
long before the transfer is dequeued. The consumer samples one write behind.

## Reproducer

`probe.c` — gbm LINEAR ARGB8888 bo, imported once into venus as a
`VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT` image, then per pass: CPU-fill through the map,
`vkCmdCopyImageToBuffer`, compare. Build and run **in-guest** (enhanced tier):

```sh
gcc -O1 -g -o probe probe.c $(pkg-config --cflags --libs gbm) -lvulkan
export GALLIUM_DRIVER=virgl MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu \
       VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json
./probe --passes 10
```

The ssh env matters: a non-login shell does not source
`/etc/environment.d/90-limina-zink.conf`, and without it there is no venus device at all.

Measured on `Fedora-Workstation-44.enhanced.synoik.raw` (clone), host at virglrenderer
`7caf247b`:

| variant | result |
| --- | --- |
| baseline | **8–10 of 10 stale**, each pass returning exactly the *previous* pass's colour |
| `--sleep-ms 1` (or more) | 0/10 |
| `--touch-after-write` (guest waits for the bo to go idle) | 0/10, three runs |
| `--gpu-writer` (GPU clear as producer) | 0/10 |
| `--reimport` (fresh VkImage every pass) | 7/10 stale |

Read the table as: not a dropped transfer (a 1 ms pause is enough), not an import-time
snapshot (`--reimport` still fails), and unaffected when producer and consumer share the
venus context (`--gpu-writer`). Pass 0 often passes because the first venus submission
takes ~9 ms of pipeline setup — which is the reporter's "the first write always works".

## The timestamp proof

`vrend-coh-instrument.patch` (apply to `third_party/virglrenderer`, rebuild with
`scripts/build-virglrenderer.sh`, run the VM with `LIMINA_COH_TRACE=1`) stamps
`CLOCK_REALTIME` at each `vrend_renderer_transfer_write_iov`; `LIMINA_COH_TRACE=1` on the
probe stamps the guest side. The guest clock is anchored to the host's, and the two series
carried a constant 64.80 ms offset (identical inter-event deltas, so the alignment is not a
guess). Corrected to guest time, pass 1:

```
.782226  guest: gpu-read-submit
.782234  host:  transfer_write BEGINS          <- after the read was submitted
.782576  guest: gpu-read-done
.782764  host:  transfer upload completes      <- after the read finished
```

## What does NOT fix it

A completion barrier (`glFinish`) at the end of `vrend_renderer_transfer_write_iov` for
iosurface-backed resources. Tried, **verified loaded and hit** (4 transfers → 4 barriers in
the trace), still 10/10 stale — because the problem is when the upload *starts*, not
whether it has finished. Reverted; it would have been a pure GL-pipeline stall on every
shared upload. The resource is EGLImage-backed (`storage_bits=0x4b`, `pbo=0`), i.e. the GL
texture's storage IS the venus-shared IOSurface, so there is no host-side copy to chase.

Host-side ordering is not the answer either: the vrend GL context is current on the worker
thread, so the ring thread cannot execute that work — any host barrier degenerates into
"ring thread waits for worker thread", which is the same shape as the present-fence
injection in `try_park_present` and invites a deadlock, and it would tax every venus batch
in the default coexist config to fix a path nothing takes today.

## The fix, when it is worth doing

Guest mesa virgl: on unmap of a **write** map of a `PIPE_BIND_SHARED` resource, flush and
wait for the bo to go idle — exactly the `--touch-after-write` semantics, proven 0/10 on an
unmodified host. Gating on SHARED mirrors the host's own gate in
`vrend_resource_iosurface_init`, so both sides key off one condition. It belongs on the
**`limina-guest`** branch (`/Volumes/mesa-cs/mesa-guest`, base mesa-26.1.5) — not
`limina-kk` — and carries the full delivery chain: fork commit →
`scripts/export-mesa-guest-patches.sh` → mesa RPM → `install-enhanced.sh` over the enhanced
images → `docs/images.md` component versions.

Two consequences to accept rather than fight:

- **The stock tier keeps the bug** (stock mesa has no such wait). Consistent with the
  two-tier guarantee — a degradation, not a boot failure — and the long-term erase is
  backing shared bos with host-visible blobs so there is no transfer to order at all (and
  no upload cost either).
- **The seam is not transfer-specific.** *Any* control-queue work races the ring, so a
  vrend GL render into a shared bo consumed by venus has the same hazard for a consumer
  that skips implicit sync. `--touch-after-write` working proves the guest kernel does
  track the fence on the bo, so `VIRTGPU_WAIT`/sync-file consumers are safe; a bare Vulkan
  importer is not. Open question, not yet probed.
