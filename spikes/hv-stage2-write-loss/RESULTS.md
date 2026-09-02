# A stage-2 mapping change does not lose guest writes (standalone), yet one did in a VM

Question raised while chasing VP9 keyframes that reached the host all-zero (`docs/graphics.md` §4.5,
`docs/design/blob-decode-targets.md`): a stage-2 write-protect placed on the buffer's pages right after
the guest wrote them correlated perfectly with the loss, so the hypervisor was the suspect. This spike
asks Hypervisor.framework the question directly, with nothing else in the loop.

A bare-metal guest (`payload.S`) fills 256 KiB with a salted pattern, hands control to the host, which
changes the stage-2 mapping of those pages, then both sides recount. The guest can run with the MMU on
(identity map, RAM Normal write-back cacheable, the shape a Linux guest has), can `DC ZVA` the range
first (`init_on_alloc`'s clear_page), and can fill with 16-byte SIMD stores (`STR Qn`, glibc memcpy's
shape) instead of `STP Xn,Xm`. `--race-once <ns>` protects the range exactly once, `<ns>` into a fill,
from another thread; the vCPU heals each fault by reopening the 16 KiB page and retrying — the shape of
libkrun's balloon heal and of the write-watch probe below. `sweep.sh` drives it across a fill.

## Measured (M1 Max, macOS 26.5, `./build.sh`; every run 0 mismatching words on both sides)

| host op | granule | MMU | ZVA | fill | when | host view | guest recount |
|---|---|---|---|---|---|---|---|
| `hv_vm_protect` R\|X then RWX | default (16k), 4k | off | – | GPR | after the fill | intact | intact |
| `hv_vm_unmap` + `hv_vm_map` | default, 4k | off | – | GPR | after the fill | intact | intact |
| `hv_vm_protect`, RAM `MAP_SHARED` | default, 4k | off | – | GPR | after the fill | intact | intact |
| `hv_vm_protect`, host wrote page first | 4k | off | – | GPR | after the fill | intact | intact |
| `hv_vm_protect` | default, 4k | on | no/yes | GPR | after the fill | intact | intact |
| `hv_vm_protect` toggled R\|X↔RWX ~300-900× | default, 4k | on | yes | GPR | during a single fill | intact | intact |
| `hv_vm_protect` once, delay swept 0-120 µs in 1 µs steps ×3 | default, 4k | on | no | GPR and SIMD | 1861 of 2904 landed inside the fill (the rest after it) | intact | intact |

Under the racing thread the guest's stores trap as stage-2 permission faults (xFSC 0xf) and the retry
lands correctly after the page is reopened; not one word was lost. In the swept one-shot run the first
healed store sits at an arbitrary offset inside the fill every time, and the word before it is intact.

## Conclusion

`hv_vm_protect` and unmap+map preserve page contents on this macOS, with or without a running vCPU
writing the page, at either granule, cacheable or not, for GPR and SIMD stores, whether the change lands
before, during or after the writes. The correlation seen in limina was real but the protect was a
bystander: it ran inside the host's ATTACH_BACKING handling, one step before the actual destroyer,
virglrenderer's shadow write-back on attach (`docs/design/blob-decode-targets.md`). The lesson that
generalises: **a lever that reproduces a symptom is not thereby its cause — isolate the lever in a
system that has nothing else in it before naming it.**

## Open: the in-VM write-watch loses the store just before its protect

With the attach fix in place, the arm-time write-watch (`probes/libkrun-write-watch.patch`: protect the
first 128 KiB of every bitstream buffer at ATTACH_BACKING, heal on fault, re-protect 50 µs later) still
produced 5 corrupt keyframes in 60 runs — a different signature from the attach race: a 16-32 byte hole,
not a zero page. In all 5 the protect landed while glibc's `memcpy` was mid-buffer (the first trapped store
is not the copy's first store, `str q3,[x0]` at +0), and the hole is exactly the store immediately before
the first trap (one case: bytes 0-15 zero, first trap at +16 from `stp q0,q1,[x3,#16]`). In the 209 buffers
where the protect landed before the copy began, nothing was lost. Guest: Linux 16k pages, cacheable
userspace mapping of a shmem GEM object (`map_wc` false), 6 vCPUs; host: IPA granule 4k.

The standalone one-shot sweep above is the same shape and does not reproduce it, so an ingredient of the
real system is missing from the probe and remains unidentified (candidates: 16 KiB stage-1 pages over
4 KiB stage-2 leaves, other vCPUs running during the change, the host-side heal reopening 16 KiB while
the change was made in 4 KiB leaves). Nothing shipped changes the stage-2 mapping of a page the guest
may be writing: libkrun has no `hv_vm_protect` call, and its two `hv_vm_unmap` sites are the balloon's
release of ranges the guest has reported free and holds isolated
(`third_party/libkrun/src/hvf/src/released_ram.rs:216`) and the blob-map window's map/unmap, which
runs on the guest's own MAP_BLOB/UNMAP_BLOB request (`third_party/libkrun/src/hvf/src/lib.rs:535`
via `src/vmm/src/macos/vstate.rs:149,163`). So this is a probe hazard today, not a product fault. Keep it in mind before building anything on write-protecting live guest
pages (dirty tracking, copy-on-write snapshots): measure with the real guest first.

The probe stays useful as a harness for stage-2 semantics questions (a tiny MMU-on guest with a
checkpoint protocol, a single-fill race and a swept one-shot protect), next to `spikes/balloon-unmap-fault`.
`probes/` keeps the two instrumentation patches the investigation ran with: the virglrenderer bitstream
scans (`BSSTAT`/`BSSCAN`) and the libkrun write-watch.
