# Does a VM's guest pipeline cache survive a reboot?

virglrs signs the pipeline-cache data it hands a guest and passes a guest's `pInitialData` to the
driver only when the tag verifies under the same key. `two-boot.sh` boots one clone of an enhanced
image twice, runs the same zink-on-venus glmark2 in each boot, and counts virglrs's per-cache verdict
lines in each boot's worker log.

Measured 2026-10-09: `Fedora-Workstation-44.enhanced.raw`, virglrs `a22cb8d`, libkrun `8d9dfe0c`,
host KK `19e3ad7ebaa`, 4 vCPUs / 4 GiB.

| run | boot 2 initial data | boot 2 glmark2 |
|---|---|---|
| `--gpu-cache-key FILE` (same file both boots) | 3 accepted, 0 ignored | 2135 |
| no key (`NOKEY=1`, control) | 0 accepted, 3 ignored (`not signed by this key`) | 2150 |

Boot 1 passes no initial data in either run, because the image's caches predate the signed format.
No `[virglrs] refused:` lines in any boot. The key file is made at mode 0600, 32 bytes.

**Read the verdict line, not the score.** The glmark2 score does not separate the two runs
(2135 vs 2150; boot 1 alone ranged from 1777 to 1997). Why a warm guest cache does not show up in fps
was not measured. The `accepted` / `ignored` lines are the oracle.
