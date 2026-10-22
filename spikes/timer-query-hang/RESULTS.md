# timer-query-hang — the "ext_timer_query GPU hang" is Metal's GPU watchdog on a long shader

**Result.** The timer query is innocent. piglit's `ext_timer_query@time-elapsed` doubles the loop
count of a deliberately slow fragment shader until one draw takes 0.1 s, and a fragment workload that
long trips macOS's GPU watchdog: `kIOGPUCommandBufferCallbackErrorHang`, a host GPU restart. The same
shader in a bare Metal program, with no VM and none of our stack, hangs the GPU the same way. What was
ours is what happened next: KosmicKrisp turned the hang into a lost device, every GL context in the VM
failed with it, and the worker died on signal 11.

**Fix.** A hang now costs only the work that hung, and the context that keeps causing them is
stopped:
- KosmicKrisp survives the hang by moving to a new Metal queue (macOS bans the queue, not the
  process) and charges the hang to the command pool whose work ran into it
  (`vkGetCommandPoolHangCountLIMINA`).
- zink reads those counts per context and resets a context that has caused `ZINK_HANG_LIMIT` hangs,
  or any context that hangs once the VM has caused more than `ZINK_HANG_BUDGET`. The limina worker
  defaults these to 2 and 4.
- vrend creates its contexts with `LOSE_CONTEXT_ON_RESET`, reads the reset status after every batch
  and refuses a reset context from then on (`refused: vrend ctx N: the host reset this context for
  hanging the GPU`).
- zink no longer crashes on the descriptor pools a lost device cannot create, which was the signal 11.

Where the fix lives: mesa fork branch `kk-gpu-hang` (KK recovery, the zink attribution and the zink
crash fixes), virglrs branch `gpu-hang` (the vrend refusal), and the worker's defaults in
`crates/limina-vmm/src/krun/mod.rs`.

Measured 2026-10-22 on the M1 Max dev Mac, macOS 27.0. Stock F44 guest
(`Fedora-Workstation-44.stock.piglit-spirv.raw` clone, Fedora Mesa, vrend over zink-on-KK), base
`limina-kk 059715d2aa5`, limina `41b0914a`.

## The guest test, end to end

`run.sh <clone> noq <out>` runs the test with every query removed, then a capped rerun
(`TQH_MAX_ITERS=64`) as the check that GL still renders afterwards:

| stack | GPU restarts | the VM afterwards | capped rerun |
|---|---|---|---|
| `limina-kk` (before) | 1, then the device is lost | every GL context fails; a 19.9 s control-queue stall; the worker dies on signal 11 ~30 s later | — |
| KK recovery only | 12 | desktop renders; clean poweroff | pass |
| KK recovery + zink attribution + vrend refusal, limit 2 | 4 | desktop renders; clean poweroff | pass |

With the full fix, the offending context was reset on its second hang and vrend refused every later
submit from it. Two more hangs still landed: work the context had already queued before the reset.
So a limit of N costs about N + 2 restarts. Each hang still stalls the virtio-gpu control queue for
the ~120 ms the watchdog takes.

Before the fix, the test's own output showed the device dying (`cpu` is the draw's `glFinish` time):

```
[TQH] iters=1048576 cpu=0.038981 s
[TQH] iters=2097152 cpu=0.079874 s
[TQH] iters=4194304 cpu=1.448419 s
[TQH] iters=4194304 cpu=6.266839 s
[TQH] iters=4194304 cpu=18.252403 s
```

The growth after the hang is zink's `VRAM_ALLOC_LOOP` retrying the out-of-memory error KK returned for
a lost device, with sleeps; KK now reports `VK_ERROR_DEVICE_LOST` there instead. `glFinish` is honest
before the hang: CPU time follows the loop count linearly up to 2M iterations, so the doubling loop
stops where it should. The test is simply asking for a draw that macOS will not run.

## The same shader on bare Metal

