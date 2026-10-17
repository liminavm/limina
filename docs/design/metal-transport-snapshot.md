# Suspend and restore for the Metal-level GPU transport

A VM suspended with KosmicKrisp clients running resumes with those clients running, as one
suspended with venus or vrend clients does (`docs/design/venus-snapshot-replay.md`). The
transport is restored the venus way: what can be copied is copied, and the host objects are
rebuilt from the guest's own commands. The difference is that the guest holds raw host values.

## What there is to keep, and where it lives

| state | lives in | kept by |
|---|---|---|
| the module's contexts, arenas, rings, fences; every KK process | guest RAM | the RAM snapshot |
| arena contents, the rings included | GPU host memory mapped into the arena window (shm 0x80) | the worker, which maps it: saved beside the RAM |
| doorbell pages (shm 0x81) | worker | nothing: stateless, re-mapped at the same offsets |
| the extension's bookkeeping: contexts and their doorbell slots, arenas (window offset, GPU address), rings, exported arenas as blob resources and their maps, imports, surface bindings | libkrun's GPU extension (`ext.rs`) | the extension's section of the GPU payload (`ExtSnapshot`), restored before anything maps the windows |
| window surfaces (`SURFACE_CREATE`) | IOSurfaces virglrs mints for the worker | the extension's section, with their rows, re-created **before vrend's replay**: other contexts' replay names them |
| host objects (textures, views, samplers, libraries, pipelines, depth-stencil, argument tables, events, shared events, counter heaps) | the GPU host | the GPU host's journal, replayed into a fresh GPU host |

## Host values the guest holds

KosmicKrisp writes two kinds of host value into memory the GPU reads: arena GPU addresses
(`gpu_base` + offset, in argument tables and in applications' own buffers through buffer device
addresses), and resource IDs of textures, texture views and samplers (in argument buffers).
Metal takes neither as a request. A restore reproduces them, measured in
`crates/limina-gpu-host/tests/first_light.rs` (the ignored `are_host_values_*`, `how_host_values_*`,
`can_placeholders_*`, `where_arenas_*` and `do_other_objects_*` tests), `tests/arena_space.rs` and
`metal.rs`'s `footprints_of_small_objects`:

- **Arenas** are placed lowest-address first-fit on 32 KiB boundaries, with freed neighbours
  merged. Each allocation takes its size plus 16 KiB, rounded up to 32 KiB (a 16 KiB arena takes
  0x8000, a 1 MiB one 0x108000), from 16 KiB to 2 GiB; buffers of 8 KiB or less come out of a
  pooled 0x28000. Other allocations share the space: the process's first context start takes
  0x68000 after the first arena (later ones take none), a timestamp counter heap 0x8000, a
  dispatch 0xd0000. So every hole is a whole number of granules, and a
  placeholder arena 16 KiB smaller than the hole fills it exactly. The restore places every arena
  before it starts anything else, in address order, filling each hole below a recorded address
  with such a placeholder whatever made the hole, and frees the placeholders once every arena
  stands.
- **Resource IDs** come from two spaces per GPU host process, textures (views included) and
  samplers, each slot reused once Metal has released it, not strictly lowest-first. The fill runs
  over every context's objects together, since two clients share one space. Objects are created
  in dependency order (a view after its parent, whatever their IDs) and each is brought to its
  recorded ID: placeholders hold the slots below it, and a slot that a placeholder already holds
  is freed and taken, retrying while Metal's release is pending.
- **Every value is verified.** A mismatch is a bug in the above. In a context's objects it loses
  that context (its ring reports `LMN_ERROR_RESTORE`) and touches nothing else; an arena that
  cannot be placed fails the whole GPU restore, so every renderer's session restarts (hardening
  backlog).
- **An arena only its export still holds** (the context that made it destroyed it, or is gone)
  is restored in a GPU host context 0, which the guest can never name, exported again, and the
  holder dropped at the end: the export keeps the arena.

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
shared event's value, and an event's highest value queued to signal (the GPU host shadows it;
an `MTLEvent` cannot be read). A restored event is brought to its value by a signal from a
queue, as `Executor::run` already does at teardown.

The ring is resumed, not re-attached: its control block lives in arena memory and is restored
with it, and the consumer picks up at the saved position.

**A command buffer recorded but not committed** is kept as bytes. From the first `BEGIN_COMMAND_BUFFER`
until `COMMIT`, the GPU host copies every command it consumes, with what each create answered, and
keeps the object table as it was when the copy began (objects created since left out, objects
destroyed since kept, argument tables' earlier addresses). The export describes the context as
the copy began and carries the copy; the restore runs it again before the ring, bringing each
create to the answer the guest already holds. The ring alone cannot be rewound: the guest may have
reused the bytes it already consumed.

Counter heap entries are not kept. A timestamp written before the suspend and resolved after it
reads zero.

## Order

Suspend (the worker, with the vCPUs quiesced): each context is exported, which gives its committed
work up to 2 s; then arena contents and surface rows are read; then the fence drain; then the rest
of the GPU section. A commit still in flight when the time runs out (one waiting on a shared event
the frozen guest would set) cannot be taken back, since it may have partly run: the context is
lost there and then, so its ring reports it and the fences waiting on it are answered before the
drain, and it is restored lost.

Restore, before the guest's DRIVER_OK is released: a fresh GPU host; contexts with their doorbell
slots; every arena of every context in one address order, with its contents, window arenas mapped
at their offsets; exports (their blobs mapped where they were) and imports; surfaces contexts made,
re-created with their rows and given back; then vrend's and venus's replay; then surfaces contexts
imported; then each ring, from its export (objects, then state, then the open command buffer);
then the placeholders go. A replay that dropped any command fails the restore: a lossy restore
must fail when it happens, not at the next frame that touches what was lost.

## Tests

- `crates/limina-gpu-host/tests/first_light.rs`: exports list what a context holds and carry
  open command buffers; arenas come back at their addresses; a context with holes below its IDs,
  a pipeline whose library is gone and a view whose parent is gone restores into a fresh GPU host
  and exports the same, and works; one restored inside an open command buffer commits it; an
  export that cannot drain loses the context.
- libkrun `ext.rs`: two contexts, window arenas around a hole, a ring, an export imported
  elsewhere, an orphaned export and a surface held twice, snapshotted and restored into a fresh
  device, snapshot the same again; the section's encoding round-trips and refuses truncation.
- The L2 suspend test with a KosmicKrisp client on screen: after the resume, no vrend context is
  refused, the guest faults on no window, the frame keeps changing, and the client is still
  drawing.
