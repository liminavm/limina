# Reproducible Linux kernel build environment for limina's L1 test guest.
#
# Run as a BUILD TOOL only (via Apple `container` — lightweight Linux VM on Apple
# Silicon); it is not part of any limina deliverable. We build aarch64 natively inside the
# container (the host runs M1), so no cross-compiler is needed. The kernel source is
# cloned and built INSIDE the container (case-sensitive ext4) because macOS is
# case-insensitive and the Linux tree has case-colliding filenames; only the resulting
# Image is copied out to a bind-mounted host dir. See scripts/build-test-kernel.sh.
#
# NOTE: `container build` (BuildKit) currently needs Rosetta installed
# (`softwareupdate --install-rosetta`). To avoid that dependency, build-test-kernel.sh
# does NOT build this image — it runs `fedora:43` directly and installs these same deps
# inline (the kernel compile dwarfs the ~1-minute dnf). This file documents the deps and
# is the fast path once Rosetta is available (`container build -t limina-kernel-builder`).
FROM docker.io/library/fedora:43

# Kernel build deps. BTF/pahole is intentionally omitted — the config fragment disables
# CONFIG_DEBUG_INFO_BTF to keep the build self-contained and fast.
RUN dnf -y install \
        gcc make flex bison bc \
        elfutils-libelf-devel openssl-devel \
        perl findutils diffutils gzip xz cpio \
        git rsync kmod python3 \
    && dnf clean all

WORKDIR /build
