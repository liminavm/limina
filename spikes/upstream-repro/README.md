# upstream-repro — reproducers for patches sent to upstream projects

One directory per upstream patch. Each holds a reproducer an upstream maintainer can run on their
own setup — stock Linux, QEMU with Fedora's virglrenderer, or a plain software driver — never one
that needs limina or a macOS host. Each README states the setup, the command, and the before/after
result on that setup.

The vehicle is an Intel host (Fedora 44, Iris Plus G7) running a Fedora 44 guest under stock QEMU
10.2 + virglrenderer 1.3.0, `-device virtio-gpu-gl-pci,blob=true,venus=true,hostmem=4G` with a
shared memfd backend and `-display egl-headless`. The guest runs two Mesa builds side by side,
installed under `/opt/mesa-main` (upstream `main`) and `/opt/mesa-fix` (`main` + the patches), and
`env.sh` selects one for a single command.

Status of each patch: `docs/upstreaming/ledger/mesa.md`.
