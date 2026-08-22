# pointer-units-oracle: measure the wire, not the theory

Two questions, one instrument, per `docs/hardening-backlog.md` §"the captured pointer is
unusable with two guest monitors":

1. **Units.** Does the guest spread the absolute-tablet range over its monitors' *logical*
   extents or their *pixel* extents? The arc answered this three ways; the final code holds
   logical on a source reading while the only discriminating measurement (961d369, scales
   1.25/2.0) observed pixel. A rig where every panel shares one scale cannot tell the models
   apart — the measurement below **requires mixed guest scales**.
2. **The seam bug.** What do we actually send (ABS x/y, REL dx/dy), and where does the guest
   actually put the cursor, at the moment of a teleport / unreachable-band event?

## The instrument

- **Host half:** `LIMINA_POINTER_WIRE_TRACE=1` makes the supervisor print `[WIRE]` lines to
  stderr for every event written to the absolute device (`dev=abs`, raw evdev type/code/value)
  and every edge-pressure burst on the relative device (`dev=rel dx= dy=`), stamped with
  wallclock µs. Emission choke points: `input.rs::send_ptr` (all ABS — both pointer modes
  drive the same device) and `input.rs::send_edge_overflow` (the only writer of REL X/Y).
  Wheel traffic is not traced; the oracle doesn't need it.
- **Guest half:** `guest-cursor-poll.sh`, run as root in the guest, samples the DRM atomic
  state (`/sys/kernel/debug/dri/*/state`) at ~50 Hz and prints one line per cursor-plane
  observation with wallclock µs. The guest clock is host-anchored (PL031 + TimeSync), so the
  two logs correlate directly.
- **Join:** `correlate.py` answers both questions from the two logs: it finds, per CRTC, the
  ABS-range interval that lands on it and compares the seam fraction against the logical and
  pixel predictions; and it merges the two logs into one timeline for reading a teleport
  event for the seam bug.

## Protocol

1. Guest: two monitors at **different scales** (e.g. 2.0 on the panel, 1.25 or 1.0 on the
   external; fractional needs mutter's `scale-monitor-framebuffer` experimental key).
2. Boot windowed with the trace on, stderr to a file:
   `LIMINA_POINTER_WIRE_TRACE=1 cargo xtask run --disk <clone> 2> wire.log`
3. Copy the poller in and start it (`bash -s` over ssh+sudo yields no output — run from a file):
   `scp -P <port> guest-cursor-poll.sh claude@127.0.0.1:/tmp/poll.sh`, then
   `ssh -p <port> claude@127.0.0.1 'sudo bash /tmp/poll.sh > /tmp/guest-cursor.log'`.
   Snapshot the guest's logical layout DURING the fullscreen phase (this is what makes the run
   interpretable): `gdbus call --session --dest org.gnome.Mutter.DisplayConfig --object-path
   /org/gnome/Mutter/DisplayConfig --method org.gnome.Mutter.DisplayConfig.GetCurrentState`.
4. Go fullscreen across both panels (fullscreen auto-captures — that is the point: the
   captured path is the one under test). A human sweeps the pointer slowly left-to-right
   across the full width, twice, then reproduces the symptom (the left band, the seam
   flick). Synthetic events are a poor oracle here — the sweep must be hardware motion.
5. `python3 correlate.py wire.log guest.log --logical <w1xh1,w2xh2> --pixel <W1xH1,W2xH2>`

Record numbers in RESULTS.md before writing any conclusion.
