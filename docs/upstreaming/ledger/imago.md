# imago — patch-audit ledger

2 patches; `UPSTREAM_BASE` `floating — see the series README`. Schema + protocol: `README.md`.
Rows are keyed by SUBJECT; ordinals are informational and drift on re-export.

**2026-08-03 — PUSHED: github.com/liminavm/imago is live** (public; `limina` = the default
branch = tip `042b1de` + the 2 rows below; `main` = upstream tip; pin tag `limina/2026-08-03`).
The local `third_party/imago` is a clone of the fork pinned by `third_party/manifest.toml`
(`cargo xtask vendor` materializes it); `patches/imago/` + `apply-imago-patch.sh` are retired —
regenerate a series for upstream submission with `git format-patch main..limina`.

**2026-08-03 — migrated to the fork model (the pilot).** `third_party/imago` is now a real
clone with upstream history; the `limina` branch = upstream tip `042b1de67dfa` + the two
rows below cherry-picked (0001 auto-merged over the maybe-async drift exactly as predicted;
0002 re-resolved onto the real-repo Cargo.toml layout, still `>=0.17, <0.18`). Destination
repo: `github.com/liminavm/imago` (plain pushed repo — upstream is GitLab, no GitHub-native
fork possible). This ledger stays the status reference and must be kept current as series
migrate and rebase.

| ord | subject | files | diag | need | checked | issue | mr | sec | fold | tier | disp | notes |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0001 | limina: discard preserves the backing file size (no truncate-to-EOF) | `src/file.rs` |  | needed | 042b1de67dfa (tip, 2026-08-03) | none-yet | none-yet | no | standalone | host | upstream-after-cleanup | tip still truncates at EOF; no upstream issue/MR exists — file the issue (see Findings); strip limina-branded comments first |
| 0002 | limina: pin vm-memory to ^0.17 to unify with the libkrun stack | `Cargo.toml` |  | needed | 042b1de67dfa (tip, 2026-08-03) | n/a | n/a | no | standalone | host | carry | upstream's `>=0.16, <0.19` is deliberate downstream flexibility; our pin is build-graph policy; retires when the libkrun stack's vm-memory converges with whatever the resolver would pick |

## Findings

Upstream is **gitlab.com/hreitz/imago** (Hanna Reitz; from the crates.io `repository`
field — NOT GitHub). Base = pristine crates.io `imago-0.2.2` (2026-02-06); upstream tip
at check time = `042b1de67dfa` (2026-07-22, post-0.2.4). GitLab.com API + `/-/raw/` work
with plain curl, no Anubis.

### 0001 — discard truncate

`try_discard_by_truncate` at tip (`src/file.rs:836-856`) is byte-for-byte the behavior we
patched out: a discard range reaching EOF is satisfied by `file.set_len(offset)`. Nothing
in the 0.2.2→tip `src/file.rs` history touches it (the deltas are discard/zero *alignment*
queries, 2026-06-24, and the maybe-async refactor `f05e604a0a0a`, 2026-07-16 — the latter
may cause trivial context drift on rebase). No issue or MR mentions truncate; work item
#16 ("DISCARD with raw macOS devices", closed) is about raw *block devices*, unrelated.

Upstreamability: for the **Raw** format the file's logical size *is* the virtual disk
size, so truncate-on-discard semantically shrinks the disk — qemu's file-posix never
truncates on discard for exactly this reason, and Hanna is a qemu block maintainer, so
the argument should land. Punch-hole reclaims the same blocks; the only thing truncate
buys is a shorter logical length, which is precisely the bug for capacity-derived-from-
file-size consumers (libkrun virtio-blk: capacity read at open → mkfs.ext4 tail discard →
"bad geometry" unmountable fs after reboot; `spikes/m10-disk-durability/`). **Action
item:** file an issue at gitlab.com/hreitz/imago with that repro, offering both shapes —
(a) drop the truncate path entirely (our patch, minus limina-branded comments), or
(b) make it conditional (only when the storage isn't capacity-bearing, e.g. a qcow2
container file where tail truncation is legit space reclaim) — and let upstream pick.
Until then the carry is tiny and rebases trivially.

### 0002 — vm-memory pin

Tip `Cargo.toml:20` still `vm-memory = { version = ">=0.16, <0.19", … }` — deliberately
wide so downstreams on 0.16/0.17/0.18 can all unify. The break is on our side: cargo may
resolve imago's vm-memory to a different semver-major than libkrun's 0.17, and then
krun-devices' `VolatileSlice` fails imago's `ImagoAsRef` bound. Not an upstream defect;
nothing to file. Mechanical carry; drop it when the stack (or upstream's floor) converges
on one vm-memory major.

