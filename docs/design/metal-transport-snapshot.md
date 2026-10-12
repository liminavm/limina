# Suspend and restore for the Metal-level GPU transport

A VM suspended with KosmicKrisp clients running resumes with those clients running, as one
suspended with venus or vrend clients does (`docs/design/venus-snapshot-replay.md`). The
transport is restored the venus way: what can be copied is copied, and the host objects are
rebuilt from the guest's own commands. The difference is that the guest holds raw host values.

## What there is to keep, and where it lives

| state | lives in | kept by |
|---|---|---|
| the module's contexts, arenas, rings, fences; every KK process | guest RAM | the RAM snapshot |
| arena contents, the rings included | replayer memory mapped into the arena window (shm 0x80) | the worker, which maps it: saved beside the RAM |
| doorbell pages (shm 0x81) | worker | nothing: stateless, re-mapped at the same offsets |
| the extension's bookkeeping: guest context ↔ replayer context, arena ↔ window offset, exported arenas as blob resources and their maps, imports, surface bindings, doorbell pages | libkrun's GPU extension (`ext.rs`) | libkrun's GPU section, restored before anything maps the windows |
| window surfaces (`SURFACE_CREATE`) | IOSurfaces virglrs mints for the worker | libkrun's GPU journal, re-created with their pixels **before vrend's replay**: other contexts' replay names them |
| host objects (textures, views, samplers, libraries, pipelines, depth-stencil, argument tables, events, shared events, counter heaps) | the replayer | the replayer's journal, replayed into a fresh replayer |

## Host values the guest holds

KosmicKrisp writes two kinds of host value into memory the GPU reads: arena GPU addresses
(`gpu_base` + offset, in argument tables and in applications' own buffers through buffer device
addresses), and resource IDs of textures, texture views and samplers (in argument buffers).
Metal takes neither as a request. A restore reproduces them, measured in
`crates/limina-replayer/tests/first_light.rs` (the ignored `are_host_values_*`, `how_host_values_*`,
`can_placeholders_*` and `where_arenas_*` tests):

- **Arenas** are placed lowest-address first-fit, with freed neighbours merged and 32 KiB between
  arenas. Other allocations share the space (the process's first context start takes 0x68000
  after the first arena; later context starts take none of it). The restore therefore places
  every arena before it starts anything else, in address order, filling each gap below a recorded
  address with a placeholder arena whatever made the gap, and frees the placeholders once every
  arena stands.
- **Resource IDs** come from two spaces per replayer process, textures (views included) and
  samplers, each slot reused once Metal has released it, not strictly lowest-first. The fill runs
  over every context's objects together, since two clients share one space. Objects are created
  in dependency order (a view after its parent, whatever their IDs) and each is brought to its
  recorded ID: placeholders hold the slots below it, and a slot that a placeholder already holds
  is freed and taken, retrying while Metal's release is pending.
- **Every value is verified.** A mismatch is a bug in the above, and loses that context (its
  ring reports lost); it never touches the device or another context.

Compute pipelines also answer a value (max threads per threadgroup); it is a property of the
pipeline, the same on replay, and is verified like the rest.

## The journal

Each host object owns the record of its creation: the command's bytes and the value it answered.
Destroying the object drops its record, so there is no destroy site to keep in step. A record
holds the records it referenced by `Arc` (a pipeline its libraries, a view its parent texture, a
texture over a surface its surface), so a create still replays after the guest destroyed what it
named. **References resolve through the record graph, never through guest handles**: the guest
reuses handles (library 5 destroyed, texture 5 created), so a referenced object that has no live
handle is rebuilt under none, in a restore-time map from record to object. Library records keep
their MSL source for the life of what uses them.

State changed after creation is read from the object when the journal is exported, never logged
as it happens: an argument table's addresses (the table keeps a shadow of what it was set to), a
shared event's value, and an event's highest value queued to signal (the replayer shadows it;
an `MTLEvent` cannot be read). A restored event is brought to its value by a signal from a
queue, as `Executor::run` already does at teardown.

The ring is resumed, not re-attached: its control block lives in arena memory and is restored
with it, and the consumer picks up at the saved position.

**A command buffer recorded but not committed** is kept as bytes. The replayer copies the commands
of an open command buffer as it consumes them and drops the copy at `COMMIT`; the export carries
the copy, and the restore runs it again before the ring. The ring alone cannot be rewound: the
guest may have reused the bytes it already consumed.

Counter heap entries are not kept. A timestamp written before the suspend and resolved after it
reads zero.

## Order

Suspend: the guest quiesces (PSCI SYSTEM_SUSPEND); each context's committed work is given a
bounded time to complete; each context's journal is exported; the worker saves arena contents and
the journals with the GPU section. A commit still in flight when the time runs out (one waiting on
a shared event the frozen guest would set) cannot be taken back, since it may have partly run:
that context is restored lost, and the log names it.

Restore, before the guest's DRIVER_OK is released: a fresh replayer; contexts; arenas, at their
addresses, with their contents copied in and mapped at their window offsets; rings resumed;
window surfaces re-created and filed to their contexts; then vrend's and venus's replay; then
each context's journal (objects, then state, then the open command buffer).

## Tests

- `crates/limina-replayer/tests/first_light.rs`: build state in one replayer (arenas with holes,
  two contexts, textures, views, samplers, a pipeline whose library the guest destroyed, an
  argument table, events), export it, import it into a fresh replayer, check every recorded
  address and ID, and draw through a restored argument table into a texture that reads back
  right.
- The L2 suspend test with a KosmicKrisp client on screen: after the resume, no vrend context is
  refused, the guest faults on no window, the frame keeps changing, and the client is still
  drawing.
