# vrend on a render thread of its own

**Status:** agreed design, not implemented yet. Fixes the hardening-backlog entry "vrend's cold shader and
pipeline compiles run on the virtio-gpu worker thread".

## Problem

A classic virgl context (stock guest Mesa's GL driver, and every Flathub app, whose GL runtime has
no venus on aarch64) runs vrend on libkrun's virtio-gpu worker thread. That thread also drains the
control queue, books fences, presents retired frames and services the cursor queue. When vrend meets
something new, the compile happens inline:

- `SUBMIT_3D` translates TGSI to GLSL and links programs (`select_linked_program`);
- every Global-ring fence calls `glFenceSync` (`Vrend::fence_global` → `take_fence`), which Mesa
  flushes without `PIPE_FLUSH_ASYNC`, so `tc_flush` syncs the threaded context and runs the queued
  zink draws inline, including `zink_create_gfx_pipeline` compiling on KosmicKrisp.

Measured 2026-10-20 with `LIMINA_WAKE_PROBE=1` (Flathub Loupe 51 starting on the synoik image):
drains of 589, 999 and 1211 ms, of which 579, 995 and 1180 ms were fence handling, and single
`SUBMIT_3D`s of 189 and 388 ms. The device is frozen for every other context, the display and the
pointer while that happens.

## Approach

`Vrend` moves, whole, onto a dedicated **vrend thread** owned by virglrs. The thread makes ctx0
current when it starts and keeps it; every GL call vrend makes runs there. The virtio-gpu worker
and rutabaga hand it work as messages and, for the hot path, do not wait.

**Why not move only fence creation.** `glFenceSync` orders against the command stream of the
context current on the calling thread. A fence created on any thread other than the one whose
contexts issued the draws signals after that thread's (empty) stream and orders nothing. The fence
and the work it covers must come from the same thread, so the whole of vrend moves.

### Ownership

- `Renderer.vrend: Option<Vrend>` becomes a `VrendHandle`: a sender into the thread's queue, the
  shared resource table `Arc`, and the existing pump. The thread owns `Vrend` and its `Slots`.
- The vrend thread **never takes the `Guarded<Renderer>` mutex.** Otherwise venus, capset and
  scanout calls on the worker would queue behind a compile and nothing would have moved.
- The only crossing between the renderers is venus blobs and embedder surfaces lent to a classic
  context on `CTX_ATTACH_RESOURCE` (`renderer.rs` `resource_attach_blob`). It becomes a message
  carrying the lent storage.

### The resource table lock

Today the classic arms (`submit_cmd`, `create_fence`, `context_create_fence`, `poll`,
`context_create`, `context_destroy`, `resource_present_fence`) hold the resource `RwLock` read lock
for the whole GL call. On the vrend thread that would block the worker's writers (`insert`,
`resource_unref`, `with_resource_mut`) for the length of a compile, and resource create and unref
are hot. **The thread takes what it needs under the lock (`Arc` clones of storage and textures),
drops the lock, then does GL.** This is a refactor inside virglrs in its own right, and the first
step of the implementation.

### Commands: asynchronous or barrier

A **barrier** sends the request and waits until the vrend thread has run everything queued before
it plus the request itself. It reintroduces a wait, so it is allowed only where the command is
rare or the answer cannot be had otherwise.

| Command | Kind | Notes |
|---|---|---|
| `SUBMIT_3D` (classic) | async | command bytes are copied into the message; the descriptor completes at once |
| `TRANSFER_TO_HOST_2D/3D` | async | |
| `TRANSFER_FROM_HOST_3D` | async | guest Mesa fences it; the fence is queued behind the transfer, so the guest sees the data when the fence retires |
| every fence (Global, classic context, present) | async | a message on the same queue, so FIFO order is the fence order; the waiter is unchanged |
| `RESOURCE_CREATE_3D` | split | validation (`plan()` over the cached features, formats and limits), the table entry and the IOSurface mint stay synchronous on the worker (no GL), so an invalid request gets its exact error; only the GL allocation is queued. `SET_SCANOUT`, `SURFACE_IMPORT` and `result_from_query` stay synchronous for free |
| `CTX_ATTACH_RESOURCE`, `ATTACH_BACKING` | async | |
| `RESOURCE_UNREF` | async | already deferred: the table drop condemns vrend's half, the thread sweeps it |
| `CTX_CREATE` (classic) | barrier | EGL context creation on the thread; rare, so the response keeps its exact error |
| `CTX_DESTROY` | async | |
| `DETACH_BACKING` | barrier | vrend copies pages out before the VMM takes them back; rare |
| host3d `RESOURCE_CREATE_BLOB` on a classic context | barrier | the persistent GL map must exist before `map_ptr` is answered |
| `SET_SCANOUT` / `SURFACE_IMPORT` `settle` | barrier only while a decode is in flight | the surface itself comes from the table; `settle` is GL only when a decode target is unsettled, so it gates on `decodes_in_flight` |
| `sync_iosurface` fallback (`glFinish`) | barrier | the rare present path that could not park |
| `GET_CAPSET*` | neither | capsets are cached at init; read without the renderer lock |
| snapshot, restore, reset, parked-session check | barrier, bounded | see below |

