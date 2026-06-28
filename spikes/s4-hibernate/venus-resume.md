# M9 venus-resume spike — can venus rebuild its Vulkan object graph against a fresh host render-server?

Date: 2026-06-28. Method: two grounded source passes (Explore), every claim cited `file:line`:
- **Guest venus driver** (Mesa): `/Volumes/mesa-cs/mesa/src/virtio/vulkan/` (`vn_*.c`).
- **Host venus render-server** (virglrenderer): `third_party/virglrenderer/src/venus/` (`vkr_*.c`).

## The question

After a host-side VM **restore**, a brand-new worker brings up an **empty** venus render-server, while the
guest's RAM is restored mid-flight holding handles to Vulkan objects the fresh host never created. Spike #3
proved this is the venus tier's gate. Can venus rebuild the host object graph by re-issuing its creates?

## Findings — guest venus driver (the bottleneck)

1. **Create-infos are discarded after forwarding.** `vn_buffer_create` forwards the `VkBufferCreateInfo`
   and stores none of it; `struct vn_buffer`/`vn_image`/`vn_device_memory`/`vn_pipeline` retain only memory
   requirements + WSI metadata, **not the creation parameters** (`vn_buffer.c:303–328`, `vn_image.h:45–62`,
   `vn_device_memory.h:16–54`, `vn_pipeline.h:50–70`). There is **no per-device object list** to walk
   (only swapchains are tracked, `vn_device.h` `chains`). → **venus literally threw away what a replay needs.**
2. **No recovery path.** `VK_ERROR_DEVICE_LOST` is fatal at every site (`vn_queue.c:405,1043,…`); no
   reconnect/replay/resume (the "suspend/resume" terms in `vn_queue.h:28–41` are fence-feedback tuning).
3. **Ring/context is one-shot.** The command ring (`vn_ring.c:278–374`, `vkCreateRingMESA`) and virtgpu
   context (`vn_renderer_virtgpu.c:1560–1573`) are established once with no re-establishment path.
4. **Object IDs from a global atomic counter** (`vn_common.c:63` `vn_next_obj_id=1`; `vn_common.h:386–390`).

## Findings — host render-server (already replay-ready)

1. **Objects are keyed by GUEST-assigned IDs**, not host pointers: `vkr_context.object_table` is a hash
   keyed by `vkr_object_id` (uint64) inserted at the guest's id (`vkr_context.h:217–226`, `vkr_common.h:114`).
2. **A fresh `vkr_context` is genuinely blank** and ready to accept a normal creation stream
   (`vkr_context.c:789–878,831`). **No snapshot/checkpoint/serialize path exists** (confirmed: zero grep
   hits) — rebuilding *must* come from the guest re-issuing commands.
3. **So a replay reconstructs a consistent graph IF** the guest re-issues the same creates, **same ids**, in
   dependency order. Blockers are hardening-grade: id-collision is a **fatal `assert`** not a graceful reject
   (`vkr_context.h:223`); memory imports that reference a `vkr_resource` are **fatal if it doesn't exist yet**
   (`vkr_device_memory.c:22–26`) so resource/blob creates must precede the memory that imports them; rings +
   in-flight fences are ephemeral/per-context and must be re-registered.

## The reframe both passes half-missed: a VM snapshot is NOT a process restart

The guest pass concluded "infeasible" largely on **non-deterministic object IDs (counter resets to 1)** — but
that assumes the guest *process restarts*. **In a host-side VM snapshot/restore the guest RAM is restored and
the guest process keeps running** — so `vn_next_obj_id`, every `vn_*` object struct, and all handle tables are
**preserved exactly**. Therefore:

- **The "non-deterministic IDs" blocker DISSOLVES** — the guest keeps its exact object ids, and the host is
  *already keyed by those same guest ids*. Identity matching is free; no determinism work needed.
- What does **not** come back for free: (a) the **create-infos** venus never stored (blocker #1 — real,
  snapshot or not), and (b) the **host ring/context** (gone with the old worker → must be re-established).

So the picture is the inverse of "infeasible": the **host is architecturally replay-ready** and the
identity problem is a non-issue for snapshots; the work is concentrated in the **guest venus driver**, which
must be taught to *retain* and *replay* — both in code we own.

## Verdict — feasible, but a substantial guest-Mesa + host-virglrenderer build (not an upstream carry)

venus-resume is achievable and entirely within code we own, in three parts:

1. **Guest venus (Mesa) — the bulk.** Retain each object's creation info (per-object or a per-device replay
   log) so it *can* be replayed; add a **resume path** that re-establishes the virtgpu context + ring with the
   fresh host, then walks the retained objects in **dependency order** and re-issues their creates with the
   **same (RAM-preserved) ids**. Memory/maintenance cost, but bounded.
2. **Host render-server (virglrenderer) — hardening.** Make object-id re-add **graceful** instead of the
   fatal `assert` (`vkr_context.h:223`); ensure the replay honors the resource→memory dependency order
   (`vkr_device_memory.c:22–26`). No snapshot path to build — it's replay-from-guest.
3. **Coordination.** Bridge the kernel virtio-gpu `.restore` (Dongwon-Kim re-creates the device + virtio-gpu
   resources/contexts) up to **userspace venus** so it knows to re-establish + replay (kernel resume is
   in-kernel; venus's ring/objects are userspace — there is no trigger today). Plus the **host-visible blob
   copy-back** for `VkDeviceMemory` *contents* (object *structure* is replayed; zero-copy blob *bytes* still
   need the guest-GPU `TRANSFER_FROM_HOST` at snapshot — guest-backed memory rides the RAM snapshot).

## Open questions for the M9.3 venus design

- **The resume trigger** (kernel `.restore` → userspace venus replay) — uevent? context-invalidation that
  makes the next venus command return a recoverable error? An explicit limina control message? **Undesigned.**
- **Dependency-ordered replay** — venus must re-issue creates in a valid DAG order (resources before importing
  memory, layouts before pipelines, …). A retained per-device *ordered* create log is the simplest vehicle.
- **In-flight work** at snapshot — quiesce (drain fences/submits) before the snapshot so there's no
  half-submitted command-buffer state to reconstruct (aligns with the M9.3 snapshot-time GPU quiesce).
- **Effort** — clearly the heaviest single piece of M9; confirms the venus tier as the *premium,
  research-flavored* feature, with virgl (kernel Dongwon-Kim) as the achievable baseline.

## Bottom line for M9.3

venus-resume is **feasible and ours to build**, and the architecture is more amenable than the per-source
"infeasible" read suggested — the host render-server is replay-ready and keyed by guest ids, and a VM
snapshot preserves those ids in restored RAM. The gate is a **guest-venus retain-and-replay** capability
(create-info retention + a resume/replay path + ring re-establishment) plus minor host hardening. Recommend
M9.3 ship the **virgl tier first** (kernel Dongwon-Kim, the proven baseline) and schedule the **venus
retain-and-replay** as its own tracked sub-project, starting with the **resume-trigger design**.
