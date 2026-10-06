# venus: free a thread's TLS state without calling into an unloaded driver

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

- **pin** (`8733525be5e` on `upstream/guest-2026-10`): once the key exists, reopen the driver with
  `RTLD_NOLOAD | RTLD_NODELETE`. The driver stays loaded until the process exits.
- **thread-exit hook** (`wip/venus-tls-atexit`): where the build finds `__cxa_thread_atexit_impl`
  (glibc 2.18+, bionic API 23+), keep the state in a `thread_local` and register its teardown with
  that hook. glibc (`l_tls_dtor_count`, checked in `_dl_close_worker`) and bionic
  (`__loader_add_thread_local_dtor`) refuse to unload a DSO while it has teardowns pending, so the
  driver stays loaded only while some thread still owes one. The main thread does not register:
  its state goes with the process, and `exit()` takes no ring teardown. Without the hook, the tss key
  is kept and a library destructor `tss_delete()`s it at unload; threads still holding venus TLS
  then leak it. That path keeps a narrow race: a thread already inside the key destructor when
  another thread's `dlclose` unmaps the driver.
- **fallback** (`wip/venus-tls-fb`, test only): the thread-exit-hook branch with the meson check
  forced off, so the `tss_delete` path runs on glibc.

## Results

QEMU 10.2 + virglrenderer 1.3.0 guest, venus on Intel Iris Plus G7, Mesa `main` b39d173ca93 and
that plus each fix. 3 runs per cell. Measured 2026-10-06.

Driver-mapped columns report whether `libvulkan_virtio.so` is still in `/proc/self/maps` after the
last instance is destroyed.

| build | `thread` | `unload` | `cycle` | `main-unload` | `main-alive` |
|---|---|---|---|---|---|
| `main` | SIGSEGV 3/3 | crashes before the report | crashes before the report | no 3/3 | exit 0 3/3 |
| pin | exit 0 3/3 | yes 3/3 | yes 3/3 | yes 3/3 | exit 0 3/3 |
| thread-exit hook | exit 0 3/3 | no 3/3 | no 3/3 | no 3/3 | exit 0 3/3 |
| fallback | exit 0 3/3 | no 3/3 | no 3/3 | no 3/3 | exit 0 3/3 |

`main-unload` is the check that the hook build skips the main thread: a registration there would
hold the driver, as the pin does. `main-alive` exits cleanly on every build, unfixed included, so it
only shows that `exit()` with a live device does not crash.

Fedora 44's `mesa-vulkan-drivers-26.2.3-1.fc44` also crashes `thread` 3/3. `nm -D` confirms the
thread-exit-hook build imports `__cxa_thread_atexit_impl` and the fallback build does not.

Not covered by a run: the fallback's race, and a worker thread that calls `exit()` itself, which runs
its own teardown from `exit()` on the thread-exit-hook path (tss destructors never ran there). The
fallback's library destructor is `__GNUC__`-only, so an MSVC build keeps today's behaviour, and it
also runs at process exit, where deleting the key is harmless.

## MR description

To be written by the submitter (Mesa's AI policy). Points it needs: the crash and its trigger
(also seen with surfaceless EGL on zink over venus terminated before its thread exits); why a key
destructor does not hold the DSO; the hook and its loader reference; the main-thread exclusion; the
fallback and its leak and race; the table above; and the alternative of pinning with
`RTLD_NODELETE` or `-z nodelete`.
