# limina

A custom virtual machine application for macOS (Apple Silicon), built in Rust on
top of [libkrun](https://github.com/containers/libkrun). The goal is a
Parallels-class experience for running **Linux** guests, with:

- High-quality 3D graphics acceleration
- First-class fullscreen + mouse capture + macOS key-combo capture
- Customizable key bindings and keyboard layout remapping (e.g. swap
  Command/Option)
- Clipboard sharing
- USB device passthrough
- NAT **and** bridged networking
- Reduced memory overhead (host and guest) and, ideally, dynamic memory
  assignment (give the VM a range; it takes/returns as needed)

We are willing to patch libkrun and any of its dependencies (virglrenderer,
virtio-gpu, the guest kernel, custom drivers) to achieve these goals.

## Status

Early research & design. See [`docs/`](docs/).

**First milestone:** boot the `Fedora-Workstation-43.raw` image (kept locally,
gitignored; `.xz` backup alongside it).

## Layout

| Path | Purpose |
|------|---------|
| `src/` | The `limina` Rust application |
| `docs/research/` | Decision-oriented research on the underlying technologies |
| `docs/design/` | Architecture and design decisions |
| `docs/roadmap.md` | Milestones and sequencing |
| `third_party/` | Vendored / patched native dependencies (not yet present) |

## Host environment (reference)

- macOS 26.0, Apple M4 Pro, 64 GB RAM, arm64
- Rust 1.90 (edition 2024), Apple clang 17 (Command Line Tools)
