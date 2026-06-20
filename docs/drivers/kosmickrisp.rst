KosmicKrisp (KK) — the host Vulkan-on-Metal driver
===================================================

What this is, and what it is **not**
------------------------------------

**KosmicKrisp (KK)** is mainline Mesa's **Vulkan driver for Apple Metal**. limina builds it
**natively on macOS arm64** and runs it as the *host* Vulkan ICD that virglrenderer's venus
backend (and now zink, see below) drives — i.e. the bottom of the host graphics stack:
``guest Vulkan → venus → KK → Metal``. KK is the **one supported** venus backend; MoltenVK was
retired (it SIGSEGV-loops the guest compositor).

.. important::

   This is the **host** Mesa build. Do **not** confuse it with ``scripts/build-mesa-zink.sh``,
   which builds the **guest** Mesa (the ``/opt/mesa-zink`` zink we ship *into* the Fedora guest,
   compiled in a Linux container). Different tree config, different target, different machine.

Source tree & build environment
--------------------------------

The Mesa checkout lives on a **case-sensitive APFS sparse image**, not the repo working tree:
host APFS is case-insensitive and cannot even check out Mesa cleanly (filename collisions).

- **Sparse image:** ``third_party/mesa-cs.sparseimage`` (gitignored), mounted at ``/Volumes/mesa-cs``.
- **Source:** ``/Volumes/mesa-cs/mesa`` (mainline Mesa; KK is upstreamed — zero limina patches for
  KK itself).
- **Build:** ``/Volumes/mesa-cs/build-kk`` · **ICD output:**
  ``/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json``
- **Python deps (mako, etc.):** the repo venv ``third_party/venv-mesa`` (``source .../bin/activate``).

Toolchain prerequisites (the part that bites)
---------------------------------------------

meson/ninja/glslang/cmake come from Homebrew. The traps — all because the needed tools are
**keg-only** (installed but *not* on ``PATH``/``PKG_CONFIG_PATH``), and a stock ``meson setup``
silently fails to find them:

- **LLVM** (``brew install llvm``, currently 22.x) is keg-only → its ``llvm-config`` is not on
  ``PATH``. KK **requires** it (``with_kosmickrisp_vk`` pulls CLC → LLVM, plus ``libclc``,
  ``spirv-llvm-translator``, ``spirv-tools``). Prepend ``$(brew --prefix llvm)/bin`` to ``PATH``.
- **expat** is keg-only → add ``$(brew --prefix expat)/lib/pkgconfig`` to ``PKG_CONFIG_PATH``
  (the EGL/dri driconf parser needs it; only matters once you build the GL frontend — see below).
- **bison**: Apple's ``/usr/bin/bison`` is 2.3 (2008); Mesa's GLSL ``glcpp`` grammar needs > 2.3.
  ``brew install bison`` and prepend ``$(brew --prefix bison)/bin``. KK proper (``opengl=false``)
  never builds glcpp so it doesn't hit this — but the zink/GL superset does. **meson bakes the
  bison path into ``build.ninja`` at configure time**, so install bison *before* (or
  ``--reconfigure`` after).

KK-only build (the canonical host driver)
-----------------------------------------

The meson line that produces the venus backend (Vulkan only, no GL frontend)::

  source third_party/venv-mesa/bin/activate
  export PATH="$(brew --prefix llvm)/bin:$PATH"
  meson setup /Volumes/mesa-cs/build-kk /Volumes/mesa-cs/mesa \
    -Dplatforms=macos -Dvulkan-drivers=kosmickrisp -Dgallium-drivers= \
    -Dopengl=false -Dzstd=disabled -Dprefer_static=true -Dbuildtype=debug
  ninja -C /Volumes/mesa-cs/build-kk

Running the worker on KK
------------------------

The worker selects KK via the ICD env var; ``scripts/run-venus-window.sh`` resolves it
automatically and refuses to boot venus on anything else::

  VK_ICD_FILENAMES=/Volumes/mesa-cs/build-kk/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json

Boot vehicle for seated/headless venus: ``spikes/venus-draw-probe/boot-seated-kk.sh``.

Debug levers
------------

- ``MESA_KK_DEBUG=msl`` — log the generated Metal Shading Language.
- ``MESA_KK_GPU_CAPTURE=1`` — arm a Metal GPU capture across device create..destroy.
- ``LIMINA_KK_*`` knobs (``NOLISTRESTART``, ``BOCACHE``, ``SLIMPUSH``, ``EARLYZ``, ``SLIMROOT``,
  ``FASTBIND``) — perf/correctness experiment toggles wired in ``boot-seated-kk.sh``.

GL on KK: zink-on-KK (baseline-3D path)
---------------------------------------

The *same* KK tree can additionally build **zink** (Mesa's GL→Vulkan gallium driver) + a headless
**surfaceless EGL**, giving accelerated **host GL on Metal** without ANGLE — the intended provider
for virglrenderer's ``vrend`` (baseline 3D for stock 4 KiB guests, immune to the 16k/4k page wall).
Proven 2026-06-20. See ``spikes/virgl-zink-kk/`` (``build-mesa-zink-kk.sh`` is the KK config plus
``-Dgallium-drivers=zink -Dopengl=true -Degl=enabled -Degl-native-platform=surfaceless
-Dmoltenvk-dir=...``, and ``RESULTS.md`` for the full recipe + caveats).
