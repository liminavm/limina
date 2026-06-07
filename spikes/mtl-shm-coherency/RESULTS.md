# Spike: venus host-visible blob coherency on the 16 KiB hv_vm_map path (#28)

**Question:** venus GL renders on the Apple GPU, but `glReadPixels` from an FBO returns
`PIXEL=0,0,0,0` (black) and the venus *feedback* sync path hangs — both read a host-visible
blob the GPU/host wrote. Is the break on the **host** side (the shm-backed Metal buffer isn't
GPU→CPU coherent) or the **guest** side (the guest's `hv_vm_map`'d view doesn't see writes the
host sees)?

## Finding 1 — the HOST side is fully coherent (`test.m`)

`vkr_mtl_shm_alloc` (virglrenderer `src/venus/vkr_metal_helpers.m`) backs venus host-visible
memory with `shm_open()` + `mmap(MAP_SHARED)` + `[dev newBufferWithBytesNoCopy:shm_ptr
options:MTLResourceStorageModeShared]`. `test.m` replicates exactly that, has the GPU fill the
buffer (`blit fillBuffer value:0xAB`), `waitUntilCompleted`, then reads the CPU's mmap view with
**no** synchronize/invalidate:
```
before GPU: byte[0]=0x00 byte[100]=0x00
device=Apple M1 Max
after GPU (CPU mmap view): byte[0]=0xab byte[100]=0xab byte[last]=0xab
RESULT: host CPU SEES GPU writes to shm-backed Shared MTLBuffer
```
The CPU had cached zeros (the `memset`), yet saw `0xAB` after — i.e. the GPU write **snoop-
invalidated the host CPU's stale lines** (hardware coherency). **The host side is correct.** The
bug is the guest's view of the same physical shm pages.

## Finding 2 — the GUEST `hv_vm_map`'d view is incoherent; only CACHED is even usable

In the guest (eglrender FBO clear→`glReadPixels`, venus) the readback is **black**. We made the
guest-facing cache hint selectable (`LIMINA_GPU_MAP_CACHE` override of `map_info` in libkrun
`resource_map_blob`, reverted after) and booted each mode:

| guest map_info | result |
|---|---|
| **CACHED** (default; vkr sets it for HOST_COHERENT+HOST_CACHED) | **black** (stale), clean |
| UNCACHED (Device-nGnRE) | **SIGBUS** — zink's unaligned reads fault on Device memory |
| WC (Normal-NC) | **SIGILL/crash** |

So **CACHED is the only mechanically viable mapping**, and it reads stale. Same physical shm
pages, host cached view = fresh, guest cached view = stale ⇒ the **guest CPU is not in the GPU's
hardware coherency domain** (the host CPU is — Finding 1). A stage-2 / shareability-level gap:
the guest's Normal-WB mapping via `hv_vm_map` is not inner-shareable-coherent with the Apple GPU,
so GPU writes never snoop-invalidate the guest's stale cache lines.

## Finding 3 — venus assumes coherency; userspace can't invalidate

- Guest venus `virtgpu_bo_invalidate`/`virtgpu_bo_flush` (mesa
  `src/virtio/vulkan/vn_renderer_virtgpu.c`) are **NOPs**: `/* nop because kernel makes every
  mapping coherent */`. True on real virtio-gpu (KVM, shared RAM); **false on our HVF path.** So
  even marking the memory non-coherent (dropping HOST_COHERENT so zink calls
  `vkInvalidateMappedMemoryRanges`) does nothing today.
- A userspace `dc civac` (data cache clean+invalidate by VA) **SIGILLs** on the guest kernel —
  invalidate-class ops aren't permitted at EL0. So the real invalidate must live in the **guest
  kernel**, not userspace mesa.

## Conclusion + fix options (the fix is NOT host-side)

The host (MoltenVK/Metal/shm) is coherent. The break is the guest reading `hv_vm_map`'d host
memory that isn't in the GPU coherency domain, with no working invalidate. Candidate fixes (all in
code we own; enhanced tier):

1. **Guest-kernel cache invalidate for host-visible blobs.** Make the virtio-gpu driver (or a
   venus ioctl) `dc civac` the range on `INVALIDATE`, and mark the memory non-coherent so zink
   issues invalidates. *Open risk:* needs the GPU writes to be fetchable from the guest after
   invalidating its private caches — Finding 1 shows the data reaches a coherent point for the
   host CPU, so an inner-shareable refetch should see it, but this must be proven with a
   kernel-side test (EL0 `dc civac` SIGILLs, so the self-contained guest probe couldn't confirm).
2. **Make the guest mapping inner-shareable-coherent with the GPU** (stage-2 attributes). Not
   controllable via `hv_vm_map` today (RWX flags only) — would need an HVF capability we don't
   have. Likely a dead end.
3. **Transfer model (copy, not map).** Route host-visible reads through guest RAM: the host (which
   sees GPU writes) `memcpy`s into a guest-RAM-backed resource on `TRANSFER_FROM_HOST_3D`. Always
   coherent (guest RAM), but loses zero-copy and needs venus to stop blob-mapping host-visible
   memory — a larger venus/vkr change. This is closest to the old slp/virgl copy model.

Current shipping state: the VN_PERF feedback-disable workaround sidesteps the *hang* (#27); this
coherency gap still black-holes `glReadPixels`-style readback and blocks putting gnome-shell
itself on venus (its present/readback would hit the same path).

## Reproduce
- Host coherency: `cd spikes/mtl-shm-coherency && clang -fobjc-arc -framework Metal -framework
  Foundation test.m -o test && ./test`
- Guest stale readback: boot `scripts/run-venus-window.sh`, then the eglrender FBO probe in
  `/tmp/coh-repro.sh` (RGBA clear → `glReadPixels` → `PIXEL=0,0,0,0`).
