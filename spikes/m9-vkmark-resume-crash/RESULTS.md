# vkmark crash on resume (dogfood, 2026-07-20) — evidence + first analysis

User report: vkmark was running (mid-benchmark) during a suspend/resume on dogfood-mac/dogfood-guest;
on resume it crashed. Firefox was also open. **Worker survived; GNOME session survived;
Firefox survived (its venus context died but Firefox tolerates context loss); vkmark
aborted** — the per-context poison containment (virgl 0026-0028/0031) worked as designed.

Build under test: the 14:41 dogfood deploy — **debug profile**, but current master
(includes libkrun 0086 gen-2 fence rebase and all M9.3/M9.4 patches).

## Timeline (guest clock, -03)

- 14:44 guest boots (fresh session post-deploy). User opens Firefox (venus ctx 6),
  vkmark (venus ctx 14, mid-benchmark), then suspends.
- 15:07:48 `PM: suspend entry (s2idle)` — the M9 bracket. Snapshot taken, worker exits 126.
- 15:08:28 user relaunches; auto-resume arms (`snapshot.bin` → consumed at 15:08).
- 15:08:45 **restore replay** runs in the fresh worker. The storm begins (host log):
  - `vkr: failed to look up object <N> of type <T>` — hundreds of distinct objects,
    for ctx 6 (firefox, objects ~307k range) and ctx 14 (vkmark, objects 438-457,
    types 6/9/15/19).
  - each failed lookup → `vkr: cs decoder: ring FATAL set at vkr_cs_decoder_lookup_object:342`
    — the 0026 poison (per-context, no worker crash).
- 15:08:46 replay summary (the load-bearing line):
  `gpu restore: replay complete with 3607 dropped wire entries (stale references);
  drops by class [transient,create,recording,noted,free,pool-reset,ring-create,
  ring-destroy,ring-stream] = [0, 2, 3539, 0, 66, 0, 0, 0, 0]`
- 15:08:46 guest resumes (`PM: resume devices took 2.011s`). Immediately:
  - kernel: `virtio_gpu ... response 0x1203 (command 0x102)` (RESOURCE_UNREF →
    ERR_INVALID_RESOURCE_ID) — guest unref of a resource the host no longer knows.
  - vkmark's first real submit hits its FATAL ring → guest venus
    `vn_ring_submit_locked` `abort()` (by design when the ring is lost) → SIGABRT
    mid `vkCreateBuffer` in `CubeScene::setup`. Coredump 9088 preserved in the guest.
- 15:09:40 ctx 4 also trips `ring FATAL at vkr_dispatch_vkWaitRingSeqnoMESA:399`.

## First-pass read (NOT yet root-caused — premises to verify)

The 3539 dropped **recording**-class wire entries dominate. Both busy contexts had large
in-flight command streams at snapshot time (vkmark mid-benchmark is the stress case).
The failed lookups happen DURING replay: dropped entries would have created objects that
later entries reference → cascading lookup failures → decoder poisons the ring → the app
dies at its first post-resume submit. Two candidate shapes, in decreasing suspicion:

1. **Drop cascade**: the initial drops are legitimate (stale refs), but each drop orphans
   downstream entries in the same ring, and the classifier counts the whole cascade as
   "recording" drops. Root cause would be whatever invalidated the FIRST reference —
   possibly the same journal/wire-epoch merge area as the gen-2 fence bug (0086 is in
   this build; this was likely a gen-1 resume — verify).
2. **Journal gap**: creates for objects 438+ (vkmark) genuinely missing from the adopted
   journal (only 2 "create" drops though), i.e. capture raced the live benchmark's
   creation stream.

Next steps (when picked up):
- Read the 0076 classifier: what exactly lands in "recording" vs "noted"; whether a
  cascade is expected to poison the ring or skip cleanly.
- Repro locally: the gen2-repro script pattern with vkmark running at suspend time
  ($CLAUDE_JOB_DIR pattern from 2026-07-20; or add a vkmark leg to venus_session_preserved).
- Decide the product goal: apps with live rings surviving resume (full fix) vs contained
  per-app death (current behavior — arguably acceptable for a benchmark, not for Firefox).
- Note: guest-side abort is upstream venus behavior (`vn_ring_submit_locked` aborts on
  lost ring) — containment beyond this needs the guest driver to fail queue-submits
  gracefully instead (VK_ERROR_DEVICE_LOST), an upstreamable mesa change.

## Files

- `supervisor-resume-session.log.gz` — dogfood-mac supervisor log, complete resume session
  (15:08:28→15:23; includes the full lookup-error storm + replay summary + DEBUG lines).
- `vkmark-coredump-info.txt` — guest `coredumpctl info 9088` (backtrace: abort ←
  vn_ring_submit_locked ← vn_buffer_create ← CubeScene::setup).
- `guest-kernel-resume-window.txt` — guest kernel journal 15:07-15:11 (s2idle cycle +
  the 0x1203 RESOURCE_UNREF error).
- The coredump itself persists on dogfood-guest (`coredumpctl dump 9088`) — survives poweroff.
