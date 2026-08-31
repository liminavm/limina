# The KosmicKrisp command-allocator pool, measured

Instrumentation for the 2026-08-31 dogfood SIGSEGV: a nil store inside AGX's own MTL4 compute
data-buffer pool, reached from a guest GL texture upload
(`vrend_renderer_transfer_write_iov` → zink → `kk_CmdCopyBufferToImage2` → `blitCDMBufferToTexture`
→ `prepareForEnqueue`). The allocator behind that pool is ours (`kk_device.c`), so the question is
whether we destroyed, reset, or overfilled one under a live encoder.

`kk_alloc_pool_report()` prints per class on the release path
(`[LIMINA-ALLOC-POOL]`, every 2000 encoder closes and at device teardown):

    live / peak / retired | size hiwater vs budget, retirement count | peak ops per command buffer
    tombstones, use-after-destroy

A destroyed allocator keeps its struct, stamped `KK_PA_DEAD`, so any stale pointer is named at the
call that used it. `LIMINA_KK_ALLOC_GUARD=abort` turns the report into a core dump there.

## Baseline: F44 enhanced.synoik, seated desktop, glmark2 on vrend/zink-on-KK

`baseline-2026-08-31.txt` holds all 699 report lines. Two workloads, one boot:

| workload | class | live/peak | destroyed | size hiwater | retirements | peak ops/cmdbuf |
|---|---|---|---|---|---|---|
| desktop + 1 glmark2 | render | 3 / 3 | 0 | 4997 KiB | ~1 per close | 5 |
| desktop + 1 glmark2 | compute | 1 / 1 | 0 | 1285 KiB | **0** | 1 |
| desktop + 12 glmark2 | render | 13 / 13 | 16 | 4997 KiB | ~1 per close | 5 |
| desktop + 12 glmark2 | compute | 1 / 1 | 0 | 1285 KiB | **0** | 1 |

Budget is 4096 KiB per allocator; the floor is 8 live per class.

## What this settles

**The destroy path is unreachable below the floor.** At one GL client the pool holds three render
allocators, so nothing is ever destroyed however long it runs. Twelve concurrent clients push it to
thirteen and sixteen destructions follow. Any probe meant to exercise destruction must therefore
drive **more than eight live allocators of the class under test** — a single-client reproducer
cannot reach the code at all, and would read as a clean exoneration.

**Destruction itself is clean here.** Sixteen destroys, zero use-after-destroy.

**The compute class never retires under this workload.** Its allocator peaks at 1285 KiB against a
4096 KiB budget, so it never drains, is never reset, and is never destroyed. The crash was in a
*compute* encoder, so whatever the dogfood workload does, it is not what glmark2 does: reaching the
suspect code on the compute class needs an allocator driven past 4 MiB.

**Command buffers are small.** Peak 5 operations per render command buffer under glmark2, 384 at
device teardown under the desktop, 1 for compute. Nothing resembling an unbounded encoder.

## What the next vehicle has to do

Drive the compute-class hiwater past the 4096 KiB budget with more than eight live compute
allocators — i.e. sustained `glTexSubImage2D` from a dozen concurrent contexts, not one. Until
those two numbers move, a run that does not crash has not tested the hypothesis.
