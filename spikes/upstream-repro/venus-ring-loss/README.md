# venus: ring loss as VK_ERROR_DEVICE_LOST — issue / RFC draft

Upstream deliberately aborts on a fatal ring (`vn_relax`, `vn_ring_submit`, the query and
semaphore feedback probes), so turning that into `VK_ERROR_DEVICE_LOST` is a policy change. It
goes upstream as an issue/RFC first. The limina-guest patch ("venus: surface ring loss as
VK_ERROR_DEVICE_LOST instead of abort()") conflicts on main and would be re-cut against whatever
shape the discussion settles on.

**Reproducer** (no special host needed): `../venus-dmabuf-import/venus-dmabuf-import --query`.
It asks venus for the properties of a dma-buf that is a virgl resource; upstream vkr fails that as a
command-stream error, the ring goes fatal, and the guest process aborts inside
`vkGetMemoryFdPropertiesKHR` — measured 2026-10-05 on the upstream rig (QEMU 10.2, virglrenderer 1.3.0,
main b39d173ca93): SIGABRT. Host log: `vkr: failed to query resource props: invalid res_id N`,
`vkGetMemoryFdPropertiesKHR resulted in CS error`.

Open question to settle before filing: whether !42501 (`VN_DEBUG=no_abort` extended to ring fatal)
is still open, and what its review said (the notes were 401-gated in the August audit).

## Issue text (draft)

> **venus: report a fatal ring as VK_ERROR_DEVICE_LOST instead of aborting the process**
>
> When the renderer marks a venus ring fatal, the guest driver calls `abort()` at the next wait or
> submit (`vn_relax`, `vn_ring_submit`, and the query/semaphore feedback probes). A fatal ring
> means the renderer-side context is gone, which is what `VK_ERROR_DEVICE_LOST` exists to report:
> the application could tear down and recreate its device, fall back to another driver, or at
> least exit cleanly. Today the whole process dies, including any other device it holds.
>
> It is easy to reach from valid API use whenever the renderer refuses something as a
> command-stream error. For example, `vkGetMemoryFdPropertiesKHR` on a dma-buf that came from a
> virgl (GL) resource: vkr logs `failed to query resource props: invalid res_id` and the
> application aborts inside the call. A small reproducer is attached; it needs only a virtio-gpu
> guest with venus and virgl (tested under QEMU 10.2 + virglrenderer 1.3.0). It is also reachable
> from host-side events that a guest cannot prevent, such as a renderer fault or a VMM that
> restores a snapshot without the renderer's state.
>
> Proposal:
> - waits on a fatal ring (`vn_relax` and the ring seqno/space waits) return failure instead of
>   aborting, and the calling entrypoints return `VK_ERROR_DEVICE_LOST`;
> - `vn_ring_submit` fails a submission on a fatal ring rather than aborting, releasing what it
>   took;
> - the feedback probes report `VK_ERROR_DEVICE_LOST` to their callers instead of aborting;
> - keep the abort available for debugging, behind a `VN_DEBUG` flag, since a core at the point of
>   failure is valuable when developing the renderer.
>
> We carry an implementation of this downstream and can send it as an MR if the direction is
> acceptable. Related: !42501, which makes ring fatal non-aborting behind `VN_DEBUG=no_abort`;
> this proposal makes non-aborting the default and keeps the abort as the opt-in.
