# limina documentation

limina is a native macOS app that runs Linux desktop guests on Apple Silicon via
[libkrun](https://github.com/containers/libkrun) + Hypervisor.framework, aiming
to replace Parallels. First milestone: boot the local
`Fedora-Workstation-43.raw` image to a usable desktop.

Start with the [**overview**](research/00-overview.md) for the layered stack,
the key findings, the feature matrix, and the cross-cutting decisions.

## Research

Source-verified, per-subsystem references (each carries `path:line` citations
into the local `third_party/` checkouts).

- [00 — Architecture overview](research/00-overview.md)
- [01 — libkrun internals & C API](research/01-libkrun-internals-and-api.md)
- [02 — macOS Hypervisor.framework (HVF)](research/02-macos-hvf.md)
- [03 — Graphics: virtio-gpu & 3D](research/03-graphics-virtio-gpu-3d.md)
- [04 — Input & keyboard](research/04-input-and-keyboard.md)
- [05 — Clipboard sharing](research/05-clipboard.md)
- [06 — USB passthrough](research/06-usb-passthrough.md)
- [07 — Networking](research/07-networking.md)
- [08 — Memory & dynamic ballooning](research/08-memory-and-dynamic.md)
- [09 — Display host integration](research/09-display-host-integration.md)
- [10 — Guest agent & vsock control plane](research/10-guest-agent-and-vsock.md)
- [11 — Audio, Rosetta & misc](research/11-audio-rosetta-misc.md)
- [GAPS & verification](research/GAPS-and-verification.md) — skeptical accuracy/completeness pass

## Design

- [Architecture](design/architecture.md) — process model, libkrun patch strategy,
  crate/module layout, FFI boundary, feature→component mapping.

## Guest images

- [images.md](images.md) — the source of truth for the Fedora guest disk images we develop and
  test against: what each is, which tier it exercises, pristine-vs-modified, and how it's produced.

## Roadmap

- [roadmap.md](roadmap.md) — milestone-based plan. **M1: boot
  `Fedora-Workstation-43.raw` to a serial console** via a from-source libkrun-efi.
