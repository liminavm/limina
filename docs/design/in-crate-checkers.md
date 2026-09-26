# In-crate checkers: Kani, loom, Miri, cargo-fuzz and the sabotage sweep

Status: **phases 1–4 landed** (see *Measured so far*; what each phase left, and why, is under
*Phases*) · Scope: limina's own crates, the guest workspace, and the libkrun fork's `limina`
branch · Model: virglrs (`third_party/virglrs/docs/design.md`, *Owed,
and waiting on work → In-crate checkers*; `third_party/virglrs/harness/sabotage/sweep.py`)

## The problem

The HVF suite (`scripts/run-suite.sh`) is limina's oracle for "does it boot and behave", and it
drives only the shipped binaries. That is the right top layer. It cannot state a property of an
internal type, though. It cannot say that no byte sequence a guest sends over USB/IP panics the
supervisor. It cannot say that the balloon coalescer never releases a host page with a live guest
page in it, for every run the guest can report. It cannot say that a keymap never leaves a
modifier stuck in the guest, for every order of presses and releases. A boot passes whether or
not any of these hold. The unit tests cover the cases someone thought of, and a hostile or buggy
guest is not limited to those.

virglrs hit the same gap below its public ABI and closed it with five tools and one rule. This
document records how the approach maps onto limina, what each tool fits, the targets in order,
and what stays out of reach.

## What virglrs learned (the parts that carry over)

Each tool fits a different shape of code. Picking the wrong one either takes forever or proves
nothing:

- **Kani** proves a property for every input up to a stated bound: no panic, no overflow, no
  out-of-bounds access, plus any assertion the harness adds. It checks overflow whatever the
  Cargo profile says, which matters because a release build wraps silently. It pays where
  **control flow is fixed, data is wide, and nothing allocates**. virglrs proved
  `RingLayout::parse`, the decoder's read bounds and `Iov::walk_from` in under a minute and a
  gigabyte each. Its object table ran to 22 GB with no verdict, and `sync::decode`, which builds
  a `Vec`, ran past 10 minutes at 6 GB. The harness counts as much as the code: comparing two
  slices of symbolic length unrolls `memcmp` without bound, so a harness compares one index Kani
  picks. `cbmc` outlives a `timeout` wrapped around `cargo kani` and has no memory cap, so an
  exploratory run is watched and `cbmc` killed by process group.
- **Exhaustive enumeration** in a plain `cargo test` takes state machines whose domains are
  small by design. It tries every operation sequence to a fixed depth and checks each step
  against a model kept outside the thing under test. Each walk asserts it reached the cases it
  exists for. These live in modules named `every_sequence`. This is where code that allocates or
  branches on accumulated state goes when Kani will not finish.
- **loom** tries every interleaving of the threads involved. A module opts in by taking its
  `Arc`, `Mutex`, `Condvar` and thread from loom under `cfg(all(test, loom))`. Its models build
  with `RUSTFLAGS="--cfg loom"` in their own `CARGO_TARGET_DIR`, so the two builds do not evict
  each other. loom weakens `SeqCst` loads to acquire/release and cannot see atomics that are not
  its own, so a handshake built on either stays with the std tests. It has no `select` either, so
  a handshake built on crossbeam's stays out of reach too. Its timed waits never time out, which
  turns a lost wakeup into a deadlock loom reports.
- **Miri** runs the unit tests that make no foreign call and checks the aliasing rules Kani does
  not. A foreign call ends the whole run, so a sweep script reruns with `--skip` past each test
  Miri stops on and records what actually ran.
- **cargo-fuzz** takes parsers too large to prove. Targets live in a `fuzz/` directory that is
  its own workspace, so the main build never sees libFuzzer. Every target checks for no panic. A
  target with a second property checks it too, usually a round trip (decode, encode, decode
  again, and get the same thing). Random bytes almost never form a real message, so corpora are
  seeded from real captures.

And the rule that ties them together: **every gate is armed by breaking the property it holds
and watching it fail.** `sweep.py` holds a list of one-line edits, each of which makes the code
wrong in a way that matters. It applies each edit alone to a clean tree, runs the named witness,
reverts, and reports `RED` (caught) or `SURVIVED` (a hole). Every edit asserts it matched, so a
refactor cannot turn an entry into a silent no-op. A witness is a `cargo test` filter or one of
`kani:<harness>`, `loom:<test>` or `doc:<filter>`. Each Kani proof and loom model is run on the
clean tree first, because `cargo test` never runs them and a proof already failing would read
every sabotage as caught. An entry lands with its witness, not after it.

