# imago — patch-audit ledger

1 patch; `UPSTREAM_BASE` = upstream `main` `c7bb559` (release 0.2.5). Schema + protocol: `README.md`.
Rows are keyed by SUBJECT; ordinals are informational and drift on re-export.

Upstream is **gitlab.com/hreitz/imago** (Hanna Reitz; from the crates.io `repository` field —
NOT GitHub). Our fork is `github.com/liminavm/imago`, a plain pushed repo (upstream is GitLab, so
no GitHub-native fork is possible): `limina` = the default branch = upstream `main` + the row
below; `main` = upstream `main`. Every rev ever pinned is kept reachable by a `limina/<date>` tag.
The local `third_party/imago` is a clone of the fork pinned by `third_party/manifest.toml`
(`cargo xtask vendor` materializes it); regenerate a series for upstream submission with
`git format-patch main..limina`.

| ord | subject | files | diag | need | checked | issue | mr | sec | fold | tier | disp | notes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0001 | limina: retry transient EFAULT on the guest-buffer data path | `src/file.rs` |  | needed | c7bb559 (0.2.5) | n/a | n/a | no | standalone | host | carry | the EFAULT window is created by limina's own ledger-settling sweep (the worker briefly revokes its mapping of guest RAM); a generic retry is not something upstream needs |

## Findings

### Retired: discard truncate

Upstream fixed it as `d48b76e` ("file: keep image size after tail discard", in 0.2.5): a discard
reaching EOF no longer shrinks a raw image, so libkrun's virtio-blk keeps the capacity it read at
open (`spikes/m10-disk-durability/`). Our patch for it is gone.

### Retired: vm-memory pin

Upstream's `vm-memory = ">=0.16, <0.19"` is deliberately wide, and libkrun now uses 0.18, inside
that range, so no pin is needed. **A lockfile can still split the graph:** a lock that recorded
imago against an older vm-memory keeps it, and then krun-devices' `VolatileSlice` fails imago's
`ImagoAsRef` bound. Unify it with `cargo update vm-memory@<old> --precise <libkrun's>` in the
workspace that builds the graph (limina's root).
