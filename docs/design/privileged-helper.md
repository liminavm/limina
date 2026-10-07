# Design — a single privileged helper (`limina-privhelperd`)  ·  DEFERRED

> **Status: DEFERRED feature / architecture decision (2026-06-27).** Nothing here is built yet. This
> doc records the *shape* so that when the first root-requiring feature ships (real-device USB
> passthrough of Apple-claimed devices below macOS 27, M7 Phase 4), we build the broker — not a one-off helper — and every later privileged
> feature reuses it. **The default stays unprivileged-first**: limina runs with no elevated
> privileges; the helper is opt-in, per-feature, and only spawned when a feature that needs root is
> explicitly requested.

## The decision

**There will be exactly one privileged helper process, `limina-privhelperd`, that brokers every
operation requiring root (or a future elevated capability).** Features do *not* each ship their own
privileged binary. The one known client:

- **USB device capture** (M7 Phase 4) — open + detach Apple-bound drivers + claim a host USB device,
  then run the `LibusbBackend` USB/IP server on it. Proven to need root, **no entitlement** (see
  `m7-usb-passthrough.md` §Phase 4; `sudo spikes/usb-probe/run.sh` on a Solo 2 confirmed
  `detach=OK claim=OK`). On macOS 27 with the AccessoryAccess grant the worker opens devices
  itself, so this client covers only Apple-claimed devices on macOS 15–26 or without that grant.
- **Not a client: vmnet.** Every vmnet mode, bridged included, runs non-root in the worker with the
  unrestricted `com.apple.security.virtualization` (measured on macOS 26,
  `spikes/vmnet-network-probe/`; `multi-vm-networking.md` §3.3). limina offers vmnet on macOS 26+;
  macOS 15 is unmeasured.
- **Future** — anything else that genuinely needs root (e.g. raw bridged interfaces, certain
  performance knobs). New privileged capabilities are added as *methods on the existing broker*, not
  as new privileged binaries.

### Why one broker, not a helper per feature

- **One audited privilege boundary.** A single small, reviewable binary holds all the root code; the
  large unprivileged surface (supervisor, UI, VMM worker) never runs as root. Per-feature helpers
  multiply the privileged attack surface and the review burden.
- **One authorization/install step.** The user grants/install the helper once (one sudo prompt in
  dev, one `SMAppService` daemon approval in the product) instead of a separate elevation per
  feature. Worse UX and worse security to prompt N times.
- **Uniform lifecycle + IPC.** No-orphan death-pact, fd-passing, and the request/response protocol
  are written and hardened once. This mirrors the data-plane model the networking design already
  uses (supervisor creates a socketpair, hands one fd to the helper, one to libkrun) — the helper
  just additionally needs privilege to *create* the resource being handed over.
- **Consistent with the tenet.** "Mechanism in the dependency, policy in limina": the helper is pure
  mechanism (capture this device / create this interface), all policy (which device, which network,
  when) stays in the unprivileged control plane.

## Shape

```
 unprivileged                                   privileged (root)
┌──────────────────────────┐                  ┌─────────────────────────────┐
│ limina supervisor / UI    │   request (IPC)  │ limina-privhelperd          │
│  - owns VM lifecycle      │ ───────────────► │  - validates the request    │
│  - decides policy         │                  │  - does the root operation: │
│  - holds NO root          │ ◄─────────────── │      • USB capture (libusb) │
│                           │   fd + result    │                             │
└──────────────────────────┘                  │  - hands back an fd, drops  │
        │  passes fd to libkrun                │    or holds privilege min.  │
        ▼                                      └─────────────────────────────┘
   VMM worker  ──►  guest (vhci_hcd)
```

- **Request/response over a UNIX socket**, with **SCM_RIGHTS fd passing** as the core primitive: the
  helper performs the privileged step and returns an *open fd* (the captured USB device's USB/IP
  stream socket) to the unprivileged supervisor, which feeds it to libkrun exactly as the mock USB
  path (3b) already does. Root touches the
  resource only long enough to create/capture it.
- **Least privilege / least duration.** The helper does the minimum privileged action and then either
  drops privilege or confines itself to the one captured resource. It
  never runs guest-facing data-plane logic it doesn't have to.
- **No-orphan death-pact** (same guarantee as the net helpers, `multi-vm-networking.md` §4): the
  helper must never outlive the VM(s)/resources it serves — EOF-on-control-socket death-pact +
  startup sweep of dead-owner leftovers, reusing the gvproxy-reaping pattern already in
  `crates/limina/src/gateway.rs`.
- **Per-request authorization, validated host-side.** The helper re-validates every request (e.g. the
  device id is one the user actually selected via `--usb`) rather than trusting the caller blindly,
  so a compromised unprivileged process can't ask it to capture arbitrary devices.

## How privilege is acquired (staged)

1. **Dev / spike (first):** run the helper under `sudo` (a dev convenience; the manual M7 Phase-4
   spike against the Solo 2 will do exactly this). Enough to validate the broker + USB capture
   end-to-end against real hardware. **Not CI-testable** (needs root + a physical device).
2. **Product:** ship `limina-privhelperd` as a launchd daemon installed via **`SMAppService`**
   (the modern `SMJobBless` replacement) — one user approval, the OS owns its lifecycle, the app
   talks to it over XPC/UNIX. Code-signed; **no Apple-managed entitlement needed for USB capture**
   (root suffices).
3. **Avoid** setuid-root on the helper binary (brittle, historically a footgun); prefer the
   launchd-daemon model.

## Scope / non-goals

- **Unprivileged-first is unchanged.** Networking (gvproxy NAT and vmnet alike) and the entire
  current feature set need **no** helper. A user who never asks to pass through an Apple-claimed
  USB device without AccessoryAccess never spawns it.
- This doc is the *architecture*; the concrete protocol (message types, the fd-passing handshake) is
  designed when the first client (USB Phase 4) is built.
- Entitlement-gated capabilities (AccessoryAccess) are **not** unlocked by this helper — root ≠
  entitlement. The helper handles the root-but-not-entitlement set; AccessoryAccess is the
  Apple-gated track (`distribution.md` §2.1).

## Cross-references

- `m7-usb-passthrough.md` §Phase 4 — the first client (USB capture); the `LibusbBackend` + the 3b
  pipeline it plugs into are already built.
- `multi-vm-networking.md` §4 — the no-orphan lifecycle this reuses.
- `crates/limina/src/gateway.rs` — the existing orphan-reaping/death-pact pattern to lift.
- Roadmap §M7 — tracked there as deferred.
