# now-playing-media-keys — can limina take macOS's media session without producing audio?

**Question.** limina's media keys are currently routed by the aux-key bucket policy
(`crates/limina-input/src/auxkey.rs`): the `Media` bucket goes to the guest at `GrabMode::Soft`
or better, i.e. whenever the VM window is focused, and the host keeps them otherwise. The
proposed design instead hands the decision to macOS's own media-session arbitration — the VM
announces itself as a player while the guest holds the virtio-snd stream open, and
`MPRemoteCommandCenter` callbacks are translated back into evdev media keys for the guest. Two
premises had to be measured before building any of it:

1. Does a process with **no audio output of its own** get Now Playing status? The CoreAudio sink
   lives in libkrun in the **worker** process (`third_party/libkrun/src/devices/src/virtio/snd/audio_macos.rs`),
   while MediaPlayer must live in the **AppKit** process. If eligibility were tied to the
   registering process actually rendering audio, the design would need a fork.
2. Is a **title-only** info dict — no duration, elapsed time, playback rate or artwork, which is
   all limina knows without a guest agent — enough to be routed to?

**Answers (measured 2026-08-27, macOS 26.5, M1 Max).** Yes to both, and the topology fork does
not exist. But retiring the session turned out to be subtler than publishing it, and that is the
finding that shapes the design.

## The probe

`nowplaying.swift` is a minimal `.accessory` AppKit app that registers `MPRemoteCommandCenter`
handlers, publishes a title-only `MPNowPlayingInfoCenter` dict, logs every remote command it
receives, and takes state commands on stdin (`playing` / `paused` / `stopped` / `clear` /
`publish` / `disable` / `unwire` / `rewire`) so one long-lived process can be walked through
every arm while a human presses the key at each step.

```sh
sh spikes/now-playing-media-keys/build.sh          # bare binary + NowPlayingProbe.app
spikes/now-playing-media-keys/NowPlayingProbe.app/Contents/MacOS/nowplaying
```

`--audio` adds a near-silent in-process `AVAudioEngine` output, to separate "must render audio"
from "must merely register". It was never needed — arm 1 passed without it.

Two things the probe is built to avoid confounding:

- It is **`.accessory`**, so it is never frontmost. A delivered command therefore proves
  system-wide media-session routing, not mere key-window focus.
- It is **bundled with a `CFBundleIdentifier`**, because a bare binary without one is a known
  confounder for Now Playing; a refusal in a bare arm must not be read as "macOS refuses a silent
  process". Whether the bare binary would also work is **untested** — limina always ships bundled,
  so the question does not need an answer.

## Results

| arm | state | fn+F8 | Control Center |
|---|---|---|---|
| 1 | registered, title-only info, `.playing`, **no audio at all** | → `togglePlayPause` | tile shows "Fedora 44 (limina)" |
| 2 | `.stopped` + `nowPlayingInfo = nil`, handlers **still registered**, a rival player open | → the rival app | tile gone |
| 3 | same, but **no rival player open** | → **still us**, `togglePlayPause` | tile gone |
| 4 | handlers `removeTarget` + `isEnabled = false` | → macOS default player | tile gone |

The logical arms above do not map one-to-one onto the raw logs, because arm 3 was found by
accident before it was isolated. `arm1-bundled-noaudio.log` is arm 1. `arm2-retire.log` is a first
pass at arm 2 whose two late `togglePlayPause` lines were in fact the arm-3 phenomenon — no rival
happened to be open at the time — which is what prompted isolating it. `arm3-release.log` carries
arms 2, 3 and 4 in sequence, each under a stated rival condition: the arm-2 press (rival open)
left no line in it, the arm-3 press (rival quit) is the `togglePlayPause` at 195.975 s, and the
arm-4 press after `unwire` again left none.

**1. Eligibility does not require audio from the registering process.** A process rendering
nothing at all held the session and received media keys system-wide. The worker/UI split is a
non-issue: the AppKit process can own MediaPlayer while libkrun owns CoreAudio, and the only
thing that has to cross between them is the stream-state signal.

**2. A title-only info dict is enough.** No duration, elapsed time, rate or artwork. The tile
renders and the keys route. Everything richer is a later, additive enhancement.

**3. The physical key and the widget send different commands.** fn+F8 arrives as
`togglePlayPause`; Control Center's own transport buttons send the discrete `pause` and
`nextTrack` (as `MPRemoteCommandEvent` / `MPSkipTrackCommandEvent`). Wiring all of
play/pause/toggle is therefore load-bearing, not defensive — a toggle-only registration would
leave the widget's buttons dead.

**4. The widget's button reflects the published `playbackState`, and goes stale if it is not
maintained.** The probe never flipped to `.paused`, so the tile kept offering "pause" and sent
`pause` twice in a row. Whatever limina publishes has to track the real stream state or the
button lies about what it will do.

**5. Releasing the tile is NOT releasing the keys.** This is the one that matters.
`playbackState = .stopped` plus `nowPlayingInfo = nil` removes the Control Center tile and yields
to any *rival* player — but with **no rival open, an app whose remote-command handlers are still
registered remains the fallback target** and goes on receiving `togglePlayPause` indefinitely
(arm 3, reproduced twice). Only tearing the handlers down — `removeTarget(nil)` and
`isEnabled = false` — actually steps out of the way, after which macOS falls back to launching its
default player (arm 4).

The consequence for the design: **retire means unwiring the commands, not just clearing the
info.** A limina that registers handlers once and only manages the info dict would silently
swallow the media keys of any Mac with no other player running, forwarding them to a guest that
is not playing anything — an invisible, permanent theft of three keys. Registration must be
symmetric with the guest's virtio-snd stream lifetime, not set up once at launch.

## Still open

Whether re-registering while a rival is **actively playing** takes the session from it — i.e.
whether the guest opening its audio stream makes the VM the media-key target immediately, or only
once the rival stops. Arm 2 shows a rival wins while we are released; it does not show what
happens when we claim during active rival playback. Measure before shipping, because it decides
whether "start music in the VM, press play/pause" works on the first press.