`longfrag.m` (build line in its header) draws one full-screen triangle with the test's shader into an
RGBA8 target, one command buffer per draw, and prints each buffer's GPU time:

| target | iters | GPU time | result |
|---|---|---|---|
| 160×160 | 2,097,152 | 76 ms | completes, 3/3 |
| 160×160 | 2,300,000 | 86 ms | `kIOGPUCommandBufferCallbackErrorHang` |
| 160×160 | 2,500,000 | 86 ms | hang |
| 160×160 | 4,194,304 | 86 ms | hang |
| 1280×1280 | 1,048,576 | 46 ms | hang |

At 160×160 the kill lands at the same 86 ms however long the draw would have run. At 1280×1280, a
draw whose fragments each run ~38 ms is killed at 46 ms. So the limit is not a fixed budget per
command buffer. It behaves like a deadline on non-preemptible fragment work, in the tens of
milliseconds; the exact rule is Apple's and is not characterised further here. Any guest shader in that
class (a heavy WebGL page, a shadertoy) reaches it the same way.

## What macOS does to the process: it punishes the queue, not the process

`LONGFRAG_RECOVER=4 longfrag 4194304 4194304 1 160` hangs the GPU, then draws a short (1024-iteration)
frame on the same queue and on a newly created one, four rounds:

| round | long draw | short, same queue | short, new queue |
|---|---|---|---|
| 0 | hang | completes | completes |
| 1 | hang | **ignored** (`kIOGPUCommandBufferCallbackErrorSubmissionsIgnored`) | completes |
| 2 | ignored | ignored | completes |
| 3 | ignored | ignored | completes |

The kernel logs `Deny submissions/ignore app[longfrag] with 2 GPURestarts in 1 submissions` after the
second hang, and from then on ignores that queue; a new queue in the same process works every time.
So a hang costs one host GPU restart and the queue it ran on. It does not cost the device.

## A Metal 4 queue around a hang, the way KK drives it

`longfrag4.m` commits the long draw, signals a shared event, then commits three short draws with a
signal after each, all without waiting, for three rounds:

- The commit right behind a hung one fails too, with a GPU time of 0: it is a victim, not a
  second hang. KK therefore charges a hang to a pool only when its command buffer ran.
- In the first two rounds, everything behind the victim runs and every signal arrives.
- In round 3, after the queue's second hang, every commit fails at once and **the signals queued
  behind them never fire**. Anything waiting on them waits forever.
- With `LONGFRAG4_SWAP=1` (a new queue after any hang, made to wait on the shared event), all three
  rounds complete and every signal arrives.

This is the recovery KK implements. Each queue generation's pending signals are tracked. A
generation that has had two hangs that ran is the one the kernel stops serving, so its signals and
the swap event are delivered from the CPU. A new queue that waited on a signal the kernel will never
send would deadlock.

## Running it

```
cp -c <stock piglit image>.raw tqh.raw
spikes/timer-query-hang/run.sh $PWD/tqh.raw noq /tmp/tqh/noq     # or `q` for the unmodified queries
clang -fobjc-arc -framework Metal -framework Foundation -o longfrag longfrag.m
./longfrag 262144 16777216 3 [size]                                # doubling loop, stops at the hang
LONGFRAG_RECOVER=4 ./longfrag 4194304 4194304 1 160                # recovery and ban
clang -fobjc-arc -framework Metal -framework Foundation -o longfrag4 longfrag4.m
./longfrag4 4194304 3 3; LONGFRAG4_SWAP=1 ./longfrag4 4194304 3 3  # Metal 4: ban, then the swap
```

A host Mesa tree other than the pinned one is built beside it with
`HOST_MESA_SUFFIX=<x> scripts/build-host-mesa.sh` (see its header). Each hang is a host GPU restart:
run on a host with no other VM, and expect a flicker on every display. `LIMINA_KK_HANG_RECOVERY=0`
turns KK's recovery off.
