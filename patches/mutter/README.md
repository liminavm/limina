# mutter carried patches (limina guest) — NO LONGER SHIPPED

**Since 2026-07-11 the guest support package carries NO patched mutter.** The GNOME tier
of the clipboard bridge is the `clipboard@limina` gnome-shell extension
(`guest/gnome-shell-extension/`), which scripts `Meta.Selection` inside the compositor —
same quiet behavior as the ext-data-control patch, with nothing for a distro mutter
update to displace (a stock `dnf upgrade` silently replaced our `50.1-1.limina` with
stock 50.3 on the dogfood guest, 2026-07-11: rpm release `.limina` loses to any stock
release bump, and mutter was deliberately not versionlocked). The optional builders
(`scripts/build-mutter-rpm.sh`, `scripts/provision/f44/build-mutter-rpm.sh`) remain for
experiments and apply whatever `*.patch` files sit here.

The checkout is `third_party/mutter` (gitignored); these diffs are the durable copy —
re-export with `git -C third_party/mutter diff -- <files>` after editing.

## Kept (not shipped)

- `0003-ext-data-control-v1.patch` — implement the standardized ext-data-control-v1
  protocol (wayland-protocols staging; KWin/wlroots parity) as a second front-end onto
  MetaSelection, modeled on the primary-selection device minus focus gating. This let
  `limina-agent-session` manage the clipboard as a plain focusless Wayland client; the
  agent still carries that backend (it lights up on any compositor shipping the
  protocol — KDE, wlroots, or a mutter built with this patch). NOT an upstream
  candidate: GNOME explicitly rejects data-control (mutter#524, privacy stance — any
  client may snoop/own the selection). Kept for ext-data-control experiments on GNOME.

## Retired (`retired/`) — root causes were in other layers

Both were validated unnecessary on completely stock mutter 50.3 on the current
KK stack (2026-07-11, dogfood-guest: no #32 decay, zero cogl stencil warnings, X11 apps
fine). They remain **upstream MR candidates** as robustness fixes.

- `retired/0001-32-stencil-clip-degrade-fix.patch` — the #32 *mitigation*: degrade
  multi-rect clipped redraws to their bounding extents when the framebuffer has no
  stencil buffer (stencil clipping silently no-ops there), plus a zero-init fix for
  `cogl_framebuffer_get_stencil_bits` reading uninitialized stack when the driver
  bits-query is unsupported. The actual bug was HOST-side (MoltenVK's DS +
  HOST_TRANSFER `memoryTypeBits=0` contradiction — see
  `spikes/venus-draw-probe/RESULTS.md` "#32 DEEP BUG ROOT-CAUSED", fixed 2026-06-10);
  stock-mutter validation the same day already concluded the patch was dormant
  defense-in-depth, and it kept being carried by inertia. The zero-init half is a
  genuine correctness fix upstream.
- `retired/0002-x11-survive-frames-client-launch-failure.patch` —
  `meta_frame_launch_client` can return NULL (binary missing) and
  `meta_x11_display_init_frames_client` passed it straight to
  `g_subprocess_wait_async` → compositor SIGSEGV on the first X11 client. The missing
  binary was OUR OWN artifact (2026-06-11 in-guest dev build with the default meson
  prefix → empty `/usr/local/libexec`); Fedora SRPM builds and stock mutter ship
  `/usr/libexec/mutter-x11-frames` and never hit it. Upstream one-liner candidate.
