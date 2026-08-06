# patches/edk2 — RETIRED (fork model since 2026-08-06)

The edk2 delta that lived here — the vendored `OvmfPkg/VirtioKeyboardDxe/` tree plus
`apply-virtio-keyboard.py`, and the in-place patches inside `scripts/build-krun-efi.sh`
(TerminalPcdProducerLib build fix, VirtioSerialDxe TPL fix, PL031 DT status=okay, the
VirtioGpuDxe GOP/ConIn enablement) — was **migrated to the fork model** in task #22:
`github.com/liminavm/edk2`, branch **`limina`**, is the delta (6 commits on the
`slp/edk2@krun-support` base the krunkit blob is built from).

- The pin lives in `[edk2]` in `third_party/manifest.toml`. edk2 is **not** vendored by
  `cargo xtask vendor`: `scripts/build-krun-efi.sh` reads the pinned rev from the manifest
  and clones/checks it out inside its own container build volume. A local `third_party/edk2`
  checkout is optional (fork surgery only).
- To change the firmware: commit on the fork's `limina` branch, push, bump the manifest rev,
  rerun `scripts/build-krun-efi.sh`. Tag before every branch rewrite — every rev ever pinned
  must stay reachable.
- Why our own build at all: krunkit ships only `KRUN_EFI.silent.fd` — serial-only, no GOP,
  and a DEBUG build whose live ASSERTs end in `CpuDeadLoop` (the #14 cold-boot wedge). Ours
  is RELEASE with a graphical, typeable boot console.

## VirtioKeyboardDxe provenance (kept from the retired vendored copy)

The fork's import commit (`OvmfPkg/VirtioKeyboardDxe: import from edk2-stable202505`)
carries these files verbatim from upstream `tianocore/edk2` tag `edk2-stable202505`
(commit `6951dfe7d59d144a3a980bd7eda699db2d8554ac`) — the *oldest* stable tag containing
the driver, to minimise VirtioLib / `VIRTIO_DEVICE_PROTOCOL` API drift against the older
slp base. License: BSD-2-Clause-Patent (the EDK2 license; files unchanged). Safety notes
verified before vendoring: the driver raises only `TPL_NOTIFY` (it does **not** repeat the
`RaiseTPL(TPL_CALLBACK)` lowering that hung VirtioSerial in this build), and its event loop
filters by `EV_KEY`, so it harmlessly co-binds limina's tablet/mouse nodes (also
`SubSystemDeviceId 18`) without emitting phantom keystrokes from pointer events. To bump:
re-import from a newer tag as a new commit on the fork branch.