### Fences

Global fences, classic context fences and present fences become messages on the vrend thread's
queue, so each orders behind the vrend work queued before it, and the waiter downstream keeps its
FIFO retirement through `virglrs-fence`. The guest's contract (one Global timeline once a classic
context exists; stock classic fences on one device-wide context; the watermark retires everything
up to the delivered id) is preserved by construction. No second ordering mechanism is added.

### Back-pressure

`hold_to_fence_depth` already blocks the worker up to 2 s before a submit. It moves to the enqueue:
the worker blocks only when the vrend queue is over its depth. Under sustained overload the stall
returns by design; that limit belongs to the backlog entry "Nothing stops a guest from submitting
seconds ahead of the host's decode", not to this change.

### Guest memory, snapshot, restore, reset

- The vrend thread writes guest RAM (`TRANSFER_FROM_HOST`, query answers), so it enters its own
  `DumpGate` section around each message that can.
- Snapshot, restore and reset can no longer assume "nothing else pumps the renderer while we hold
  the worker". Each becomes a barrier that drains the queue and then runs its renderer work on the
  vrend thread, patterned on `docs/design/async-video-decode.md` (tickets plus a settle barrier at
  every read). The barrier carries a timeout: a render thread parked in a sync point once hung the
  suspend quiesce (`docs/graphics.md`), and this must fail loudly rather than hang.
- `Renderer::poll` (parked queries) is pumped on the vrend thread by the thread itself.

### Panics

The vrend thread runs vrend under the same `Guarded` poison: a panic there marks the whole renderer
dead, as a panic on the worker does today. Containment does not change with this work.

## Decisions

1. **Error responses.** Responses keep their exact errors wherever a barrier is cheap or no GL is
   needed: `CTX_CREATE` is a barrier, and `RESOURCE_CREATE_3D` validates synchronously. Two errors
   can no longer travel in the response, because it is written before the vrend thread runs the
   work: `SUBMIT_3D` stream poison, and a GL allocation refusal after validation passed. Both are
   logged at warn when the thread reports them. A barrier on `SUBMIT_3D` would defeat the change,
   and one on every create would put Mesa's staging uploads (thousands per run) behind queued
   compiles.
2. **Panic scope stays whole-renderer** (above). A vrend panic killing only classic contexts is
   possible later, as its own change.

## Verification

- **RED first:** an L2 test that boots the synoik image with `LIMINA_WAKE_PROBE=1`, starts a cold
  GL client on virgl (glmark2-es2-wayland from the accessible tooling, or Flathub Loupe's startup),
  and fails on any `control queue drain` whose fences bucket exceeds 50 ms or any `[GPUSLOW]
  SUBMIT_3D` line. It fails by seconds on today's tree.
- virglrs harness cases for the FIFO fence order across the thread boundary, the barrier commands,
  and the table lock not being held across GL.
- The full suite, with the classic-context snapshot/restore tests as the regression net for the
  barrier work.

## Implementation order

1. virglrs: stop holding the table read lock across GL in the classic arms.
2. virglrs: `VrendHandle` and the vrend thread; the classification table above.
3. libkrun: snapshot, restore and reset as bounded barriers; `DumpGate` on the vrend thread;
   back-pressure at the enqueue.
4. Update virglrs `docs/design.md`, which says vrend's GL context belongs to whichever thread
   called init.
