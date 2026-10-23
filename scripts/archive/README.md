# Archived scripts (superseded — pending deletion)

These are kept for reference after being superseded. They are **not** wired into
any current workflow. Slated for deletion once we're confident nothing here is
worth resurrecting.

| Script | Superseded by | Why |
|---|---|---|
| `build-mesa-zink-sysext.sh` | `scripts/build-gpu-drivers.sh` + `guest/gpu-drivers/install.sh` | A systemd-sysext overlay can only *shadow* (not *remove*) the distro's `libgallium`, and a mixed-version ABI blend breaks mutter's KMS EGL. The vendor drivers sidestep the question: they install under `/opt/limina` and are found through the loaders' own vendor directories, so the distro's Mesa stays whole. |
| `build-mutter-sysext.sh` | — | The enhanced tier ships stock mutter. |
| `build-mesa-zink.sh` | `scripts/build-gpu-drivers.sh` | Prefix-style guest zink+venus build. The vendor build drives every virtio-gpu: zink on KosmicKrisp over a limina transport, virgl elsewhere, venus for Vulkan (`docs/design/metal-transport-delivery.md`). |
