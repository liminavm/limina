# timer-query-hang — the "ext_timer_query GPU hang" is Metal's GPU watchdog on a long shader

**Result.** The timer query is innocent. piglit's `ext_timer_query@time-elapsed` doubles the loop
count of a deliberately slow fragment shader until one draw takes 0.1 s, and a fragment workload that
long trips macOS's GPU watchdog: `kIOGPUCommandBufferCallbackErrorHang`, a host GPU restart. The same
shader in a bare Metal program, with no VM and none of our stack, hangs the GPU the same way. What is
ours is what happens next: KosmicKrisp turns the hang into a lost device, every GL context in the VM
fails with it, and the worker later dies on signal 11.

Measured 2026-10-22 on the M1 Max dev Mac, macOS 27.0, no other VM running. Stock F44 guest
(`Fedora-Workstation-44.stock.piglit-spirv.raw` clone, Fedora Mesa, vrend over zink-on-KK at
`limina-kk 059715d2aa5`), limina `41b0914a`.

## The guest test without any query

`run.sh <clone> noq <out>` swaps in `time-elapsed.c` (piglit's test plus one `[TQH] iters=… cpu=…`
line per draw, and `TQH_NOQ=1` to drop every `glBeginQuery`/`glEndQuery`/`glGetQueryObject`), rebuilds
that binary in the guest and runs it once:

```
[TQH] iters=1048576 cpu=0.038981 s
[TQH] iters=2097152 cpu=0.079874 s
[TQH] iters=4194304 cpu=1.448419 s
[TQH] iters=4194304 cpu=6.266839 s
[TQH] iters=4194304 cpu=18.252403 s
```

then `[LIMINA-DEVICE-LOST] MTL_COMMAND_QUEUE_ERROR_TIMEOUT … kIOGPUCommandBufferCallbackErrorHang` and
one `GPURestart` in the kernel log. With no query in the program, the hang is not the query's.

`glFinish` is honest: the CPU time of a draw follows the loop count linearly up to 2M iterations, so
the doubling loop stops where it should. The test is simply asking for a draw that macOS will not run.

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

At 160×160 the kill lands at the same 86 ms however long the draw would have run; at 1280×1280 a
draw whose fragments each run ~38 ms is killed at 46 ms. The limit is therefore not a fixed budget per
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

## What our stack does with it

From the `noq` worker log, after the device-loss report:
- KK marks the whole device lost (`kk_queue.c`, `check_device_lost` → `vk_device_set_lost`) and from
  then on refuses command allocators; zink's `vkBeginCommandBuffer` fails for every context.
- Every GL context in the VM is served by that one device, so the guest desktop's rendering fails
  with the offending app's; `[LIMINA-ENC] NULL compute encoder` lines follow.
- The virtio-gpu control queue blocks for 19.9 s in one drain.
- About 30 s after the hang the worker exits on signal 11, and the VM with it.

## Running it

```
cp -c <stock piglit image>.raw tqh.raw
spikes/timer-query-hang/run.sh $PWD/tqh.raw noq /tmp/tqh/noq     # or `q` for the unmodified queries
clang -fobjc-arc -framework Metal -framework Foundation -o longfrag longfrag.m
./longfrag 262144 16777216 3 [size]                                # doubling loop, stops at the hang
LONGFRAG_RECOVER=4 ./longfrag 4194304 4194304 1 160                # recovery and ban
```

Each hang is a host GPU restart: run on a host with no other VM, and expect a flicker on every display.
