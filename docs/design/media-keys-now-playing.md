# Media keys as a media session, not a keyboard bucket

Status: **shipped.** The policy is `crates/limina/src/window/media_policy.rs` (pure, unit-tested),
the MediaPlayer side `crates/limina/src/window/media_session.rs`, the guest signal
`crates/limina-vmm/src/audio_state.rs` over libkrun's snd `set_pcm_state_callback`, and the bucket
table `crates/limina-input/src/auxkey.rs`. Measurements: `spikes/now-playing-media-keys/RESULTS.md`.

## 1. What the bucket rule gets wrong

Aux keys (the special/media top row) reach us as `NX_SYSDEFINED` events and are routed by a
per-bucket ownership table (`crates/limina-input/src/auxkey.rs`, `AuxBucket::min_grab`). The
`Media` bucket — play/pause, next, prev, ff, rw — goes to the guest at `GrabMode::Soft` or
better, i.e. **whenever the VM window is focused**, and the host keeps it otherwise.

That rule answers the wrong question. "Who is focused" is a *keyboard* question; "who should
receive a transport command" is a *media session* question, and macOS already arbitrates it
system-wide. Two consequences follow from asking the wrong one:

- **The common case is backwards.** Music playing in the guest while the user works in a host
  app is exactly when the transport keys are wanted, and it is exactly when the bucket rule
  denies them: the VM is not focused, so the host keeps the key. Conversely, a focused VM
  swallows play/pause even when the thing actually playing is Spotify on the host.
- **The Control Center transport is unreachable.** The menu-bar Now Playing widget, the
  headphone gestures and Siri all speak the media-session protocol, not the keyboard. A design
  built on key routing can never reach them.

## 2. The design

The VM **announces itself to macOS as a player** for as long as the guest holds its audio device
open, and translates the remote commands macOS routes back to it into the evdev media keys the
guest already understands. macOS keeps ownership of the arbitration; limina supplies a player and
a translation.

```
guest opens virtio-snd stream 0  ──▶  worker  ──▶  supervisor  ──▶  MPNowPlayingInfoCenter
                                                                    MPRemoteCommandCenter
   guest evdev KEY_PLAYPAUSE     ◀──  worker  ◀──  supervisor  ◀──  remote command callback
```

Deliberately **no guest state** is involved in this first version: no MPRIS, no title, no artist,
no artwork, no position. The session is announced with a title-only info dict naming the VM, and
commands are delivered as media keys the guest's own desktop already routes to its own player.
Everything richer is an enhanced-tier addition on top of exactly this shape (§8) and needs a guest
agent; this version works on a **stock guest with no limina components at all**, which is what
makes it the right floor.

### 2.1 It works despite the process split — measured, not assumed

The registering process renders no audio. libkrun's CoreAudio sink lives in the **worker**
(`third_party/libkrun/src/devices/src/virtio/snd/audio_macos.rs`), while `MPNowPlayingInfoCenter`
must live in the **AppKit** process. If macOS tied media-session eligibility to the registering
process actually producing audio, this design would need a fork.

It does not, and audio turns out to play no part in the arbitration at all. A silent
`.accessory` process holds the session and receives media keys system-wide; a title-only info
dict — no duration, elapsed time, rate or artwork — is enough to be routed to; and a process
rendering nothing wins the session from an idle rival exactly as well as one rendering a tone
(measured 2026-08-27 and 2026-08-29, macOS 26.5; `spikes/now-playing-media-keys/RESULTS.md`).
The probe was given in-process, child-process and no-audio arms precisely to find a fork here.
There is none: **the supervisor registers, the worker supplies only the stream signal.**

### 2.2 The VM becomes a participant in macOS's rules

The rule macOS arbitrates by, as measured:

> Registered players rank by **when each last announced itself as playing** — wiring the
> handlers and publishing `.playing`, which the measurements only ever did together. Announcing
> moves you to the front — unless a rival is *actively
> playing right now*, which no amount of registering displaces. Pausing does not reorder, and
> rendering audio is not part of it.