## Where limina's value lies

Two kinds of code in limina fit these tools: host code that parses bytes a guest controls, and
pure policy arithmetic. **Miri will do little here.** Most of limina's `unsafe` is a call into
AppKit, IOSurface, Mach, HVF or Linux syscalls (`guest/limina-init/src/main.rs` alone has 53
uses), and Miri stops at every one. It stays as a cheap sweep over the pure crates, not a
priority.

### Tier A: guest-controlled bytes parsed by the host

This is the VM-escape surface. A panic here takes down the worker or the supervisor; a wrong
bound is worse.

| Target | Tool | Property |
|---|---|---|
| libkrun xHCI: `RingWalker::next` (`third_party/libkrun/src/devices/src/usb/xhci/trb.rs:209`), `EventRing` (`:237`), `usb/xhci/engine.rs` | Kani on the ring cursor's link-TRB and cycle-bit walk; fuzz TRB streams over plain-mmap guest memory; enumerate slot and endpoint state transitions | A walk never leaves its segment; no guest TRB sequence panics or reaches a state the spec forbids |
| FIDO: `request::parse` (`crates/limina/src/fido/request.rs`), guest CBOR reaching the host authenticator | fuzz, raw and against a model of the rules | No panic, no unbounded allocation, and the rules as CTAP2 states them. Highest consequence in the table. |
| Control plane: `FrameHeader::decode` (`crates/limina-proto/src/lib.rs:865`), `read_message` (`:915`) | Kani on the header (16 bytes, exhaustively provable); fuzz `read_message` with a round trip | `MAX_PAYLOAD` holds; an unknown type decodes to `Message::Unknown` and is never a stream error |
| vdagent: `decode` (`crates/limina/src/vdagent/codec.rs:235`), `Reassembler::push` (`:345`) | fuzz, plus enumeration of split points | **Any split of the same bytes yields the same messages**, which is what `push`'s doc promises |
| Snapshots: the decoders in `third_party/libkrun/src/vmm/src/snapshot.rs` (`decode_vcpu` `:715`, `decode_usb` `:1127`, …), `GpuSnapshotPayload::from_bytes` (`third_party/libkrun/src/devices/src/virtio/gpu/journal.rs:827`) | fuzz with a round trip, seeded from real snapshots | A corrupt snapshot is refused before restore starts, never a panic halfway through |

`limina-usbip` is left out on purpose. Only `limina-test` uses it; the shipped USB path is
libkrun's xHCI. If it ever ships, note first that `serve_urbs`
(`crates/limina-usbip/src/server.rs:85`) sizes a buffer from the guest's 32-bit
`transfer_buffer_length` with no cap, so one URB can ask the host for 4 GiB.

The snapshot file is not guest input, but it is read back from disk after a crash, a partial
write or a version skew, and a panic mid-restore loses the VM.

### Tier B: arithmetic and state invariants

- **The balloon coalescer.** `ReclaimCoalescer` (`third_party/libkrun/src/devices/src/virtio/balloon/device.rs:82`)
  states its own invariant: a host page is emitted only when every guest page in it was reported
  free, and unaligned fringes round inward. It keeps a `HashMap`, so the whole thing is an
  enumeration target: every sequence of `add` runs over a few host pages at sub-page
  granularity, checked against a bitmap of what was reported, with `take_full_pages` (`:143`)
  never emitting a page the bitmap does not fully cover. The per-run arithmetic in `add` is
  proved with Kani (see *Measured so far*). A guest can make `add` walk up to a million pages
  per descriptor, since `len` is its own 32-bit figure and only the run's start is checked
  against guest memory. That costs only the guest's own device thread, and a run overhanging
  guest RAM releases nothing outside it, because `ReleasedRam::release` refuses a range that
  crosses its region's end.
- **Released RAM.** `third_party/libkrun/src/hvf/src/released_ram.rs` says "the released set
  must be exact", because `hv_vm_map` fails on any overlap. That calls for enumeration of
  `release` (`:199`) and `handle_fault` (`:270`) sequences against a model of which pages are
  mapped, over a small guest range. A loom model of `release` racing `handle_fault` follows,
  since a race there corrupts guest memory. `hv_vm_*` and `madvise` sit behind a seam the model
  replaces.
