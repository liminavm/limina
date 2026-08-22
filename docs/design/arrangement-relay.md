# The host→guest arrangement relay

The guest is told *where* each connector sits, so a guest with no saved monitor
configuration comes up arranged like the host. Shipped 2026-08-19 (M15 wave 1 part 4);
this doc records the geometry model and the design for closing its one residual, the
fractional-scale metric.

## The pipeline

`crates/limina/src/window/arrangement.rs` computes a guest-desktop position per connector
from the host panel arrangement → `DisplayControl.pos` on the display control channel →
libkrun carries it as the per-scanout rect `GET_DISPLAY_INFO` always had room for
(`DisplayInfo::position`) → the enhanced kernel exposes it as the DRM `suggested X`/
`suggested Y` connector properties plus `hotplug_mode_update`, which compositors gate the
offsets on (mutter returns early without it, with no journal warning). Stock guests ignore
the rect and keep their linear default.

Three ordering/timing rules are load-bearing (each cost a rig cycle):

1. **The whole suggested set precedes the connect's hotplug.** mutter evaluates the
   suggestions at the instant the hotplug lands; in-place moves go out before the connect
   they accompany, and a slot coming up carries its position on the connect itself.
2. **One config-change event carries every update that can share it**, and the device
   holds further events until the guest acks — the ack is a read-then-clear, and an event
   raised between the guest's read and its ack would otherwise be wiped (libkrun
   `f03e94c`; regression: `l1_multidisplay::l1_b_back_to_back_updates_survive_the_ack_race`).
3. **The device default is never pushed**: a slot never told a position already sits at
   (0, 0), and pushing it costs a config-change cycle at desktop arrival in every
   single-display session.

## The geometry model: structure from points, metric from prediction

mutter consumes the suggestions **verbatim as logical coordinates** and validates the set
hard (`verify_suggested_monitors_config`): any overlap, or any monitor whose rect does not
share an exact edge with a neighbour, rejects the whole set → linear default. So positions
cannot be a unit conversion of host frames. The host arrangement supplies the *structure*
(who touches whom, on which side, with what cross-axis offset, detected in point space
with a small tolerance); the *metric* is rebuilt exactly in predicted guest-logical units
by walking the adjacency graph.

The prediction: a panel's guest-logical size is its usable frame in host **points** —
hidpi drives the guest at device pixels and mutter picks the backing factor as scale;
non-hidpi drives it at points and mutter picks 1. Either way logical ≈ points.

**The residual:** where mutter picks a scale the prediction cannot see — a fractional
scale on a fixed-mode connector (the rig's 2560×1440 BenQ gets 1.25 → logical 2048, not
2560) — the metric is wrong for that panel. The set still validates when the mispredicted
panel sits on the *far side* of every shared edge (the rig's geometry, which is why the
rig passed); mirror the arrangement and a neighbour's position derives from the wrong
width, the set fails mutter's adjacency check, and the guest falls back to linear —
degraded, never wrong, but not arranged.

## Closing the residual: the agent's logical rects are the metric

The exact fix is feedback, not better prediction: `limina-agent-session` already reports
the compositor's own logical rects (`zxdg_output_v1`, the pointer-mapping relay). Those
rects ARE the metric mutter will validate against — no policy replication, any compositor.

- Every emission runs the reported sizes through `arrangement::correct_metric` first
  (`arrangement::reported_logical_sizes` maps connector → slot): a reported size replaces the
  prediction unconditionally, and a changed result re-pushes the positions that moved as
  in-place moves through the ordinary `positions_sent` diff. The re-push updates the
  connector props; when it rearranges the guest is mutter's call — see the precedence
  rules below.
- **Sizes only, never positions.** Taking positions from the report would fight a user
  who rearranged the guest in its own settings; a rearrangement changes reported
  positions, not sizes, and must not trigger a re-push. Convergence follows: sizes are
  stable within a session, so a report changes the emission at most once per scale
  change.
- The stock tier keeps the prediction as its floor — correct at whole-number scales,
  linear fallback otherwise, exactly today's behavior.

**When a corrected push takes effect (verified end to end on the seated guest, mutter
50.1):** the KMS layer sees every in-place change — the connector-state diff compares
`suggested_x`/`suggested_y` themselves, returns `META_KMS_RESOURCE_CHANGE_FULL` and
reloads (`meta-kms-connector.c:1006` at 50.1), and our kernel updates the property values
from every `GET_DISPLAY_INFO` response (`virtgpu_vq.c:840`) — but mutter ≥ 50's
`ensure_configured` applies the first **existing** config (current, then previous) whose
monitors are all still connected *before* it ever consults `create_suggested`
(`meta-monitor-manager.c`, the `existing_configs` loop; the apply logs "Applied current
based monitor configuration" under `MUTTER_DEBUG=backend`). Consequences:

- A live session never rearranges from a bare re-push, and a disconnect + corrected
  reconnect of the same monitor set re-applies the *previous* config the same way.
- Suggested offsets are consulted exactly when no complete existing config is available:
  at session start, and at the first appearance of a monitor set in a session. The
  common flow still lands corrected: the agent reports the primary's logical size long
  before a second display joins, so that display's first connect already carries
  corrected positions and the suggested set applies.
- The residual that remains: a set that already fell back to linear (mispredicted at its
  first appearance) stays linear for the rest of the session; the corrected offsets are
  live in the connector props and heal at the next seat or first-appearance event. If an
  in-session rearrangement is ever required, the lever is the agent calling
  `org.gnome.Mutter.DisplayConfig.ApplyMonitorsConfig`, not the props.
- L2 acceptance: `crates/limina-test/tests/l2_arrangement.rs` pins the linear fallback,
  both no-rearrange cases, and the seat-time apply in one boot.

**Side-discovery, recorded so it is not rediscovered as a bug:** with
`hotplug_mode_update` present, `should_use_stored_config` returns FALSE outside init —
monitors.xml is only consulted at session start. Within a session, mutter's
existing-config precedence protects a user rearrangement of the *same* monitor set (it IS
the current config); a set the session has not seen yet takes our suggested offsets
(qxl/vmwgfx semantics — "the host dictates layout" — right for limina, where guest
monitors mirror physical panels). Suggested applies are TEMPORARY and never persisted.

Rejected alternatives, so they are not re-derived:

- **Author the EDID physical size to steer mutter's scale choice** toward the predicted
  logical size: lies to the guest about DPI, which is user-visible in font sizing —
  identity stays honest.
- **Replicate mutter's `meta_monitor_calculate_mode_scale` host-side**: the inputs are
  ours (we author the mode and physical size), but the heuristic is version- and
  config-dependent (fractional-scaling experimental keys), and it only ever upgrades the
  guess — the feedback loop is exact and covers every compositor that reports.
