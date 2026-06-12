# mutter carried patches (limina guest)

The checkout is `third_party/mutter` (gitignored, upstream tag 49.5 = commit `658f672`);
these diffs are the durable copy — re-export with `git -C third_party/mutter diff -- <files>`
after editing. Built **in the guest** and installed by
`spikes/venus-draw-probe/install-mutter-fix.sh` (read its header: the libmutter-17 install
path trap is encoded there).

- `0001-32-stencil-clip-degrade-fix.patch` — the #32 fix: cogl stencil-clip degrade when
  the framebuffer has no stencil buffer (gnome-shell-on-venus/KK renders without one) +
  meta-stage-impl clipped-redraw degrade. Upstream MR candidate.
- `0002-x11-survive-frames-client-launch-failure.patch` — `meta_frame_launch_client` can
  return NULL (binary missing), and `meta_x11_display_init_frames_client` passed it
  straight to `g_subprocess_wait_async` → the whole compositor SIGSEGVs the moment the
  first X11 client triggers Xwayland. Found 2026-06-11: our guest build was configured
  with the default meson prefix, so MUTTER_LIBEXECDIR pointed at /usr/local/libexec
  (empty) and the fallback "./src/frames/mutter-x11-frames" (builddir-relative) also
  failed → instant session crash on any X11 app. Guard + configure the build with
  `--prefix=/usr --libexecdir=/usr/libexec` so the (same-version) Fedora-stock
  /usr/libexec/mutter-x11-frames is used. Upstream one-liner candidate.