- **Balloon policy.** The pure functions in `crates/limina/src/balloon_policy.rs` (`decide`
  `:2111`, `inflate_bound` `:1863`, `allowance_pages` `:1916`, `scrub_target_pages` `:1494`,
  `gap_action` `:1623`, `giveback_floor_pages` `:1974`) are Kani targets. The properties: the
  target stays within `[0, max − min]`; nothing inflates at or above `PRESSURE_HIGH`; an
  inflation step respects the MemFree clamp; and no arithmetic overflows. `vcpu_policy.rs` has
  the same shape.
- **Input and grab state machines.** These are enumeration targets:
  - `limina-input` (`keymap.rs`, `hidkbd.rs`, `router.rs`): every press and release sequence, under
    every remap configuration, leaves no key or modifier held in the guest once the host has
    released everything.
  - `crates/limina/src/window/grab_policy.rs` (`reveal_step` `:284`, `free_step` `:662`,
    `press_step` `:879`): no event sequence leaves the grab held with no owner window.
- **Float geometry** (`window/fit.rs`, `absfit.rs`, `arrangement.rs`) comes last. Kani handles
  floats slowly, and a bug there costs UX rather than safety.

### Tier C: concurrency

loom candidates were `crates/limina/src/control.rs` (connect, reconnect, the shutdown
handshake); `crates/limina-vmm/src/power.rs`, `quiesce.rs` and `wake.rs` (quiescing vCPUs is
what snapshots stand on); the frame handoff in `crates/limina/src/window/present.rs`; and
`third_party/libkrun/src/vmm/src/macos/vcpu_sched.rs`. Each needs its sync primitives behind a
`cfg(all(test, loom))` shim first. Anything that crosses HVF, AppKit or guest memory stays with
the HVF suite. Reading them sorted them into three: the control plane's clipboard greeting, the
band sampler against a vCPU's guard, and the power watch are loom's, and have models. The
host-sleep bracket and the frame handoff are not, for the reasons under *Measured so far*.

## Layout and commands

- **limina's own checkers.** Kani proofs live in `#[cfg(kani)]` modules beside the code, loom
  models in `loom_model` modules under `cfg(all(test, loom))` beside the code, and enumeration in
  `every_sequence` test modules. A module with a model takes its locks from loom under
  `cfg(all(test, loom))` and compiles its ordinary tests out there. The shim keys on `test` as well
  as `loom` because loom is a dev-dependency: a crate built as a dependency of another crate's loom
  run has no loom to use, and keeps std's. Fuzz targets live in a root `fuzz/` directory that is
  its own workspace, excluded from the main one like `xtask/`. Each crate with a proof or a model
  declares `cfg(kani)` and `cfg(loom)` under `unexpected_cfgs` in its own `[lints.rust]`, as
  virglrs does in its `Cargo.toml`; the workspace has no shared lint table to put it in.
- **libkrun's checkers** go on the fork's `limina` branch beside the code they check, with a
  `fuzz/` directory of their own. They follow the fork model like every other libkrun change.
- **Modules of the `limina` binary.** `limina` has no library target, so a fuzz target reaches
  one of its modules by compiling it in from its own source file (`fuzz/src/lib.rs`). That works
  only for a module that names nothing else in the crate, such as `vdagent/codec.rs`. A module
  that does needs a seam first.
- **The sabotage sweep** is `scripts/sabotage-sweep.py`, a port of virglrs's. An entry names the
  file it edits and the crate directory its witness runs in, so it can target either tree. Only
  the files it edits must be clean, because this tree always holds untracked images and scratch.
- **Commands.** `cargo xtask check kani [crate-dir ...]`, `cargo xtask check fuzz [target ...]
  [--seconds N]` and `cargo xtask check sabotage [pattern ...]` wrap `scripts/check.py`, which
  remains the source of truth, following the one-command convention (`xtask/src/main.rs`).
  `cargo xtask check loom [crate-dir ...]` runs every `loom_model` module under `--cfg loom` in
  `target/loom`, from the library target, or from the binary for a crate without one (`limina`). Miri gets its subcommand when its first sweep lands. Kani runs on its own
  pinned toolchain (`cargo install --locked kani-verifier && cargo kani setup`), and fuzzing on
  nightly (`cargo install --locked cargo-fuzz`). None of this needs HVF or codesigning.
