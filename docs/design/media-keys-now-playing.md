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

## 3. Two signals: the stream's lifetime, and what it carries

**Whether the VM is a player** is answered by the virtio-snd PCM lifecycle for playback stream 0,
which libkrun handles explicitly — `VIRTIO_SND_R_PCM_START` / `STOP` / `RELEASE` in
`third_party/libkrun/src/devices/src/virtio/snd/device.rs`. A discrete event to hang a callback
on. Be honest about what it means: **the guest's audio device is active**, not *music is playing*.
PipeWire keeps a sink node open across a pause and only suspends it after an idle timeout, and a
system beep opens it for a moment. So `STOP`/`RELEASE` mean *stop being a player* rather than
"paused" — once the guest has let go of the device we genuinely have nothing to control. The
retire side wants **hysteresis** (a few seconds' hold-off) so a track gap or an idle suspend does
not hand the session away and make the user's next press miss the guest.

**Whether the guest is playing right now** cannot come from that, and the gap is not academic —
it is the whole of §6's conditional mapping. Measured on a Fedora 44 guest, 2026-09-01, pausing a
YouTube video with the mouse:

| after the click | what the host sees |
|---|---|
| 0 – 3 s | buffers still arriving at 96/s, every sample bit-exact zero |
| ~3 s | buffers stop arriving at all |
| ~5 s | `STOP` — the first thing the lifecycle says |

Five seconds is far too late: a headset comes off within one, macOS routes a `pause`, and a belief
still reading "playing" turns it into a toggle that *starts* the paused video. The first phase is
the fix. **Silence is the fast signal, and it is exact, not a threshold**: in 242 s of real
playback not one buffer was bit-exact zero except at a track change (8 buffers, 340 ms) and the
sink's initial prefill — while the quietest audible passages measured (peak 0.0026 of full scale)
still contained no zero-filled buffer at all. Encoded content carries dither; a pause carries
nothing. So the detector is "N consecutive zero frames", not an RMS threshold, and **500 ms** is
the chosen N — 1.5× the worst run content produced, a tenth of the lifecycle's latency.

It is a *latency bridge, not a new ground truth*: silence flips the belief early, the lifecycle
event confirms it moments later, and any single non-zero frame flips it back at once. Being wrong
costs one swallowed `pause`.

## 4. The seam

Mechanism in the fork, policy in limina, as everywhere else:

- **libkrun** gains two callbacks on the snd device — a stream-state one, and an audibility one
  that reports the edges between sound and silence. Both are mechanism: the device counts zero
  frames and reports the crossing, but *how long a silence is a pause* is the embedder's
  threshold, passed in. The smallest upstreamable addition, no policy.
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
| `togglePlayPause` | `KEY_PLAYPAUSE` (164), always |
| `play` | `KEY_PLAYPAUSE`, only if we believe the guest is paused |
| `pause` | `KEY_PLAYPAUSE`, only if we believe the guest is playing |
| `nextTrackCommand` | `KEY_NEXTSONG` (163) |
| `previousTrackCommand` | `KEY_PREVIOUSSONG` (165) |

The guest only understands a *toggle*, so the discrete commands have to become one — and that
makes them conditional. macOS sends a bare `pause` for things that are not a user pressing pause:
headphones coming off, a call arriving. Forwarding it as a toggle *starts* a paused video, which
is the opposite of what was asked. So the discrete commands are gated on a belief about what the
guest is playing, and a command that asks for the state we are already in is swallowed —
answering `Success`, since the command did reach the right player and a failure status would only
invite macOS to route it elsewhere.

**The belief, and its limits.** No component of ours runs in the guest, so playback state is
inferred from three things we can see: whether the stream's buffers carry sound or bit-exact
silence (§3, the fast one), the stream starting and stopping, and the toggles we have sent
ourselves. Both delivery paths feed the last — the routed commands here, and the hard grab, which
hands the media bucket straight to the guest without passing through the handlers (§7). It can
still drift, because audibility answers "is sound coming out of the guest", not "is *that* player
playing": a video muted in the guest reads as paused, and a paused video reads as playing while
some other app makes noise, since PipeWire mixes everything into one stream before we see it.
That is why `togglePlayPause` is the one unconditional arm — the physical key means "the other
one" whatever we believe, so it is always both useful and the way back to a correct belief.
Closing the gap properly wants a guest agent reporting per-player state, which is the same lever
§10 wants for MPRIS.

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
- **`Media` requires `GrabMode::Hard`, and the full grab splits in two to say so.** A capture the
  fullscreen policy took on the user's behalf is not the same claim as Cmd-Ctrl-G: one is a
  request for a big window, the other is the user saying this VM owns the keyboard. Only the
  second takes the keys that control what the user is listening to — which they may well have
  started in a host app before going fullscreen. So `GrabMode` gains `Auto` between `Soft` and
  `Hard`, and the tap tells them apart with `captured && !grab_state().holding()`.

  Under `Hard` the tap eats the `NX_SYSDEFINED` event and forwards the key directly, and macOS
  mints no remote command for the session path to double-deliver — measured against a booted VM,
  not assumed (arm 11): captured, the guest reacted and the session holder logged nothing;
  uncaptured, the same press arrived as `togglePlayPause`. A wrong answer there would have meant
  every press reaching the guest twice.

  That direct path is also the only delivery we can *guarantee*. A session-routed key arrives
  while macOS still agrees we hold the session, and a rival started afterwards takes it; under
  the hard grab the key is ours unconditionally. Reserving that for the explicit gesture is the
  point of the split.
- **`Volume` moves to `GrabMode::Hard` with it**, for the same reason and one of its own: the
  documented whisper-trap (guest pinned to 100% under a host volume capped at grab time, with no
  in-grab way to fix it) is a genuinely surprising state to land in, and confining it to the
  explicit grab keeps it from ambushing anyone who merely went fullscreen. Volume is not a remote
  command, so below `Hard` it simply stays with the host device and its HUD.
- **Nothing sits at `Auto`.** That is the honest reading of the table rather than an oversight:
  the tier exists so the two captures can mean different things, and so per-key config has
  somewhere to put a key that a big window *should* claim.

Measured on a booted guest, with the mechanism read out of the worker log rather than inferred
from the outcome — the `media:` line is what distinguishes the two delivery paths:

| capture | session | fn+F8 reaches | what the log shows |
|---|---|---|---|
| `Auto` (fullscreen) | held | the guest | `taken — the guest gained the screen`, then `macOS routed a command to us` |
| `Auto` | retired | nothing | no `media:` line, no reaction |
| `Hard` (Cmd-Ctrl-G) | retired | the guest | no `media:` line — forwarded directly, macOS never saw the press |

The first row is the load-bearing one: same outcome as before the split, opposite mechanism. An
outcome-only check cannot tell those apart, which is why the round-trip is logged.
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
