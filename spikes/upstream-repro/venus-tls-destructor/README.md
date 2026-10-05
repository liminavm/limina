# venus: keep the driver loaded once the TLS key exists

**Bug.** `vn_tls_get()` registers `vn_tls_free` as a tss-key destructor (first reached from
`vkCreateDevice`). A key destructor does not keep its DSO loaded, and the loader `dlclose()`s the
ICD when the last instance is destroyed, so a thread that used venus and exits after its instance
is gone calls into the unmapped driver: SIGSEGV in `__nptl_deallocate_tsd`.

**Reproducer.** `venus-tls-destructor.c`: a worker thread creates an instance and a device on the
first venus physical device, destroys both, and returns. Any venus guest reproduces it — no
particular host GPU, renderer, or page size.

    cc -o venus-tls-destructor venus-tls-destructor.c -lvulkan -lpthread
    VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.x86_64.json ./venus-tls-destructor

## Results

| Mesa | Setup | Result |
|---|---|---|
| Fedora 44 `mesa-vulkan-drivers-26.2.3-1.fc44` | QEMU guest, venus on Intel Iris Plus G7 | SIGSEGV after `worker done`, 3/3 |
| `main` b39d173ca93 | same | SIGSEGV after `worker done`, 3/3 |
| `main` + fix (series tip e09e44d2d0d) | same | `worker joined`, exit 0, 3/3 |

Measured 2026-10-05.

## MR description (draft)

> **venus: keep the driver loaded once the TLS key exists**
>
> A thread that used venus and exits after the last `VkInstance` is destroyed crashes in
> `__nptl_deallocate_tsd`: `vn_tls_free` is registered as a tss-key destructor, a key destructor
> does not keep its DSO loaded, and the loader has already `dlclose()`d the ICD.
>
> Reproducer (any venus guest; tested under QEMU 10.2 + virglrenderer 1.3.0, venus on an Intel
> host): a thread creates an instance and a device, destroys both, and returns. Source attached;
> `cc -o repro repro.c -lvulkan -lpthread`. Before: SIGSEGV 3/3. After: clean exit 3/3. Also seen
> in the wild with surfaceless EGL on zink-over-venus terminated before its thread exits (niri's
> headless EGL tests run each test on its own thread).
>
> The pin is taken only once the key exists, so processes that never touch venus TLS keep
> unloading the driver as before. An alternative is linking the ICD with `-z nodelete`, which
> pins it unconditionally.