So claiming while a host player is mid-playback does not displace it (arms 5-6), and that player
takes the keys straight back the moment it resumes (arm 10) — while claiming once it has fallen
idle does win, even against an app that is still open and was playing seconds ago (arms 7-9).

**That is the design working, not a limitation.** The goal is to make the VM a peer subject to the
same rules as any native player, not a privileged one — and "the transport keys drive whatever
last played" is the rule. A limina that seized the media session merely by having a VM open would
be a worse citizen than iTunes: it would take the keys away from the app the user was actually
listening to, on the strength of a guest that is playing nothing.

The reach of the feature follows from that, and is worth stating plainly: the guest opening its
audio stream announces the VM, which then holds the media keys until some host player starts
playing again. A host app actually playing keeps them. Both halves are correct.

What this buys that key routing **cannot buy at any grab level**: the Control Center transport
buttons, Siri, and headset gestures — a double-tap on the earbuds pausing whatever is playing in
the guest. None of those ever enter the keyboard event stream, so no aux-key bucket policy can
reach them however permissive it is made. They are only available to a registered media session,
and they are the part of this that is genuinely new rather than better-arbitrated.

## 3. The signal: the guest's PCM stream lifetime

"Is the VM a player right now" is answered by the virtio-snd PCM lifecycle for playback stream 0,
which libkrun already handles explicitly — `VIRTIO_SND_R_PCM_START` / `STOP` / `RELEASE` at
`third_party/libkrun/src/devices/src/virtio/snd/device.rs:306-320`. No sampling, no RMS
thresholding, no heuristics: a discrete event to hang a callback on.

Be honest about what that event means: **the guest's audio device is active**, not *music is
playing*. PipeWire keeps a sink node open across a pause and only suspends it after an idle
timeout, and a system beep opens it for a moment. So the state limina publishes is "the VM has
audio open", mapped to `playbackState = .playing`, and `STOP`/`RELEASE` mean *stop being a
player* rather than "paused" — once the guest has let go of the audio device we genuinely have
nothing to control.

