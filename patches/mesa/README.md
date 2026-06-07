# patches/mesa — limina enhanced-tier Mesa patches (zink)

The limina enhanced tier ships a **patched zink** (GL→Vulkan) so the GNOME/GL stack runs on
**venus → MoltenVK → Metal → Apple GPU** instead of llvmpipe. This dir carries our patch
series over upstream Mesa; the source clone + build happen in the Apple `container` Linux
build env (host APFS is case-insensitive and can't check out mesa) via
`scripts/build-mesa-zink.sh`, which installs the `/opt/mesa-zink` tree we deliver to the guest.

This replaces the old ad-hoc in-guest `~/mesa` build (task #26).

## Base
- **Upstream Mesa main, commit `3515c52e8cf31549b6068ef43c23c89830b6db46`** (pinned in
  `scripts/build-mesa-zink.sh` as `MESA_COMMIT`; 2026-06-07). gitlab.freedesktop.org is
  Anubis-bot-blocked → build clones a GitHub mirror.

## Patches (apply in filename order)
- **`0001-zink-nullDescriptor-emulation-MR37115.diff`** — Mesa **MR !37115**: zink
  nullDescriptor *emulation* (dummy descriptors) so zink runs on Vulkan implementations
  that lack `robustness2.nullDescriptor` — which MoltenVK does. Without it zink bails
  (`Zink requires the nullDescriptor feature of KHR/EXT robustness2`) → llvmpipe. Clean (no
  LIMINA-DIAG debug hacks). Not in any Mesa release/main — we must carry it until it lands.

## What we DON'T build here
Venus (`libvulkan_virtio`) is the guest's **stock** Mesa Vulkan driver (`-Dvulkan-drivers=`
is empty). zink is pointed at venus at runtime via
`VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json`. (If we ever need to patch
venus itself — e.g. for the #28 zero-copy coherency work — add `-Dvulkan-drivers=virtio` and
a venus patch here.)

## Re-export / DIAG hygiene
The in-guest working tree carried temporary `LIMINA-DIAG` debug hacks (force
`driver_name_is_inferred=false`, `mesa_loge(__LINE__)` markers) — those are **debug only and
must never be committed here**. Only clean, upstreamable patches belong in this series.
