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
from "must merely register"; `--audio-only` renders and registers nothing, standing in for the
worker; and `--audio-sibling` registers here while *spawning* an `--audio-only` child, which is
limina's actual supervisor/worker shape. The child is spawned rather than launched from a second
shell on purpose: a separately launched process would be the terminal's responsibility, not ours,
and would not model limina.

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
| 5 | claim (wire + publish `.playing`) **while a rival is actively playing** | → the rival | tile stays the rival's |
| 6 | same, after the rival has been **paused** | → the rival (it resumed) | tile stays the rival's |
| 7 | claim **while rendering a tone in this process**, rival paused | → **us** | both listed |
| 8 | claim while a spawned **child** renders the tone, rival paused | → **us** | both listed |
| 9 | claim with **no audio anywhere**, rival paused — the control | → **us** | both listed |
| 10 | rival **resumed and actively playing**, then the key | → the rival | — |
| 11 | a real VM window **focused** (Media at soft grab), then the key | → the guest only | — |
| 11b | same VM, window **unfocused** | → us, `togglePlayPause` | — |

The logical arms above do not map one-to-one onto the raw logs, because arm 3 was found by
accident before it was isolated. `arm1-bundled-noaudio.log` is arm 1. `arm2-retire.log` is a first
pass at arm 2 whose two late `togglePlayPause` lines were in fact the arm-3 phenomenon — no rival
happened to be open at the time — which is what prompted isolating it. `arm3-release.log` carries
arms 2, 3 and 4 in sequence, each under a stated rival condition: the arm-2 press (rival open)
left no line in it, the arm-3 press (rival quit) is the `togglePlayPause` at 195.975 s, and the
arm-4 press after `unwire` again left none. `arm5-reclaim.log` carries arms 5 and 6, and its
value is entirely in what it does *not* contain: the probe starts fully released, claims at
1104 s while a rival plays, and records no command at all across both subsequent presses.

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

**6. Claiming does not take the session from a rival — and a paused rival still holds it.**
Registering handlers and publishing `.playing` while another app is mid-playback changes nothing:
the tile keeps naming the rival, and the key goes to the rival (arm 5). Pausing that rival does
*not* hand the session over either — the next press resumed it, and the probe again received
nothing (arm 6). The arbitration is sticky to the app that most recently *rendered audio*, and it
survives a pause; being a registered, `.playing`, tile-eligible player is not enough to displace
it.

This is the behavior the design wants: the VM is a participant in macOS's arbitration, not a
privileged one. A player that seized the session merely by existing would take the keys from the
app the user is actually listening to. The reach that follows — the guest gets the keys when the
VM is what most recently played, a still-running host player that played more recently keeps them
— is correct on both halves. Arms 1–4 read so cleanly because they ran on an empty field.

**7. The ranking key is the most recent claim, not the most recent audio.** Arms 7 and 8
appeared to show that rendering audio wins the session — first from the registering process,
then from a spawned child, which would have decided whether registration had to move into the
worker. The control killed both: arm 9 claimed against the same paused rival while rendering
**nothing at all**, and won just the same. What separates arms 7-9 from arms 5-6 is not audio but
timing — 5 and 6 claimed *while the rival was still playing* and lost; 7-9 claimed *after* it had
gone idle and won. Announcing yourself — wiring the handlers and publishing
`playbackState = .playing`, which every arm did together and which this spike therefore cannot
decompose — is itself the event macOS ranks on, and a pause does not reorder anything.

The design consequence is that the open question dissolves rather than being answered: limina
needs no audio of its own, no token tone, and no registration in the worker. The supervisor
registers, the worker's stream state supplies the trigger, and the topology in §4 of the design
doc stands unchanged.

**8. An actively playing rival still takes it straight back, and delivery is exclusive.** With the
probe holding the session, resuming the rival and pressing the key paused the *rival*, and the
probe logged nothing (arm 10). One press, one recipient — no evidence of a command being fanned
out to every registered player.

**9. The Control Center menu lists several sessions at once; the key still has one target.**
Through arms 7-9 both the rival and the probe were listed. Being on the menu is eligibility;
ranking is a separate question, and only the ranked-first player receives the key.

## The rule, as measured

> macOS ranks registered players by **when each last announced itself as playing**. Publishing
> `.playing` counts as such an announcement and moves you to the front — unless a rival is
> *actively playing right now*, which no amount of registering displaces. Pausing does not
> reorder. Rendering audio is not part of it.

Two implementation rules fall out, both in §5 and §7 of the design doc:

- **Announce on the transition, never on a timer.** Because re-publishing `.playing` re-claims the
  front of the ranking, a limina that periodically re-asserted its state would steal the media
  keys from whatever the user is actually listening to, repeatedly and invisibly. Publish when the
  guest's stream opens and when the state genuinely changes; never refresh for its own sake.
- **The claim is cheap and correct at stream-open.** The guest opening its audio device is exactly
  the moment the VM has a claim to make, and the rule then gives it the keys unless a host player
  is mid-playback — in which case the host player should keep them.

**10. Consuming the event at limina's tap does suppress the remote command.** Measured against a
real booted VM rather than reasoned about, because a wrong answer here means the guest receives
every press twice and play/pause cancels itself out. With the limina window **focused** — the
`Media` bucket's soft grab consuming the `NX_SYSDEFINED` event — the guest reacted (its player
raised a "nothing to play" notice) and the probe, which held the media session throughout, logged
nothing at all. The same press with the window **unfocused** reached the probe as
`togglePlayPause`. One press, one recipient, and the tap decides which.

So `Media` can stay at `GrabMode::Full` safely: under an explicit capture the tap takes the key
directly and macOS never mints a command for the session path to double-deliver.

## Still open

Nothing. Both premises and both follow-up questions are measured; implementation is unblocked.
