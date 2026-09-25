# In-crate checkers: Kani, loom, Miri, cargo-fuzz and the sabotage sweep

Status: **phases 1 and 2 landed**; phases 3–4 proposed (see *Measured so far*) · Scope: limina's own crates, the guest workspace, and
the libkrun fork's `limina` branch · Model: virglrs (`third_party/virglrs/docs/design.md`, *Owed,
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
  its own, so a handshake built on either stays with the std tests.
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

loom candidates are `crates/limina/src/control.rs` (connect, reconnect, the shutdown
handshake); `crates/limina-vmm/src/power.rs`, `quiesce.rs` and `wake.rs` (quiescing vCPUs is
what snapshots stand on); the frame handoff in `crates/limina/src/window/present.rs`; and
`third_party/libkrun/src/vmm/src/macos/vcpu_sched.rs`. Each needs its sync primitives behind a
`cfg(loom)` shim first. Anything that crosses HVF, AppKit or guest memory stays with the HVF suite.

## Layout and commands

- **limina's own checkers.** Kani proofs live in `#[cfg(kani)]` modules beside the code, loom
  models in `cfg(all(test, loom))` modules beside the code, and enumeration in `every_sequence`
  test modules. Fuzz targets live in a root `fuzz/` directory that is its own workspace, excluded
  from the main one like `xtask/`. Each crate with a proof declares `cfg(kani)` (and later
  `cfg(loom)`) under `unexpected_cfgs` in its own `[lints.rust]`, as virglrs does in its
  `Cargo.toml`; the workspace has no shared lint table to put it in.
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
  remains the source of truth, following the one-command convention (`xtask/src/main.rs`). Loom
  and Miri get their subcommands when their first model or sweep lands. Kani runs on its own
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
- **The sweep.** Twenty-eight entries, twenty-eight caught, each by the assertion or test written
  for it.

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
3. **Snapshots and memory.** Fuzz the snapshot decoders with a round trip; enumerate the
   coalescer and `released_ram` against their models; add the loom model of `released_ram`.
4. **Input and grab enumeration**, then the Tier C loom models.

Every phase ends with this document updated: what each tool now covers, with measured time and
memory, and what was tried and did not fit, with the numbers.

## Out of reach

- Anything that crosses into HVF (`hv_vm_*`, `hv_vcpu_*`), AppKit, IOSurface, Mach ports or
  Metal. The seams in front of them can be checked, the calls themselves cannot.
- The guest side's syscall-heavy code (`guest/limina-init`), other than its pure parsers.
- Ordering properties that rest on `SeqCst` or on atomics shared with guest memory, which loom
  cannot model faithfully.
