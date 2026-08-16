# mutter — patch-audit ledger

> **SERIES RETIRED 2026-08-03.** limina no longer patches mutter — we are writing a
> drop-in gnome-shell/mutter replacement, so the patch series (`patches/mutter/`) and its
> experimental build scripts were removed. This file is kept as the historical audit record;
> the one active patch (0003 ext-data-control) and the two retired robustness fixes it once
> carried are all moot now that mutter is not a dependency we ship. No upstreaming owed.

1 patches; `UPSTREAM_BASE` `floating — see the series README`. Schema + protocol: `README.md`.
Rows are keyed by SUBJECT; ordinals are informational and drift on re-export.

| ord | subject | files | diag | need | checked | issue | mr | sec | fold | tier | disp | notes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0003 | ext data control v1 | `src/meson.build`, `src/wayland/meta-wayland-data-control.c`, `src/wayland/meta-wayland-data-control.h` +2 |  | needed | 9e9f2bf8abb5 (main tip, 2026-08-03) | https://gitlab.gnome.org/GNOME/mutter/-/work_items/3941 | n/a | no | standalone | guest-enhanced | carry | upstream REJECTED the protocol by name (2026-08-03 re-check: still no implementation, no MR); not shipped since 2026-07-11 — experimental carry only; extension bridge stays the GNOME path |

## Findings

### 0003 — ext-data-control-v1

Upstream mutter main (`9e9f2bf8abb5`, 2026-08-03) has **no** ext-data-control support:
no `data-control` sources under `src/wayland/`, no hits in `src/meson.build`, zero MRs
matching "ext-data-control"/"data-control" (open or closed). And this is not an
oversight — it is an explicit, named rejection: work item **#3941** ("Support
ext-data-control (wayland protocol)", filed after the protocol's wayland-protocols
ratification) was closed 2025-02-28 by swick with "consensus among mutter developers
that we do not want to add those kinds of protocols", after jadahl's rationale ("no plan
to support unmanaged clipboard access via Wayland … a sandbox hole"; portals are the
sanctioned path). #4333 was duped onto it 2025-09-20 — community pressure continues, the
position hasn't moved. This matches and refreshes the series README's mutter#524 citation
(#524 is the older wlr-data-control rejection, closed 2019).

Consequences for limina: **do not submit** — the wlroots pedigree doesn't help; mutter
rejected this exact protocol post-ratification. No native GNOME path is coming, which is
why GNOME's clipboard landed on stock `spice-vdagent` (#37, 2026-08-15) rather than on
anything of ours; the agent's ext-data-control backend still lights up on KDE/wlroots
guests. GNOME's sanctioned alternative is the portal/RemoteDesktop clipboard — already
limina's opt-in RD rung (`limina-clipboard-rd-optin`). The patch stays as an unshipped
experiment (`scripts/build-mutter-rpm.sh` applies whatever sits here); `need` = needed
in the sense that no upstream equivalent exists to retire it, `disp` = carry.

The `guest-enhanced` rubric is moot in its usual form — the patch is NOT delivered
(guest mutter is stock since 2026-07-11): (a) capability = focusless clipboard
management for `limina-agent-session`; (b) stock guest: extension bridge provides the
same behavior, boots/works unaffected; (c) host-side alternative n/a (clipboard needs a
guest peer); (d) exit strategy: none via mutter — exit is the extension bridge (already
taken) or the portal path, not upstreaming.

Note: `retired/0001` (cogl stencil zero-init half) and `retired/0002` (x11 frames NULL
deref) remain genuine upstream one-liner candidates per the series README — out of scope
for this row but worth their own tracker items when mutter upstreaming is scheduled.

