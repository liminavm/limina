# Metal transport: command queues (wire VERSION 5)

Status: implemented, and delivered to the enhanced and `metal.test` images (`docs/images.md`).
Wire VERSION 4 has one queue per context: every guest `MTLCommandQueue` lands
on the context's single `MTL4CommandQueue`, so two MLX streams take turns, and limina-metal holds
a command buffer at a cross-queue wait no submitted signal has reached
(`spikes/metal-cpp-shim/RESULTS.md`). VERSION 5 gives a context as many host queues as the guest
makes, each completing on its own.

## Wire

- **`CREATE_QUEUE {handle, completed: BufRef}`** makes an `MTL4CommandQueue`, an object like any
  other: `DESTROY` buries it until its commits complete, and a snapshot replays its creation.
  `completed` names a u64 in the ring's arena, 8-aligned, where the host stores the highest
  finished seqno of the queue's commits.
- **Queue 0 is the context's own queue**, and its completed value is
  `RingControl::completed`.
- **`Commit` and `QueueEvent` gain `queue`** (a queue handle, `0` for the context's own). The
  rule that a queue event comes between commits holds per context, as now.
- **Seqnos stay one increasing sequence per context**, shared by every queue. Each queue's
  completed value is the highest finished seqno among *its* commits. Commits on one queue finish
  in order, but commits on different queues do not, so no value says "every commit up to here".
- The context's residency set is added to every queue it makes.

## Fences

A fence is `(seqno, completed_at)`: it signals once the u64 at `completed_at` in the ring's arena
reaches `seqno`, and `completed_at` 0 means `RingControl::completed`.

- **UAPI:** `drm_virtgpu_limina_fence.pad` becomes `completed_at`. The module checks it lies in
  the ring's arena and is 8-aligned. It owns the arena's pages, so it reads a nonzero
  `completed_at` itself: a seqno already reached gets a signaled fence at once, with no virtio
  command. Only a pending fence goes to the host. `completed_at` 0 always goes to the host: the
  module does not know `RingControl`'s layout.
- **A virtio ring per completed value.** Upstream's fence driver signals a dma-fence context's
  earlier fences along with a later one, and the host retires a `(ctx, ring_idx)`'s fence
  commands in order the same way; queues complete independently. So the module gives each
  `completed_at` a file fences on a ring index of its own (0 for `completed_at` 0), with the
  dma-fence context `base + ring_idx`, as upstream does for context rings. `ring_idx` is below 64,
  so a context fences at most 63 queue slots besides its own (`ENOSPC` after).
- **Virtio command `0x4c06`** carries `completed_at` after `seqno`. libkrun passes it through,
  and limina-vmm's fence thread polls that offset in its mapping of the ring arena, not the
  control block.

## Who changes

| component | change |
|---|---|
| `limina-gpu-wire` | VERSION 5, `CREATE_QUEUE`, `queue` fields, generated C header |
| `limina-gpu-host` | a queue per handle, per-queue completed stores, queue in `commit`/`queue_event`, residency per queue |
| guest module (`guest/virtio-gpu-limina`, linux fork) | `completed_at`: checked, read for the fast path, sent in `0x4c06` |
| libkrun (fork) | `0x4c06` with `completed_at`, through the fence trait |
| limina-vmm `metal_ext` | fence thread polls `completed_at` |
| KosmicKrisp guest bridge | headers re-synced to VERSION 5; `queue` 0 and `completed_at` 0 everywhere, so behaviour is unchanged |
| `limina-gpu-client`, `limina-gpu-smoke`, tests | `queue` on commits and queue events; `create_queue` |
| limina-metal (`spikes/metal-cpp-shim`) | a host queue per `MTLCommandQueue`, completion per queue, the hold removed, arena reaping over every queue's completed value |
| delivery | guest drivers under `/opt/limina/5`, the module through DKMS, the enhanced images refreshed, `docs/images.md` |

A VERSION 4 snapshot no longer restores: `Consumer::resume` refuses the version.

## Tests, red first

- `first_light`: two queues; queue A waits for an event only the CPU sets; queue B's work is done
  and B's completed value reaches its seqno while A's stays behind; after the CPU's signal, A
  completes. A wait on queue B committed before its signal on queue A completes. A context with a
  queue exports and restores, and the restored queue runs work.
- limina-metal: two queues each running a long single-threadgroup kernel finish in about the time
  of one, not two. The event-wait cases pass without the hold.
- Module: a fence for a seqno already reached signals without a host round trip.
- MLX: the C++ suite, `check`, the model and serve steps; two models generating at once, against
  omlx on macOS.
- The full HVF suite.
