<!-- ledger-check: dep=mlx base=b6642e604843776705b57b1eaecb6f57e519b793 -->
# mlx — patch-audit ledger

2 patches on `UPSTREAM_BASE` = `ml-explore/mlx` `main` `b6642e6`. Schema: `README.md`. Rows are
keyed by SUBJECT. Post-audit rows, not researched against upstream's trackers yet.

Our fork is `github.com/liminavm/mlx`, a GitHub fork: `limina` = the default branch = upstream
`main` + the rows below. The local `third_party/mlx` is a clone of it pinned by
`third_party/manifest.toml` (`cargo xtask vendor` materializes it). A series for upstream comes
from `git format-patch <base>..limina`.

| ord | subject | files | diag | need | checked | issue | mr | sec | fold | tier | disp | notes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0001 | metal: build the backend on Linux over limina's GPU transport | `CMakeLists.txt`, `mlx/backend/metal/{CMakeLists.txt,allocator.cpp,device_info.cpp}` |  | needed | b6642e6 | | | no | standalone | guest | carry | everything is under `MLX_METAL_LIMINA`; upstream has no Metal-over-a-transport backend to take it into |
| 0002 | metal: return a pair, not a tuple, from compute_bluestein_constants | `mlx/backend/metal/fft.cpp` |  | needed | b6642e6 | | | no | standalone | guest | upstream | a libstdc++ portability fix that stands alone: libc++ converts a tuple to the returned pair, libstdc++ does not |
