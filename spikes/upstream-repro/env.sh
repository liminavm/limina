#!/bin/sh
# Run one command against a Mesa installed under /opt/mesa-<name> in the guest.
#   env.sh main|fix <command> [args...]
# Vulkan defaults to the venus ICD; set VK_ICD=lvp_icd for lavapipe.
p=/opt/mesa-$1
shift
[ -d "$p" ] || { echo "no Mesa at $p" >&2; exit 1; }
export LD_LIBRARY_PATH="$p/lib64${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export LIBGL_DRIVERS_PATH="$p/lib64/dri"
export GBM_BACKENDS_PATH="$p/lib64/gbm"
export LIBVA_DRIVERS_PATH="$p/lib64/dri"
export __EGL_VENDOR_LIBRARY_FILENAMES="$p/share/glvnd/egl_vendor.d/50_mesa.json"
export VK_DRIVER_FILES="$p/share/vulkan/icd.d/${VK_ICD:-virtio_icd}.x86_64.json"
exec "$@"
