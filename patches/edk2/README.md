# Vendored EDK2 sources for the KRUN_EFI firmware build

`scripts/build-krun-efi.sh` builds our GOP firmware from `slp/edk2@krun-support` and
patches it in place (see that script). Most of our changes are *in-place patches* to
files that already exist in the slp base (the `.dsc`/`.fdf`, `PlatformBm.c`,
`VirtioSerialPort.c`). This directory carries **whole upstream files the slp base
lacks**, so the build can drop them in: the build mounts `patches/edk2` at
`/edk2-vendor` and copies these trees into the edk2 checkout before patching.

## `OvmfPkg/VirtioKeyboardDxe/`

An EDK2 UEFI driver that binds a virtio-input device over `VIRTIO_DEVICE_PROTOCOL`
and produces `EFI_SIMPLE_TEXT_INPUT_PROTOCOL` (+ the `…Ex` variant) — i.e. it turns
libkrun's virtio keyboard into firmware **ConIn**, which is what gives GRUB and the
early firmware a typeable keyboard in the limina window. The `slp/edk2@krun-support`
base predates this driver (it was added upstream ~Dec 2024), so we vendor it.

- **Upstream:** `tianocore/edk2`, path `OvmfPkg/VirtioKeyboardDxe/`
- **Pinned to:** tag `edk2-stable202505` (commit `6951dfe7d59d144a3a980bd7eda699db2d8554ac`)
  - chosen as the *oldest* stable tag that contains the driver, to minimise VirtioLib /
    `VIRTIO_DEVICE_PROTOCOL` API drift against the older slp base.
- **License:** BSD-2-Clause-Patent (the EDK2 license; unchanged, files are verbatim).
- **Files:** `VirtioKeyboard.c`, `VirtioKeyboard.h`, `VirtioKeyCodes.h`, `VirtioKeyboard.inf`.

### How the build consumes it (GOP builds only)

`build-krun-efi.sh` step (4):
1. copies `OvmfPkg/VirtioKeyboardDxe/` into the checkout;
2. adds `VIRTIO_SUBSYSTEM_INPUT 18` to `OvmfPkg/Include/IndustryStandard/Virtio10.h`
   (the slp base defines only GPU=16 / FILESYSTEM=26);
3. adds `VirtioKeyboard.inf` to `ArmVirtKrun.dsc` + `.fdf` (after VirtioGpu);
4. patches `PlatformBm.c`: an `IsVirtioInput` filter + `FilterAndProcess(Connect)` to
   bind the device, then an `AddInput` callback that adds each resulting `SimpleTextIn`
   handle to `ConIn` — the input twin of the existing VirtioGpu→`ConOut` patch;
5. declares `gEfiSimpleTextInProtocolGuid` in `PlatformBootManagerLib.inf`.

Safety notes (verified against the source before vendoring): the driver raises only
`TPL_NOTIFY` (it does **not** repeat the `RaiseTPL(TPL_CALLBACK)` lowering that hung
VirtioSerial in this build), and its event loop filters by `EV_KEY`, so it harmlessly
co-binds limina's tablet/mouse nodes (also `SubSystemDeviceId 18`) without emitting
phantom keystrokes from pointer events.

### Re-vendoring / bumping

```sh
TAG=edk2-stable202505   # or newer
for f in VirtioKeyboard.c VirtioKeyboard.h VirtioKeyCodes.h VirtioKeyboard.inf; do
  curl -fsSL "https://raw.githubusercontent.com/tianocore/edk2/$TAG/OvmfPkg/VirtioKeyboardDxe/$f" \
    -o "patches/edk2/OvmfPkg/VirtioKeyboardDxe/$f"
done
```
Then rebuild (`scripts/build-krun-efi.sh`) and update the pinned tag/commit above.
