<!-- ledger-check: dep=mlx base=dc5ea6e98e97a97ffb1abce98fe7deb20ca5f39c -->
# mlx — patch-audit ledger

2 patches on `UPSTREAM_BASE` = `ml-explore/mlx` `main` `dc5ea6e`. Schema: `README.md`. Rows are
keyed by SUBJECT. Post-audit rows, not researched against upstream's trackers yet.
Rebased 2026-11-21 onto `dc5ea6e` from `b6642e6`; both patches replayed unchanged, and the old tip
is tag `pre-rebase-2026-11-20` (`4cb085ba`).

Our fork is `github.com/liminavm/mlx`, a GitHub fork: `limina` = the default branch = upstream
`main` + the rows below. The local `third_party/mlx` is a clone of it pinned by
`third_party/manifest.toml` (`cargo xtask vendor` materializes it). A series for upstream comes
from `git format-patch <base>..limina`.

| ord | subject | files | diag | need | checked | issue | mr | sec | fold | tier | disp | notes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0001 | metal: build the backend on Linux over limina's GPU transport | `CMakeLists.txt`, `mlx/backend/metal/{CMakeLists.txt,allocator.cpp,device_info.cpp}` |  | needed | `dc5ea6e` (read 2026-11-21: `CMakeLists.txt:77` still turns `MLX_BUILD_METAL` off on any system but macOS and iOS) | | | no | standalone | guest | carry | everything is under `MLX_METAL_LIMINA`; upstream has no Metal-over-a-transport backend to take it into |
| 0002 | metal: return a pair, not a tuple, from compute_bluestein_constants | `mlx/backend/metal/fft.cpp` |  | needed | `dc5ea6e` (read 2026-11-21: `fft.cpp:369` still returns `std::make_tuple(w_k, w_q)` from a function declared `std::pair<array, array>` at :323) | | | no | standalone | guest | upstream | a libstdc++ portability fix that stands alone: libc++ converts a tuple to the returned pair, libstdc++ does not |
