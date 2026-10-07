# Source: point host GL/VK at the private hardening build.
P=/Volumes/mesa-cs/zink-kk-prefix-hardening
ICD=/Volumes/mesa-cs/build-kk-hardening/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json
export VK_ICD_FILENAMES=$ICD VK_DRIVER_FILES=$ICD LIMINA_KK_ICD=$ICD MESA_PREFIX=$P
mkdir -p $P/vulkan-rpath; ln -sf /opt/homebrew/lib/libvulkan.1.dylib $P/vulkan-rpath/libvulkan.1.dylib
export DYLD_LIBRARY_PATH=$P/vulkan-rpath DYLD_FALLBACK_LIBRARY_PATH=$P/lib:/opt/homebrew/lib
export MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink LIBGL_DRIVERS_PATH=$P/lib EGL_PLATFORM=surfaceless
