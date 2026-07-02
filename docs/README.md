# limina documentation

limina is a native macOS app that runs Linux desktop guests on Apple Silicon via
[libkrun](https://github.com/containers/libkrun) + Hypervisor.framework, aiming
to replace Parallels. Milestones M1–M6, M8-polish and M10 are shipped (boot →
desktop → venus 3D → networking → clipboard/shares → dynamic memory → disks);
see the [roadmap](roadmap.md) for the live status.

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

Decision-oriented design docs — the founding one plus one per major feature
(shipped docs double as as-built records):

- [Architecture](design/architecture.md) — process model, libkrun patch strategy,
  crate/module layout, FFI boundary, feature→component mapping.
- [Tier-2 coexist GPU](design/tier2-coexist-gpu.md), with deep dives:
  [zero-copy IOSurface present](design/tier2-iosurface-zerocopy-present.md),
  [host-visible coherency](design/tier2-host-visible-coherency.md),
  [cogl batched-render bug](design/tier2-cogl-batched-render-bug.md),
  [venus ring idle wakeups](design/venus-ring-idle-wakeups.md).
- [Runtime display resize](design/runtime-display-resize.md) — shipped.
- [M6 dynamic memory](design/m6-dynamic-memory.md) — shipped.
- [M7 USB passthrough](design/m7-usb-passthrough.md) — mock shipped; real-device
  capture rides the [privileged helper](design/privileged-helper.md) (deferred).
- [M9 suspend/resume + VM snapshots](design/m9-suspend-resume.md) — designed, not started.
- [M10 multiple disks](design/m10-multiple-disks.md) — shipped.
- [Multi-VM networking](design/multi-vm-networking.md) — proposal (phases 0–3 in scope).
- [VM definitions & persistence](design/vm-definitions.md) — the per-VM config model.
- [Distribution & updates](design/distribution.md) — signing, notarization,
  updates, guest-tools delivery.

## Reviews

- [2026-07-01 full review](reviews/2026-07-01-full-review.md) — VMM design/implementation,
  per-patch upstreamability triage of all carried series, plans/risks; feeds the
  upstreaming effort.

## Guest images & tiers

- [images.md](images.md) — the source of truth for the Fedora guest disk images we develop and
  test against: what each is, which tier it exercises, pristine-vs-modified, and how it's produced.
- [tiers.md](tiers.md) — the two-tier guarantee as shipped (stock floor vs enhanced),
  including the RPM-replace delivery rationale.
- [Hardening backlog](hardening-backlog.md) — "finish what's shipped" ledger.
- [Parallels migration runbook](dogfooding-parallels-migration.md).

## Roadmap

- [roadmap.md](roadmap.md) — the milestone plan and live status (M1 boot → M11
  productization), including per-milestone as-built notes.
