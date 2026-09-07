# Which compute dispatch was in flight when AGX faulted

The AGX crash kills `limina-vmm` inside `copyFromBuffer:toTexture:` (see
`spikes/kk-alloc-pool/RESULTS.md`). Log lines cannot say which dispatch was in flight: whatever is
buffered in stdio dies with the process.

## The mechanism

KK writes each compute copy into a `MAP_SHARED` ring **before** handing it to AGX
(`src/kosmickrisp/bridge/mtl_encoder.m`). The kernel writes back dirty pages of a shared mapping
even when the process dies on SIGSEGV, and there is no syscall per entry — which is what lets it
stay on by default instead of being an option nobody armed when the rare thing finally happened.

Each entry carries `done`, stored 0 before the call and 1 after. **After a crash, the entry still
holding 0 is the dispatch that was in flight.** The culprit names itself rather than being
inferred from whatever happened to be logged nearby.

The ring keeps the neighbours too, so the guilty dispatch can be compared with what surrounds it.

**Each entry also carries the encoder's *generation*.** Encoder addresses are recycled fast — a
measured desktop reuses one within tens of encoders — so the raw pointer carries no identity, and
a run of entries sharing a pointer is not a run on one object. The generation comes from the
liveness table in `mtl_encoder.m`; `enc_state` beside it says whether that incarnation was live,
ended or released when the copy was recorded.

## Using it

The path comes from `LIMINA_KK_DISPATCH_TRACE`, or is derived from `LIMINA_KK_POOL_SNAPSHOT`
(which the supervisor already points at a managed VM's `logs/`), so a managed VM gets it with no
extra wiring. Then:

```
cc -O2 -o dump dump.c
./dump '<bundle>/logs/kk-pool.txt.dispatch.<pid>'
```

It prints any in-flight entry first, then the last 32 completed, oldest last.

## What it has caught

**2026-09-07, the fourth AGX fault.** The ring survived the SIGSEGV and named the in-flight
dispatch: a `45x32` `buf->img` copy, stride 180 — a glyph upload indistinguishable from the
thirty-one before it, which cycled through four fixed shapes. That **retired the theory this spike
was built to test**: the fault does not belong to an unusual dispatch, and nothing about the
request's size is the variable. It agrees with the disassembly, which puts the fault on a NULL
compute-pass pointer rather than on any allocation.

The same crash exposed what the v1 entry could not answer. The faulting encoder pointer appeared
59 times in the ring, which read as one long-lived encoder — but a live desktop recycles encoder
addresses within tens of encoders, so those 59 entries spanned several incarnations. Hence the
generation field: reading a pointer as an identity is exactly the mistake to design out.

The magic is `LDM2` since that change; an older dumper refuses the file rather than reading the
fields at the wrong offsets.
