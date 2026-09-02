# Which compute dispatch was in flight when AGX faulted

The AGX crash kills `limina-vmm` inside `copyFromBuffer:toTexture:`. Two occurrences so far with
a byte-identical register state (see `spikes/agx-blit-pool/RESULTS.md`), and neither could say
**which dispatch** asked for the 32,259-byte data-buffer allocation that straddled the end of
AGX's 1 MiB segment.

Log lines cannot answer that: whatever is buffered in stdio dies with the process.

## The mechanism

KK writes each compute copy into a `MAP_SHARED` ring **before** handing it to AGX
(`src/kosmickrisp/bridge/mtl_encoder.m`). The kernel writes back dirty pages of a shared mapping
even when the process dies on SIGSEGV, and there is no syscall per entry — which is what lets it
stay on by default instead of being an option nobody armed when the rare thing finally happened.

Each entry carries `done`, stored 0 before the call and 1 after. **After a crash, the entry still
holding 0 is the dispatch that was in flight.** The culprit names itself rather than being
inferred from whatever happened to be logged nearby.

The ring keeps the neighbours too, because *"is this dispatch unusual?"* is the question the fault
actually turns on — a 1 MiB segment holds only ~32 allocations of the failing size, so the guilty
one should look nothing like the small glyph uploads around it.

## Using it

The path comes from `LIMINA_KK_DISPATCH_TRACE`, or is derived from `LIMINA_KK_POOL_SNAPSHOT`
(which the supervisor already points at a managed VM's `logs/`), so a managed VM gets it with no
extra wiring. Then:

```
cc -O2 -o dump dump.c
./dump '<bundle>/logs/kk-pool.txt.dispatch.<pid>'
```

It prints any in-flight entry first, then the last 32 completed, oldest last.

## Verified

On a booted desktop: 467 dispatches recorded with correct geometry — the small glyph and icon
uploads (`8x1`, `19x22`, `144x64`, `208x64`) that vrend's `transfer_write_iov` path produces — and
correctly reporting no in-flight entry while the process is alive.

**Not yet verified against a real crash**, which needs the fault to recur on an instrumented
build. That is the point of shipping it: the next occurrence is either explanatory or it tells us
the copy path is not where the 32 KB request comes from, and both are progress.