- **Stopping Kani.** Proofs run under `-Z unstable-options --harness-timeout 10m`, which stops
  `cbmc` itself. A `timeout` around `cargo kani` does not stop it, and neither does killing
  `cargo kani`; kill its process group. A proof that needs longer than the limit is the wrong
  shape for Kani, not a reason to raise the limit.
- **Cadence.** None of these join the pre-commit hook or the 38-minute HVF suite. Enumeration
  tests are ordinary `cargo test` tests and run wherever those do. Kani, loom and the sabotage
  sweep run on demand and before a change to the code they cover lands. Fuzzing is time-boxed:
  `-max_total_time`, one target at a time.
- **Crashes become regression tests.** A fuzz crash is minimized and committed as a failing unit
  test before the fix, per the RED-first rule in `CLAUDE.md`. Corpora are not committed; the
  seeding scripts that rebuild them from real captures are.

## Measured so far

Measured 2026-09-25 on the dogfood Mac (M4 Pro, on battery), Kani 0.68.0 / CBMC 6.11.0,
cargo-fuzz 0.13.2.

- **Control-plane header.** `FrameHeader::parse` (`crates/limina-proto/src/lib.rs`) accepts
  exactly the headers with the magic and a payload within `MAX_PAYLOAD`, for every one of the
  2^128 headers, and every bounded header round-trips. Each proof takes under a second. The
  first attempt, a proof of `decode` itself, ran 10 minutes to 2.3 GB with no verdict: its
  refusal formats an `io::Error` message, and Kani models the allocation even on paths the proof
  never takes. `decode` now wraps an allocation-free `parse`, and the proofs are of `parse`.
- **Balloon policy.** Four proofs over every guest report and every policy state
  (`crates/limina/src/balloon_policy.rs`): a target never exceeds the room; acute pressure or
  starvation only releases; inflation needs a calm guest and moves at most one step; at host
  Normal an inflation step never digs past the free-list margin. Each takes 1 to 3 seconds. The
  clock was the whole cost. `Instant::now` reaches `clock_gettime`, which Kani does not model,
  so the proofs build their instants from a fixed one. `Instant::duration_since` normalizes
  through `Duration::new`, whose division by 10^9 kept every harness with a symbolic clock past
  10 minutes and 3 GB, under CaDiCaL and Z3 alike. A second division, of one guest figure by
  another in `io_pain_can_be_ours`, turned out to cost nothing. Found by bisecting: symbolic
  report with concrete state, 0.45 s; symbolic state with its time fields cleared, 1.1 s. The
  proofs stub `duration_since` to answer any elapsed time, which is sound because none of the
  properties depends on time.
- **Balloon coalescer** (libkrun fork, `src/devices/src/virtio/balloon/device.rs`). `add`'s
  arithmetic is split into `inward` and `locate`, unchanged, because the coalescer's `HashMap`
  seeds its hasher from the OS, which Kani cannot model. One proof covers every run a guest can
  report and every host page from 4 KiB to 256 KiB. Every page `add` marks free is a whole guest
  page inside the run, filed in the host page and slot that hold it, at the right GPA. One page
  Kani picks stands for all of them, so nothing is unrolled; it proves in under a second. Its
  one assumption is load-bearing: the run's host address and GPA must sit at the same offset
  into their host pages, and without it `locate` underflows. It holds because guest RAM starts
  at 1 or 2 GiB (`src/arch/src/aarch64/layout.rs`) with page-rounded sizes and page-aligned host
  mappings. The first version of the harness picked any aligned page in the run rather than the
  pages `add` walks, so a start that was never rounded went unseen; writing the sweep entry
  exposed it. Two entries were first caught by an overflow in the harness's own arithmetic,
  not by the assertion about the broken code, so the assertions are now written so they cannot
  overflow themselves.
- **Fuzzing.** `control_frame` (every frame `read_message` accepts round-trips) ran 7.4 M
  inputs in 60 s with no crash. `vdagent_stream` (pushing a stream whole or split gives the same
  messages) ran 5.1 M in 60 s with no crash, and caught a planted bug (refusing a chunk whose
  body had not fully arrived) within seconds.
