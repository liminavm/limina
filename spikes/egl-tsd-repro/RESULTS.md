# egl-tsd-repro — venus ICD unloaded with a live TLS-key destructor

**The bug:** any thread that ever used venus SIGSEGVs at thread exit in
`__nptl_deallocate_tsd` once the Vulkan loader has `dlclose()`d the driver. venus
registers a C11 TLS key (`vn_tls_key`, `vn_common.c`) whose destructor
`vn_tls_free` lives in `libvulkan_virtio.so`; unlike `__cxa_thread_atexit_impl`,
pthread/C11 key destructors do **not** pin the DSO they point into, and the loader
unloads drivers when the last instance is destroyed. Thread exits later → the
destructor is called through a pointer into unmapped memory.

`egl-tsd-repro.c` (self-contained, no headers needed): worker thread does
surfaceless-EGL init → zink-on-venus context → teardown → `eglTerminate` →
returns. `cc -o egl-tsd-repro egl-tsd-repro.c -ldl -lpthread`. Root-caused
originally by matching the fault PC against the run's link maps (niri's headless
`egl_*` tests, which run each test on its own thread, were the original sighting —
first seen in a gnome-shell-rs session on the dogfood-guest guest, 2026-07-02).

## Observations (2026-07-02)

- **dogfood-guest guest** (F44, mesa-vulkan-drivers 26.1.3-2.limina, glibc 2.43):
  plain run exit 139 (SIGSEGV after "teardown complete");
  `LD_PRELOAD=/usr/lib64/libvulkan_virtio.so` (pins the ICD) → clean exit 0.
- **Fresh local build VM** (clone of `Fedora-Workstation-44.enhanced.raw`,
  mesa 26.1.3-1.limina, venus on KK healthy): same RED, deterministic.
- **Upstream still broken**: mesa `main` and `mesa-26.1.3`
  `src/virtio/vulkan/vn_common.c` both create the key with no DSO pinning
  (checked 2026-07-02).

## Fix

`patches/mesa/0013-venus-pin-icd-for-tls-destructor.diff`: after
`tss_create(&vn_tls_key, vn_tls_free)` succeeds, re-open our own DSO
(`dladdr` on `vn_tls_free` → `dlopen(RTLD_NOW|RTLD_NOLOAD|RTLD_NODELETE)`) so the
destructor stays mapped for the life of the process. This is the standard idiom
for the pthread_key-vs-dlclose hazard; the pin is a one-time ~few-MB residency
cost only for processes that actually used venus. Upstreamable.

Validation: rebuild the F44 mesa RPM (`scripts/provision/f44/build-mesa-rpm.sh`,
in-guest) with 0013 → install in the build VM → repro exits 0 + venus still
enumerates. (See the git log for the validated result.)