The retire side wants **hysteresis** (a few seconds' hold-off) so a track gap or a PipeWire idle
suspend does not hand the session away and make the user's next press miss the guest.

## 4. The seam

Mechanism in the fork, policy in limina, as everywhere else:

- **libkrun** gains a stream-state callback on the snd device — the smallest possible upstreamable
  addition, no policy.
- **The worker** forwards it to the supervisor as a line on the existing worker→supervisor control
  socketpair, alongside `surface` / `frame` / `scanoutgone` (parsed in
  `crates/limina/src/window/present.rs:645`, `spawn_reader`). One new verb, no new channel.
- **The supervisor** owns registration, retire hysteresis, per-VM arbitration (§7) and the
  translation back to `InputState::tap_aux_key` (`crates/limina/src/window/input.rs:1487`) — which
  is already exactly "emit this evdev media key, down then up", and is what the aux tap calls
  today. **Nothing new in the input stack**; only the source of the event changes.

## 5. Registration must be symmetric with the stream

This is the part the measurements changed, and getting it wrong is worse than not shipping the
feature.

Clearing `nowPlayingInfo` and setting `playbackState = .stopped` removes the Control Center tile
and yields to any *rival* player — which reads like a clean handback, and is not one. With **no
rival open, an app whose remote-command handlers are still registered stays the fallback target**
and goes on receiving `togglePlayPause` indefinitely. Only `removeTarget(nil)` plus
`isEnabled = false` actually steps aside, after which macOS falls back to its own default player.

So a limina that registered its handlers once at launch and only managed the info dict would
**silently swallow three keys on any Mac with no other player running**, forwarding them to a
guest that is not playing anything — invisible, permanent, and very hard to attribute. The rule:

> Wire the commands when the guest opens the stream; **unwire** them when it closes it. The
> handler registration, not the info dict, is what holds the keys.

**Announce on the transition, never on a timer.** Because announcing re-claims the front of the
ranking (§2.2), a limina that periodically re-asserted its state — a keepalive, a
defensive refresh, a re-publish on every window event — would repeatedly and invisibly steal the
media keys from whatever the user is actually listening to. Publish when the guest's stream opens
and when the state genuinely changes; never refresh for its own sake.

`playbackState` must also be maintained rather than set once: the Control Center button renders
from it, so a stale `.playing` leaves the widget offering "pause" forever and sending `pause`
repeatedly.

## 6. Command mapping

fn+F8 arrives as `togglePlayPause`. Control Center's own buttons send the **discrete** `pause`
and `nextTrack`. So wiring all of play / pause / toggle is load-bearing, not defensive — a
toggle-only registration leaves the widget's buttons dead.

| remote command | guest key |
|---|---|
| `togglePlayPause`, `play`, `pause` | `KEY_PLAYPAUSE` (164) |
| `nextTrackCommand` | `KEY_NEXTSONG` (163) |
| `previousTrackCommand` | `KEY_PREVIOUSSONG` (165) |

The guest only understands a toggle, so `play` and `pause` collapse onto it. A desync between
macOS's idea of our state and the guest's is possible but self-correcting, because our published
state is derived from the actual stream rather than from the commands we received.

Every other command (`changePlaybackPosition`, seek, skip, shuffle, repeat, like, rating, …) is
explicitly `isEnabled = false`. That is the documented way to say "this player cannot do that";
leaving them at their defaults advertises capabilities no evdev key can service.

## 7. What stays, and what changes, in the bucket table

- **`Media` leaves the soft grab.** With the window merely focused, media keys are no longer
  intercepted; they go to macOS, which routes them to whoever owns the session — us, when the
  guest is playing. Net behavior in the common case is unchanged, and the unfocused case starts
  working. Measured on a booted guest, with the session as the only variable: focused and
  uncaptured, the key reached the guest while we held the session and reached **nothing** once
  the session had retired. That null control is what distinguishes routing *through* macOS from
  an interception that merely looks like it — a still-soft-grabbed key would have reached the
  guest in both.
- **`Media` stays at `GrabMode::Full`.** Under an explicit capture the VM owns the keyboard
  outright, so the tap eats the `NX_SYSDEFINED` event and forwards the key directly, and macOS
  mints no remote command for the session path to double-deliver. Measured against a booted VM,
  not assumed: focused, the guest reacted and the session holder logged nothing; unfocused, the
  same press arrived as `togglePlayPause` (arm 11). A wrong answer here would have meant every
  press reaching the guest twice.
- **`Volume` is untouched.** Volume is not a remote command, our audio leaves through CoreAudio,
  and the existing full-grab-only rule is already right for the reasons in `auxkey.rs`'s header.
- **`Brightness` and `Other` are untouched.**

**Multiple VMs.** `MPNowPlayingInfoCenter.default()` is process-wide, so one limina.app can
present exactly one session however many VMs it runs. Most-recent-stream-start wins, and the
command goes to *that* VM — deliberately **not** the focused one, since the whole point is that
focus is the wrong question.

## 8. Two-tier behavior

- **Stock guest (no limina components).** Fully works. The guest's own desktop already binds
  `virtio_snd` and already handles `KEY_PLAYPAUSE`; nothing guest-side is required. This is the
  version described above.
- **Enhanced guest (later).** With `limina-agent` present, the same shape carries real metadata
  and real commands: MPRIS title/artist/album/artwork/position published into the info dict, and
  MPRemoteCommand callbacks delivered as MPRIS method calls to the exact player the agent picked,
  instead of synthesized keys. That removes the toggle collapse of §6 and makes the scrubber and
  artwork work. It is strictly additive: the key-synthesis path remains the floor for guests
  without the agent, and the arbitration, the stream signal and the retire discipline are
  unchanged.

Note the app icon shown in the Now Playing widget is always limina's — that is the *app's* icon
and cannot be overridden per VM. Artwork (enhanced tier) is the only per-content lever.

## 9. Open before implementation

Nothing. Both premises (§2.1) and both follow-up questions — ranking (§2.2) and tap suppression
(§7) — are measured in `spikes/now-playing-media-keys/RESULTS.md`.