- **FIDO requests.** `ctap2::handle` parsed each request in the middle of the ceremony it
  asked for, so it could not be exercised without the enclave. `fido/request.rs` now parses a
  message whole into a typed request before `ctap2` touches the store, the enclave or Touch ID;
  every refusal and its order are unchanged. `ctap2_request` (any bytes) ran 2.8 M inputs in
  60 s with no crash, and 4.2 M more under a 32 MB per-allocation cap found no declared length
  that becomes a large host allocation. `ctap2_roundtrip` builds well-formed requests from
  fields the fuzzer picks and checks `parse` against a model of the rules. It ran 700 k in 60 s
  with no crash, and caught a planted bug (an empty allowList read as present) within seconds.
  The sweep found that no test noticed the ES256 requirement being removed; one does now.
- **xHCI** (libkrun fork, `fuzz/`). `xhci_guest` drives the controller only as a guest can:
  register reads and writes, pointers into its RAM, TRBs and contexts planted there, worker
  passes through `run_pass` (the worker thread's own loop body, public under `cfg(fuzzing)`),
  and snapshots that must restore to the same state on a controller with the same gadgets.
  The first five-minute run, with inputs from an `Arbitrary` derive, was clean and worth nothing:
  `xhci-depth`, which replays a corpus and counts the stages each input reached, found none of
  472 inputs had built an event ring. Random bytes do not point four registers at rings laid out
  in RAM. So inputs became a byte format of the harness's own, and `xhci-seeds` writes a whole
  driver bring-up (reset, rings, two slots enabled and addressed, a descriptor fetch, an
  interrupt endpoint configured and fed, snapshot, stop, reposition, disable) as every prefix.
  Seeded, the target found two guest-triggerable panics of the ring worker, each while it held
  the controller lock, which poisons it for every later MMIO access:
  - Address Device indexed the 9-entry slot table with the slot id from the command TRB,
    unchecked; slot 255 panicked, and a vacant slot in range was created instead of refused.
    Found in 6 k inputs.
  - The event segment's base comes from the guest's ERST entry, and its end overflowed the
    address space; debug builds panicked, release builds wrapped. Found in 11 k inputs.

  Both are fixed on the fork with regression tests. A 15-minute run after the fixes executed
  546 k sessions clean, and coverage went from 998 edges (unseeded) to 2006; of the 2160-input
  corpus it left, 336 addressed a device and 321 configured an endpoint. The fuzz build checks
  overflow and a release build does not, so a fuzz overflow is also a wrap in the shipped binary,
  and worth reading for what the wrap does.
- **xHCI ring walker** (libkrun fork, `usb/xhci/trb.rs`). The walk moved into `next_from`
  over any TRB source, unchanged, because guest memory is an mmap. The proof gives every read an
  arbitrary TRB or a failed access, independently, which stands for every ring and for a guest
  rewriting its ring mid-walk. One step returns only a work TRB read with a matching cycle bit
  and leaves the walker past it, parks on the producer boundary, keeps the pointer aligned,
  tracks Toggle Cycle, and reports a link loop only after 64 links. About a minute, with the
  loop unwound 66 times.
- **xHCI slot commands** (libkrun fork, `usb/xhci/engine.rs`, `every_sequence`). Every
  sequence of 25 slot commands over two slots the walk can enable and one it never can, to depth
  four: 390,625 sequences, 11 s in a debug build, each step checked against a model of the slot
  table. The model is the spec's for the states commands leave slots in, and records two
  deliberate departures: a command on a slot that is not enabled completes with nothing changed
  (the guest's teardown of a slot a restore dropped must complete), except Address Device; and a
  command in the wrong state is accepted rather than refused. It found nothing new.
- **Crate features.** `krun-devices` compiles its USB code only with `--features usb`. Without
  it, a proof is compiled out and Kani reports the crate verified with nothing checked, and a
  test filter matching no test passes. `check.py` and the sweep pass the features per crate.
- **Released RAM** (libkrun fork, `src/hvf/src/released_ram.rs`). The module says its
  released set "must be exact", but `release`, `handle_fault` and `reclaim` called `hv_vm_*` and
  `madvise` directly. They now go through a `Stage2` trait, which `ReleasedRam` takes as a type
  parameter defaulting to HVF, so no caller changed. Modelling it raised a premise nobody had
  measured: what `hv_vm_unmap` does to a page already unmapped, which a page released twice
  reaches. `spikes/hv-unmap-semantics` measured it: unmap is page-wise and idempotent, map refuses
  any overlap. The enumeration walks every sequence of 33 operations (releases, reclaims, guest
  touches, raw faults, injected unmap and map failures) to depth four over two GPA-adjacent
  regions and a page that is not RAM: 1,185,921 sequences, 13 s in a debug build. After every step
  the set and a stand-in for stage 2 must both equal a page-level model. It found three ways for
  the set to stop being exact, each now fixed with a named test:
  - a release repeating an earlier one, whose unmap then failed, rolled back and erased the
    earlier release, leaving its pages unmapped with no record;
  - a reclaim or heal whose map failed dropped the ranges after it from the set;
  - two releases on each side of a region boundary coalesced into one range, which the heal
    mapped from the first region's host base, putting host memory that belongs to neither region
    into the guest. Latent: no RAM layout has GPA-adjacent regions today.

  It also found the heal window running a chunk past a clipped start, against its comment. That
  was harmless, since RAM regions start chunk-aligned, but it is fixed. The loom model has three
  races over one two-page region: a release racing a heal of its window, two vCPUs faulting on one
  page, and a reclaim racing a heal. They check what `release`'s comment asks for: with the
  discard moved outside the lock, a heal lands between the unmap and the discard and leaves a live
  page marked reusable. Each model runs in about 0.01 s, and the loom build takes 20 s. loom brings
  a second `syn` major into the fork's lockfile, as a dev-dependency under `cfg(loom)` only.
- **Balloon coalescer, enumerated** (libkrun fork). Every sequence of three reported runs, each
  starting at any half guest page and 1, 2, 3 or 8 halves long, over three host pages in two
  regions, through `take_full_pages` and `merge_runs`: 438,976 sequences in under a second.
  Each is checked against a model that frees a guest page only when one run covers it and
  releases a host page exactly when all its guest pages are free. It found nothing in the code.
  The sweep found a hole in the test: its check that no two merged runs could merge again only
  compared neighbours, so an unsorted merge went unseen. It now compares every pair.
- **Snapshots** (libkrun fork, `src/vmm/src/snapshot.rs`, `fuzz/`). The reader parses from a
  file streaming in on a thread; under `cfg(fuzzing)` it also parses bytes already in memory, and
  the head and frame CRCs are computed but not enforced, since a fuzzer cannot forge them.
  `snapshot_head` reads a head and requires a write, read and write again to give the same bytes.
  `snapshot_ram` applies a whole file's frames into 8 MiB + 64 KiB of guest memory.
  `snapshot-seeds` writes the seeds from a real snapshot's head (3.7 GB, an F44 enhanced desktop
  from `spikes/suspend-perf`), without its GPU section. Found and fixed, each RED first:
  - the head's vCPU, register, device and queue counts sized `Vec::with_capacity` before the
    head CRC is checked; a fuzzed head asked for 22.9 GB within seconds. They go through
    `bounded_count` now, as the xHCI section's did;
  - a region's chunk size sizes every worker's frame buffer, and a zero frame costs nine bytes,
    so a 4 GiB chunk had a worker zero-fill 4.3 GB before the write into guest RAM refused it.
    Every readable version writes 4 MiB chunks, so anything larger is refused;
  - a region near the top of the address space overflowed a frame's address: a panic in debug,
    a wrapped address in release.

  The fuzzer found only the first. The other two need a region's length, chunk and frame count
  changed together, which `snapshot_ram` did not manage in 107 k inputs; they came from reading
  the walk it exercises. After the count fix `snapshot_head` ran 3.6 M inputs in five minutes
  clean (1,697 edges), and after all three `snapshot_ram` ran 118 k in two. The sweep then found that no test enforced the
  head CRC: the test for it corrupted the vCPU count, which the parse refuses before the CRC
  matters. It now corrupts the GIC blob.
  `GpuSnapshotPayload::from_bytes` (`virtio/gpu/journal.rs`) is not fuzzed. It needs the `gpu`
  feature, which would bring rutabaga and virglrs into the fuzz build, and it runs only on a
  payload the head CRC has vouched for. It also sizes `with_capacity` from a count in its bytes,
  so a writer bug or a version skew would reach that.
- **Timing under load.** The fork's `sweep_fault_handler_fields_concurrent_touches` gave up after
  50 sweeps without a collision; beside the 13 s enumeration, on battery, its toucher thread
  managed one pass in those 50. It now sweeps for up to 10 s.
- **Keyboard** (`crates/limina-input/src/ledger.rs`). The held-key bookkeeping lived in
  `InputState` beside AppKit types and sent each edge as it decided it. It is now `KeyLedger`,
  which returns the edges; `InputState` sends them, and the monitor and the tap call the same two
  entry points. The walk drives it from a model keyboard through macOS's Modifier Keys setting
  (none, Control↔Command, Option↔Command), with keys that change unseen, focus losses, the end of a
  capture and normalization flips, into a model guest with evdev's semantics. 32 operations to
  depth four under each setting: 3,145,728 sequences, 24 s. It checks that the guest holds exactly
  what the ledger believes; that a flush leaves nothing held; that healed modifiers are the ones
  the user holds (by position under normalization, from a table written apart from `KeyRemap`);
  that Caps Lock matches the LED; and that an event's own press lands with every other held
  modifier already down. Checked only after each step, it passed. Checked edge by edge it found one
  ordering bug: a Caps Lock tap went out before the heal, so after Control was released unseen,
  Caps Lock reached the guest as Control+Caps Lock. Fixed in the ledger and in `handle()`, which
  had synced Caps Lock ahead of everything.
- **Grab policy** (`crates/limina/src/window/grab_policy.rs`). Two walks through `GrabState`
  against the rules the module states. The free path runs 15 operations to depth five (759,375
  sequences): key status, Space, menus, screenshot sessions, macOS in front, buttons, pointer
  positions, clicks, the dwell, and the grab's transitions. Every sample must grab exactly when the
  model does, and the explicit-release latch must match it. The edge presses run 11 operations to
  depth six (1,771,561 sequences) at the Light hold. A release must come exactly when the model's
  charge earns it. Both run in under a second and found nothing. What the booked property named,
  no grab held with no owner window, is decided by two one-line predicates (`must_drop_grab`,
  `key_loss_releases`) that the window tick and the tap compose through AppKit. Enumerating that
  composition needs a seam in front of the tick and the tap, which is left for when it earns it.
- **Clipboard greeting** (`crates/limina/src/control.rs`, `loom_model`). The poller offering a host
  copy raced `admit` greeting an agent that had just connected, with a stand-in pasteboard server
  that another app writes to as AppKit does (`clearContents`, which bumps the change count, then
  `setString`). After each run the poller is ticked until it has nothing left to offer and every
  guest requests the newest offer it holds, last, as late as a guest can: the host must serve it,
  with what is on the pasteboard. Three models, 0.12 s together; the first loom build of `limina`
  took 45 s. They found three ways a guest kept an older clipboard until the next host copy, each
  shown by the model and fixed in turn: a peer registered only after its greeting missed a copy
  offered in between; a greeting re-minted a serial for the text already on offer, which retired
  the serial the other peers held, so their requests were refused and nothing re-offered it; and a
  greeting read the pasteboard before taking its serial, so a poller's newer copy was outranked by
  older text. Seams: `PasteboardServer` in front of NSPasteboard, a `Peer` generic over its write
  half, and the registry's send, admit and host-copy steps as free functions.
- **vCPU band** (libkrun fork, `src/vmm/src/macos/vcpu_sched.rs`, `loom_model`). The band sampler
  and a vCPU's own `BandGuard` both want the band back from a thread that computed flat out, and
  the sampler then hands it over again. The stand-in kernel records which hold each party took
  back. One model, 0.03 s. Against the code as it was it ended with the thread banded and the flag
  everyone reads saying otherwise: the guard, preempted across the sampler's two moves, cleared
  the flag after the new hold's kernel call. Nothing takes such a thread back out, and the shipped
  default (`rt+dyn#1`) runs this on vCPU 0. A move is now claimed by a compare-and-swap on one word
  that also counts the holds, and finished by whoever claimed it. The word publishes each hold's
  baseline with release/acquire, where the flag and the baseline had been `Relaxed`. That half is
  argued, not witnessed: weakening the publish to `Relaxed` survived the model, which never showed
  the guard a stale baseline behind a new hold.
- **Power watch** (libkrun fork, `src/devices/src/legacy/power_watch.rs`, `loom_model`). The waiter
  protocol `generation()` documents (read the generation, then the state, and wait past the
  generation only if the state is not yet what it wants) raced against a transition. loom's timed
  wait never times out, so a transition slept through would be a deadlock. It found nothing.
- **vCPU event handshake** (libkrun fork, `src/vmm/src/macos/vstate.rs`). Out of loom's reach: the
  park sites wait in crossbeam `select`. Reading them for a model found one site out of line.
  `wait_for_event`'s event arm handles `Pause` and treats every other event as an IRQ wake, so a
  `Snapshot` arriving there would be consumed and its reply channel dropped, and `snapshot_vcpus`
  would fail at once. A blocked crossbeam select takes the event arm every time when the event is
  sent before the kick (2,000 of 2,000 in a standalone probe), and `snapshot_vcpus` sends the event
  first. It is latent: HVF parks an idle vCPU inside `hv_vcpu_run` and does not hand over the WFI
  trap (as `vcpu_sched.rs` notes), and a probe in `wait_for_event` recorded no block at all during
  `l1_snapshot_save_writes_file_and_exits_126`, both vCPUs taking the `Snapshot` at the top of
  their run loop. Every other park site services `Snapshot`. Not fixed yet.
- **Host-sleep bracket** (`crates/limina-vmm/src/power.rs`). Not loom's: `willSleep`, `didWake` and
  each step of the post-wake watch run whole under the bracket's one lock, so the threads reduce
  to a sequence of atomic steps interleaved with the guest's transitions. That is an enumeration
  of `HostSleepState` against guest transitions, which needs a seam for the watch's clock; the
  primitive the watch waits on is the power watch above.
- **Frame handoff** (`crates/limina/src/window/present.rs`). Not loom's either: the reader and the
  surface-port receiver each change one store under one lock, and the hazards are the orders in
  which the control lines and the Mach messages arrive, for a consumer on the AppKit main thread.
  An enumeration of arrival orders fits, once the store is generic over the surface type.
- **The sweep.** Seventy-three entries, each caught by the assertion, test or model written for it,
  after two holes found by the sweep itself were closed (the coalescer's merge check and the head
  CRC). One more hole was in an entry and not in a model: registering a peer after its greeting
  survived the first clipboard model, which cannot reach the gap once serials are reused, and is
  caught by the three-thread one. The baseline publish above was tried and retired. Each entry
  since phase 3 was run as it was added; the full sweep was not re-run end to end.

The rule for Kani, sharpened from virglrs's: it needs code that neither allocates nor does
arithmetic on time, on any path the harness can reach, taken or not. Find the cost by bisecting
which inputs are symbolic, then prove the property on an allocation-free core, or stub the
expensive call with an over-approximation the property does not depend on.

## Phases

1. **Infrastructure and the cheap proofs.** Landed: the toolchains, the root `fuzz/` workspace,
   the sabotage sweep, `cargo xtask check`, the control-plane header proofs, the balloon policy
   proofs, the balloon coalescer proof on the libkrun fork, and the `control_frame` and
   `vdagent_stream` fuzz targets, and the CTAP2 request parser split out of the FIDO ceremony
   with its two fuzz targets. Nothing booked for phase 1 is still owed.
2. **xHCI.** Landed: the `xhci_guest` fuzz target with its seeds and depth oracle, and the two
   fixes it led to. The hardware-free seam was already there (`UsbDeviceModel`, with the mock and
   HID gadgets). Also landed: the Kani proof of the ring walker's step, and the enumeration of
   slot command sequences. Nothing booked for phase 2 is still owed.
3. **Snapshots and memory.** Landed: the snapshot fuzz targets, seeded from a real snapshot, and
   the three reader fixes; the `released_ram` seam, enumeration and loom model, and the three
   bookkeeping fixes; the coalescer enumeration; `cargo xtask check loom`. Left, and why: the GPU
   payload decoder (see *Measured so far*).
4. **Input and grab enumeration**, then the Tier C loom models. Landed: the keyboard ledger and
   its walk, with the Caps Lock ordering fix; the grab policy's free-path and edge-press walks;
   the clipboard greeting model and its three fixes; the vCPU band model and its fix; the power
   watch model. Left, and why: the tick and tap's composition of the ownership predicates (it
   needs a seam in front of both); the host-sleep bracket and the frame handoff, which are
   enumeration work, not loom's; and the `Snapshot` a vCPU's WFx wait would drop, found by
   reading and not yet fixed (see *Measured so far*).

Every phase ends with this document updated: what each tool now covers, with measured time and
memory, and what was tried and did not fit, with the numbers.

## Out of reach

- Anything that crosses into HVF (`hv_vm_*`, `hv_vcpu_*`), AppKit, IOSurface, Mach ports or
  Metal. The seams in front of them can be checked, the calls themselves cannot.
- The guest side's syscall-heavy code (`guest/limina-init`), other than its pure parsers.
- Ordering properties that rest on `SeqCst` or on atomics shared with guest memory, which loom
  cannot model faithfully.
