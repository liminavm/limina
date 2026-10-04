# KWin with GL compositing never finishes starting on the enhanced stack

On `Fedora-Workstation-44.enhanced.kde.raw` with `KWIN_COMPOSE=Q` removed, Plasma 6.7 (KWin 6.7.5,
DRM backend, GL compositing) on the coexist venus/virgl stack never finishes starting. The screen
stays black with a cursor (`evidence-2026-10-03/last-frame.png`). It reproduced across a reboot.

## What is known

- **plasmashell and kded6 time out behind KWin.** Their systemd units restart every ~40 s. KWin's
  D-Bus does not answer: `busctl get-property org.kde.KWin /KWin ...` times out.
- **KWin is not stuck on one lost fence.** Every sample of its main thread is in
  `virtio_gpu_wait_ioctl -> dma_resv_wait_timeout`, but the guest's virtio-gpu fence counter keeps
  advancing at about 15,000 fences/s (`virtio-gpu-irq-fence`: 689721, 1400621, 2137887 at 48 s
  intervals), and the host has nothing parked; the stale-fence watcher never fired. KWin is
  looping submit-and-wait.
- **On the host, the moment KWin's first frame goes out** (22:44:46 in
  `evidence-2026-10-03/worker.log`) there is one `present fence injection failed (resource 5):
  rutabaga component failed with error -22; presenting now` (rolled back, by design) and a 186 ms
  control-queue drain. Then the log is quiet.

## Evidence

`evidence-2026-10-03/`:
- `worker.log`: host worker and supervisor, host paths scrubbed;
- `guest-journal.txt`: full guest journal of the hung boot;
- `guest-state.txt`: KWin's kernel stack, the fence counter, DRM clients and state, package
  versions;
- `last-frame.png`.
