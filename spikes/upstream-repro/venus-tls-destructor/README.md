# venus: don't run a thread's TLS teardown from an unloaded driver

**Bug.** `vn_tls_get()` registers `vn_tls_free` as a tss-key destructor (first reached from
`vkCreateDevice`). A key destructor does not keep its DSO loaded, and the loader `dlclose()`s the
ICD when the last instance is destroyed, so a thread that used venus and exits after its instance
is gone calls into the unmapped driver: SIGSEGV in `__nptl_deallocate_tsd`.

**Reproducer.** `venus-tls-destructor.c`. Any venus guest reproduces it — no particular host GPU,
renderer, or page size.

    cc -o venus-tls-destructor venus-tls-destructor.c -lvulkan -lpthread
    VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.x86_64.json ./venus-tls-destructor [mode]

| mode | what it does |
|---|---|
| `thread` (default) | a worker thread creates an instance and a device on the first venus device, destroys both, and returns |
| `unload` | `thread`, then one more instance create/destroy on the main thread, and reports whether `libvulkan_virtio.so` is still in `/proc/self/maps` |
| `cycle` | 100 workers in turn, each with its own instance and device, then the same report |
| `main-unload` | the main thread creates and destroys an instance and a device, then reports whether the driver is still mapped; a teardown registered on the main thread would hold it ("yes") |
| `main-alive` | the main thread creates an instance and a device and returns from `main()` without destroying them |

## Fixes compared

- **key deleted at unload** (`d6e1586bfbb` on `venus-tls-teardown`, `c0d892b7d18` on
  `upstream/guest-2026-10`; the fix to send): keep the tss key and give the driver a library
  destructor that `tss_delete()`s it. Deleting a key runs no destructors and stops every thread's
  exit from calling `vn_tls_free`, so threads still holding venus TLS when the driver unloads leak
  it: a `struct vn_tls` plus one emptied `vn_tls_ring` wrapper per instance that gave the thread a
  ring, a few dozen bytes per thread per load cycle. The rings themselves are already destroyed by
  `vkDestroyInstance` (`vn_instance_fini_ring`). Unloading is unchanged: the driver goes at the
  last `vkDestroyInstance`, as before.
- **key deleted from `atexit()`** (`6e88fb430f7`, `wip/venus-tls-key-atexit`): the same deletion,
  registered with `atexit()` from `vn_tls_key_create_once` instead of a destructor attribute, as
  `src/util` registers its cleanups. On glibc this runs at `dlclose`, not only at exit: `atexit` is
  a static-only routine (`libc_nonshared.a`) that calls `__cxa_atexit(func, NULL, __dso_handle)`
  with the library's own handle (`stdlib/atexit.c`), and the library's fini from `crtbeginS.o`
  calls `__cxa_finalize(__dso_handle)` (libgcc `crtstuff.c`), which `_dl_close_worker` runs before
  unmapping (`elf/dl-close.c`, `_dl_call_fini`). The built driver imports `__cxa_atexit` and
  `__cxa_finalize`.
- **thread-exit hook** (`338ca7b81ae`, comparison only): register the teardown with
  `__cxa_thread_atexit_impl` (glibc 2.18+, bionic API 23+), whose loaders refuse to unload a DSO
  while it has teardowns pending (`l_tls_dtor_count`, checked in `_dl_close_worker`). This closes
  the race below, but a thread alive at the last `vkDestroyInstance` keeps the driver mapped until
  the next `dlclose` that unloads anything, or process exit: nothing re-runs the close sweep when
  the count drops, and the count is not reachable from outside libc.
- **pin** (`8733525be5e`, comparison only): once the key exists, reopen the driver with
  `RTLD_NOLOAD | RTLD_NODELETE`. The driver stays loaded until the process exits.

## Results

QEMU 10.2 + virglrenderer 1.3.0 guest, venus on Intel Iris Plus G7, Mesa `main` b39d173ca93 and
that plus each fix. 3 runs per cell. Measured 2026-10-06.

Driver-mapped columns report whether `libvulkan_virtio.so` is still in `/proc/self/maps` after the
last instance is destroyed.

| build | `thread` | `unload` | `cycle` | `main-unload` | `main-alive` |
|---|---|---|---|---|---|
| `main` | SIGSEGV 3/3 | crashes before the report | crashes before the report | no 3/3 | exit 0 3/3 |
| key deleted at unload | exit 0 3/3 | no 3/3 | no 3/3 | no 3/3 | exit 0 3/3 |
| key deleted from `atexit()` | exit 0 3/3 | no 3/3 | no 3/3 | no 3/3 | exit 0 3/3 |
| thread-exit hook | exit 0 3/3 | no 3/3 | no 3/3 | no 3/3 | exit 0 3/3 |
| pin | exit 0 3/3 | yes 3/3 | yes 3/3 | yes 3/3 | exit 0 3/3 |

`unload` and `cycle` read the map only after one extra instance create/destroy, which gives the
loader a `dlclose` to unload on; on the hook build that is what releases a driver held past the
last `vkDestroyInstance`. `main-unload` shows the hook build registers nothing on the main thread.
`thread` crashing on `main` and passing on the `atexit()` build is what shows the handler runs at
`dlclose`: a handler that waited for process exit would leave the crash in place. `cycle` loads and
unloads the driver 100 times in one process, registering the handler each time. `main-alive` exits cleanly on every build, unfixed included, so it only shows that `exit()` with a
live device does not crash.

Fedora 44's `mesa-vulkan-drivers-26.2.3-1.fc44` also crashes `thread` 3/3.

Not covered by a run: the race the key-deletion fix leaves. A thread that has already fetched the
destructor in `__nptl_deallocate_tsd`, or is inside `vn_tls_free`, when another thread's last
`vkDestroyInstance` unmaps the driver still faults: `munmap` invalidates the translations on every
CPU before it returns, so cached instructions do not help. Only a thread exiting during the final
teardown can hit it; an application that joins its threads first cannot. The library destructor is
`__GNUC__`-only and also runs at process exit, where deleting the key is harmless. Windows needs
nothing: Mesa's emulated tss runs key destructors from the driver's own thread-detach callback
(`src/c11/impl/threads_win32_tls_callback.cpp`), which the loader stops calling once the DLL is
freed, and venus builds no renderer there (no vtest, no virtgpu).

## MR description

To be written by the submitter (Mesa's AI policy). Points it needs: the crash and its trigger
(also seen with surfaceless EGL on zink over venus terminated before its thread exits); why a key
destructor does not hold the DSO; the key deletion, its bounded leak and the remaining race; why
not the thread-exit hook (it delays unloading, which upstream keeps on purpose) or a pin with
`RTLD_NODELETE` / `-z nodelete`; and the table above.

Prior art to cite (searched 2026-10-06; no issue or MR covers the venus key): mesa#13571 is the
same crash through sysprof's tss destructor in every driver, fixed in sysprof
(GNOME/sysprof!152 links its static library `-z nodelete`) and picked up by mesa!38347; the
Mesa-side `-z nodelete` attempt, mesa!36978, was closed for it. A driver built with
`-Dsysprof=true` against sysprof 49+ is therefore already NODELETE and cannot show this bug; the
Fedora build and a default `main` build are not. mesa#11085 and the open mesa!31185 are the same
unload problem for `atexit()` handlers.
