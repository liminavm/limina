# KosmicKrisp guest-input bounds probe

Under venus, a guest's Vulkan calls reach KosmicKrisp without validation, so invalid usage is
input KK must refuse or survive. `probe.c` drives KK directly through the Vulkan loader, one case
per process, and reports each as PASS, FAIL or CRASH: descriptor set layouts and allocation,
descriptor set binds, image limits, descriptor writes and copies, and push descriptors.

Every case passes from limina-kk `1bf40e3a14a` on, in builds with and without asserts; before the
fixes each misbehaved or crashed (except `mip-legal`, the control). Some cases pass on an unfixed
KK too, so a pass alone does not prove a fix; run the whole set.

    cc -Wall -I/opt/homebrew/include probe.c -L/opt/homebrew/lib -lvulkan -o probe
    VK_ICD_FILENAMES=<kk build>/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json ./probe [case]

Each case re-executes the binary: a bare `fork()` cannot reach Metal's XPC services.
