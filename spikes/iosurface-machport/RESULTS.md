# Spike: scope scanout IOSurfaces to the window process via a Mach port

**Goal:** close the local security hole where the worker exports each guest scanout as a
`kIOSurfaceIsGlobal` surface, so any same-user process can `IOSurfaceLookup(id)` and read the
guest screen (`spikes/venus-draw-probe/iosdump.swift` is the live PoC — used all through the
runtime-resize work). The fix the backlog calls for: create the scanout surfaces **non-global**
and hand each one to the supervisor (window process) via a **Mach port**, not a global id.

The risky assumption — *can a parent/child Rust pair pass an IOSurface Mach port, and does
non-global actually hide the surface from strangers?* — is what this spike proves, before
touching the real present path.

## What was proven (macOS 26.5, Apple M1 Max)

Build: `clang -Wno-deprecated-declarations -o /tmp/iosmp probe.c -framework IOSurface -framework CoreFoundation`

1. **`bootstrap_register` / `bootstrap_look_up` still work** (deprecated since 10.5, but functional).
   The parent allocates a receive right, inserts a MAKE_SEND, `bootstrap_register`s it under a
   per-instance name (`eti.noronha.limina.spike.<pid>`); the child — which inherits the same
   `bootstrap_port` — `bootstrap_look_up`s the name and gets a send right to the parent. This is
   the parent↔child rendezvous (Mach ports can't ride `SCM_RIGHTS` over the existing unix
   socketpair, so a Mach channel is required).
2. **Mach-port handoff works.** The child creates a NON-global IOSurface, writes a known pixel
   (`11 22 33 44`), `IOSurfaceCreateMachPort`s it, and sends the port to the parent via `mach_msg`
   with a single `MACH_MSG_PORT_DESCRIPTOR` (`MACH_MSGH_BITS_COMPLEX`, disposition
   `COPY_SEND`). The parent receives it and `IOSurfaceLookupFromMachPort`s it back to a live
   `IOSurfaceRef`.
3. **Pixel-accurate.** Parent reads `pixel = 11 22 33 44` from the reconstructed surface — the
   handoff carries the real shared memory, not a copy.
4. **The hole closes.** A *stranger* process (`ioslooker`, no Mach port) calling
   `IOSurfaceLookup(<id>)` on the non-global surface returns **NULL (hidden)**. Contrast: a
   `kIOSurfaceIsGlobal` surface is found by the same stranger (today's behavior; what iosdump
   exploits). Note: the *holder* of a surface can always `IOSurfaceLookup` its own id — that is
   not the hole; the hole is a stranger resolving an id it never received.

Compiler corroboration: `kIOSurfaceIsGlobal` itself is marked
`API_DEPRECATED("Global surfaces are insecure")` — Apple agrees this is the wrong tool.

## Files

- `probe.c` — end-to-end parent/child: rendezvous → port handoff → pixel verify → (intra-process)
  id-lookup note.
- `iosholder.c` / `ioslooker.c` — the clean stranger test: `iosholder {n|g}` creates a
  non-global / global surface and prints its id + holds it alive; `ioslooker <id>` is an unrelated
  process that tries `IOSurfaceLookup(<id>)`. Non-global → NULL; global → found.

Run the stranger test (avoid `&` under fish job-control; use two terminals or `nohup … &` in bash):
```
clang -o /tmp/iosholder iosholder.c -framework IOSurface -framework CoreFoundation
clang -o /tmp/ioslooker ioslooker.c -framework IOSurface
/tmp/iosholder n   # prints e.g. 58, then holds 20 s
/tmp/ioslooker 58  # -> NULL (hidden)
```

## Integration plan (next)

Replace the `surface <id> <w> <h>` / `frame <id>` id-based control protocol with a Mach handoff:

- **Rendezvous setup (supervisor, before spawn).** Allocate a receive port + send right;
  `bootstrap_register` it under `eti.noronha.limina.<pid>.<nonce>`; pass the name to the worker via
  an arg/env (`--surface-port-name`). The worker `bootstrap_look_up`s it once at startup. (Or invert:
  worker registers, supervisor looks up — supervisor-as-receiver is cleaner since it's the long-lived
  parent and already owns the window.)
- **Surface announce.** In `create_global_iosurface` (rename → `create_scanout_iosurface`), drop
  `kIOSurfaceIsGlobal`; after creating the ring, `IOSurfaceCreateMachPort` each surface and
  `mach_msg` the ports to the supervisor, keyed by the same small ring index the protocol already
  uses. The control fd still carries the line protocol for sequencing (`surface <ring_idx> <w> <h>`,
  `frame <ring_idx>`), but the supervisor resolves `ring_idx → IOSurfaceRef` from the Mach ports it
  received, never from `IOSurfaceLookup(id)`.
- **Supervisor side (`window.rs`).** Replace `IOSurfaceLookup(id)` (lines ~511, ~572, ~814) with a
  lookup into the ring-index→`IOSurfaceRef` map populated from the Mach ports. On a mode change /
  reconfigure, the worker sends a fresh set of ports; drop the stale map (mirrors today's
  `cache.clear()`).
- **Reboot/relaunch.** The worker↔supervisor Mach channel must be re-established on worker relaunch
  (same swap point as the scanout socketpair in `spawn_windowed_worker`). The supervisor's receive
  port is long-lived; a relaunched worker just re-looks-up and re-sends ports.
- **Cursor surface** (`window.rs:572`, `iosurface.rs` cursor path) takes the same treatment.
- **RED-first test.** A test that boots a windowed worker and asserts a stranger `IOSurfaceLookup`
  over the announced scanout cannot read it (i.e. the worker no longer creates global surfaces) —
  e.g. assert every scanout surface id is not stranger-resolvable, or simply that
  `kIOSurfaceIsGlobal` is gone and the window still renders (L1 display test stays green).
- **Keep iosdump working as an oracle** for our OWN debugging by having the worker *optionally*
  (behind `LIMINA_GLOBAL_SCANOUT=1`, default off) still mark surfaces global — so the debug oracle
  survives but the shipped default is secure.
