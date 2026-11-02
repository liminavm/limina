# patches/mesa — tombstone

Nothing here is a build input. limina's Mesa changes are commits on the `liminavm/mesa` fork, on
two branches:

- **`limina-kk`** — the host KosmicKrisp + zink-on-KK build, pinned by `[kosmickrisp]` in
  `third_party/manifest.toml`; audit status in `docs/upstreaming/ledger/kosmickrisp.md`.
- **`metal-native-context`** — the guest vendor drivers, `limina-kk` plus guest-only commits,
  pinned by `[kosmickrisp-guest]`; audit status in `docs/upstreaming/ledger/kosmickrisp-guest.md`.

The guest keeps its distro's Mesa; there is no separate guest-Mesa branch or exported patch series.
Patches that once lived here and are still worth sending upstream have rows in those ledgers; the
diffs themselves are in this directory's `git log`.

Do not add patches here: commit on the branch the change targets.
