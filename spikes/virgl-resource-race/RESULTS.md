# The virgl_resource_table race

`virgl_resource_table` (`third_party/virglrenderer/src/virgl_resource.c`) is a process-global
hash table with no lock. Upstream never needed one: every operation runs on the VMM's virtio-gpu
thread, through the `virgl_renderer_*` entry points.

limina's zero-copy IOSurface scanout put a **second thread** on it. `vkr_mtl_iosurface_free`
calls `virgl_resource_forget_iosurface`, which `util_hash_table_foreach`s the table to zero every
cached copy of a dying surface id — and that free runs on whichever **venus ring thread** served
the `vkDestroyImage`. A concurrent `virgl_resource_remove` frees the node the walk is standing on.

## What it costs when it fires

`limina-vmm` SIGSEGV, `KERN_INVALID_ADDRESS`, faulting thread `vkr-ring-N`:

```
virgl_resource_forget_iosurface_cb + 4     <- first load of res->iosurface_id
util_hash_table_foreach
virgl_resource_forget_iosurface
vkr_mtl_iosurface_free
vkr_dispatch_vkDestroyImage
```

The address is garbage, not a mapped region. Guest session teardown is the natural trigger: a
compositor exiting destroys its images in a burst on the ring thread while the guest unrefs the
matching virtio-gpu resources on the device thread. Both storms, one table.

## The harness

`race.c` drives the two threads the crash did — create/remove against `forget_iosurface` — over
the fork's real `virgl_resource.c`, with stubs for the handful of symbols the paths under test
never reach (`stubs.c`). It links the sources rather than `libvirglrenderer`, because none of
these symbols are exported from the dylib.

Built under **ThreadSanitizer**, so the verdict does not rest on catching a segfault: freed hash
nodes get benignly reused often enough that an unsanitized build can run clean for minutes.

```
./build.sh && ./race
```

Needs a configured meson build at `third_party/virglrenderer/build` for `config.h` and
`virgl-version.h`.

Before the lock:

```
WARNING: ThreadSanitizer: data race
  Read of size 8 by thread T2:   _mesa_hash_table_next_entry <- util_hash_table_foreach
                                 <- virgl_resource_forget_iosurface
  Previous write of size 8 by thread T1: hash_table_insert <- util_hash_table_set
                                 <- virgl_resource_create_from_iov
```

first sweep, every run. After: 20000 create/remove cycles against ~1200 full sweeps, silent,
exit 0.

## The fix

A plain `mtx_t` guarding **every** table operation — set, remove, get, clear, destroy, and the
forget walk — mirroring `virgl_fence.c`, which solved the same problem for the fence table in the
same tree. Locking only the two sides that happened to collide would leave the same invariant
("device-thread-only, except…") that broke once already.

The lock covers the table, not the resources: a `virgl_resource *` from `virgl_resource_lookup`
is still the caller's to use unlocked. The one field genuinely shared across threads —
`iosurface_id`, zeroed by the walk while the device thread reads it to present — is `_Atomic`.

`util_hash_table_remove` runs `virgl_resource_destroy_func` inline, so the lock is held across
`pipe_callbacks.unref`. That is safe here and worth re-checking if the unref chain grows: nothing
reachable from vrend's unref re-enters this table, and `vkr_mtl_iosurface_free` is called only
from `vkr_image.c`, never from a resource destroy.
