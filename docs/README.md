# limina documentation

This directory holds research and design docs. The research is **decision
oriented**: each document inventories the concrete implementation options for a
feature, weighs tradeoffs against our constraints (macOS / Apple Silicon host,
Linux guest, willing to patch the stack), and ends with a recommendation.

## Research

| Doc | Topic |
|-----|-------|
| [research/00-overview.md](research/00-overview.md) | The stack, constraints, and how the pieces fit |
| [research/01-libkrun.md](research/01-libkrun.md) | What libkrun provides today (API, backends, VMM internals) |
| [research/02-macos-hvf.md](research/02-macos-hvf.md) | macOS Hypervisor.framework capabilities & limits |
| [research/03-graphics-3d.md](research/03-graphics-3d.md) | virtio-gpu, virglrenderer, Venus, native context, display |
| [research/04-input-keyboard.md](research/04-input-keyboard.md) | Mouse capture, key combos, remapping, layouts |
| [research/05-clipboard.md](research/05-clipboard.md) | Clipboard sharing options |
| [research/06-usb-passthrough.md](research/06-usb-passthrough.md) | USB passthrough options |
| [research/07-networking.md](research/07-networking.md) | NAT (TSI/gvproxy/passt) and bridged networking |
| [research/08-memory.md](research/08-memory.md) | Memory overhead and dynamic/ballooning options |

## Design

| Doc | Topic |
|-----|-------|
| [design/architecture.md](design/architecture.md) | Overall architecture (to be written after research) |

## Roadmap

See [roadmap.md](roadmap.md).
