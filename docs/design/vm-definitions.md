# Design — VM definitions & persistence  ·  PHASE 1 SHIPPED

> **Status: Phase 1 + the control-center UI SHIPPED 2026-07-02** (`crates/limina/src/vmlib/`
> + `src/center/`; L2 guard `crates/limina-test/tests/vmdef.rs`): `.liminavm` bundles with
> `vm.toml` v1, `limina create/start/ls/stop/rm`, flag overrides, run-lock + pidfile, and the
> AppKit control center (bare `limina` / double-clicked limina.app) with import (clonefile),
> configure (cpus/memory/ssh port), start/stop/reset/delete. Two as-built deltas from the
> sketch below: the running-VM flock lives on a stable `run/lock` sentinel, NOT `vm.toml`
> (atomic tmp+rename saves would swap the inode out from under the lock), and `vm.toml`
> gained a `[boot] firmware` key (headless starts need one). The MAC is allocated + stored
> but not yet plumbed to the worker. Snapshots (M9), bridged modes, and `limina snapshot/
> restore` remain future phases.
>
> Originally PROPOSED 2026-07-01 because three in-flight designs
> silently presume per-VM persistent identity that did not exist: the `Network`
> abstraction wants stable per-VM MACs (`multi-vm-networking.md` §3.1), M9 wants named
> snapshots plus a recorded disk set (`m9-suspend-resume.md` §8, the M10 cross-dependency),
> and the multi-VM app wants a VM library at all. Fields marked (later)
> ship with their feature, not before.

## 1. The problem

limina today is a CLI: one invocation = one anonymous VM, fully described by flags
(`--disk`, `--net`, `--memory`, `--window`, `--share`, …). Nothing persists between runs
except the disk images themselves. That is exactly right for a dev harness and exactly
wrong for the product:

- A user's "Fedora" is a *thing* they start, stop, snapshot, and reconfigure — not a
  20-flag incantation.
- Per-VM identity (UUID, MAC, ssh port pin, snapshot set) must survive restarts, or
  networking leases flap and snapshots have no home.
- The M10 disk-set manifest and M9 named snapshots need a durable place to live that
  travels with the VM.

## 2. The decision (proposed)

**A VM definition is a directory** — `<name>.liminavm/` — containing a single TOML config
plus everything owned by that VM:

```
Fedora.liminavm/
├─ vm.toml              # the definition (schema below)
├─ state.toml           # mutable machine state (window frame/size) — machine-written,
│                       #   safe to delete; kept OUT of vm.toml so a window drag never
│                       #   rewrites user config (see docs/design/display-modes.md)
├─ disks/               # default home for this VM's images (definition may reference
│                       #   absolute paths elsewhere — e.g. a shared base image)
│  └─ root.raw
├─ snapshots/           # M9: one subdir per named snapshot (RAM+device state + disk refs)
│  └─ 2026-07-01-pre-upgrade/
├─ nvram/               # EFI variable store, when we grow one
└─ logs/                # supervisor + worker + gvproxy logs for the last N runs
```

- **Default library location:** `~/Library/Application Support/Limina/VMs/` (macOS-native;
  the app's VM library enumerates this dir). A `.liminavm` dir is **self-contained and
  relocatable** — Finder-copyable like Parallels' `.pvm`, double-clickable later when the
  app registers the extension. `limina` accepts either a library name (`limina start
  fedora`) or a path to a `.liminavm` dir.
- **One file, TOML, versioned.** `vm.toml` starts with `config_version = 1`. Unknown keys
  are a *warning*, not an error (older limina opening a newer VM should degrade, matching
  the two-tier posture). Unknown *required* semantics bump `config_version`.
- **The CLI keeps working flag-only.** No definition required: today's invocation shape
  stays as the "ephemeral VM" path (harness, spikes, CI depend on it). Flags also override
  definition values when both are given (`limina start fedora --window --memory 8G`),
  which is how one-off experiments avoid editing the file.

## 3. `vm.toml` schema (v1 sketch)

```toml
config_version = 1

[identity]
# name  = "Fedora Workstation" # optional display override; defaults to the bundle name
uuid    = "3f2c1e9a-…"          # allocated at create, never changes; snapshot/networkd key
created = "2026-07-01T12:00:00Z"

[hardware]
cpus   = 4
memory = "8G"        # the MAXIMUM; managed VMs always boot dynamic with a 1 GiB floor
reclaim = "moderate" # disabled|light|moderate|aggressive (how hard idle memory is clawed back)
battery = true       # mirror the host battery into the guest (virtio-i2c SBS); default true

[[disk]]                        # ordered — attach order IS device order (M10)
path  = "disks/root.raw"        # relative = inside the bundle
id    = "root"                  # virtio serial → /dev/disk/by-id/virtio-root (libkrun 0038)
[[disk]]
path  = "/Volumes/Big/scratch.raw"
ro    = false

[[cdrom]]
path = "Fedora-Workstation-netinst.iso"

[[network]]                     # multi-vm-networking §3.1's Network object, per-VM slice
mode     = "nat"                # nat | (later) shared | bridged | host
mac      = "5a:94:ef:e4:0c:ee"  # allocated at create; stable across runs
ssh_port = 0                    # 0 = auto-allocate from 2222 (prints the resolved port)
# [[network.forward]] host = 8080, guest = 80   # (later) dynamic REST forwards

[[share]]
name = "Projects"
path = "~/Projects"
ro   = false

[display]
window     = true
gpu        = "auto"             # auto (coexist venus+sw2d) | software-2d
resolution = "host"             # host (match the window's screen; the DEFAULT) |
                                # dynamic (guest follows the window) | WxH fixed
                                # — see docs/design/display-modes.md

[input]
normalize_modifiers = true      # the shipped default; false = --no-normalize-modifiers

[guest]                         # written by install-guest-tools / read for the manifest check
tools_version = "7.1.2"
os_release    = "fedora-44"

[snapshots]                     # (M9) index maintained by limina, not hand-edited
# [[snapshots.entry]] name = "pre-upgrade", created = …, disks = [ {id="root", …} ]
```

Everything in the table maps 1:1 onto an existing flag (or a designed feature's knob); the
resolver produces the same internal spec the CLI produces today, so the worker and
supervisor don't change — **the definition layer is pure supervisor-side policy.**

## 4. Identity, locking, concurrency

- **UUID** is the durable key (`identity.name` is an optional mutable display label; the bundle
  directory name is the fallback and remains the CLI lookup key). `limina-networkd` (later) keys
  leases by UUID; snapshots record it; the privileged helper can scope grants by it.
- **MAC** is allocated at create (locally-administered range, hash of UUID for
  reproducibility) and stored — this is the piece the networking design needs *first*.
- **Locking:** `flock` on `vm.toml` (LOCK_EX|LOCK_NB) while the VM runs — the same pattern
  M10 applies per-disk — so double-starting a definition fails fast with a clear message.
  Disk-level flocks stay (a definition may reference a disk another VM holds).
- **Snapshots record the disk set** (id, path, size, format) and **fail closed** on
  restore mismatch — the M9§8 requirement gets its home here.

## 5. CLI surface (incremental)

Phase 1 (with the first consumer, likely the Network abstraction or M9):
- `limina create <name> [flags…]` — materialize a definition from today's flags (also the
  migration path: run once with your usual flags + `create`).
- `limina start <name|path> [override flags…]`, `limina ls`.

Phase 2 (with M9): `limina snapshot <vm> <name>`, `limina restore <vm> <name>`.
Phase 3 (app): the VM library UI enumerates the same directory; no second store.

## 6. What this is NOT

- Not a daemon or registry: files in a directory, no background state. `limina ls` reads
  the dir. (A management daemon can layer on later without changing the format.)
- Not a guest-config channel: nothing in `vm.toml` reaches the guest; guest-side facts
  (tools version) are *cached* here for the manifest check, source of truth stays in-guest.
- Not a new spec language: it is exactly the CLI, persisted.

## 7. Open questions

- Whether `nvram/` (persistent EFI variables) is worth adding before Windows/other-OS
  guests force it — today's KRUN_EFI is stateless.
- Definition-level `[policy]` knobs (autoballoon aggressiveness, capture sensitivity) —
  defer until a second user of each exists.
- Bundle-internal disk *ownership* semantics on delete (`limina rm` prompts for the bundle;
  absolute-path disks are never touched).

## 8. Library location & per-VM placement — PLANNED (2026-08-12)

Motivated by dogfooding on a small-disk mac mini with a big external APFS volume: the
library should be movable, and individual VMs should be placeable outside it, without a
hand-rolled symlink. Nothing here changes the bundle format — bundles stay self-contained
and Finder-relocatable; this is purely about *where the library points* and *what the
library enumerates*.

### 8.0 What already works (the baseline this builds on)

- `$LIMINA_VM_LIBRARY` overrides the whole library (`vmlib/bundle.rs:100-110`); all
  consumers (`ls`, resolve, the control center's create/import/delete) funnel through the
  one `library_dir()`.
- `limina create --dir <path>` creates a bundle anywhere (`main.rs::cmd_create`), and
  `resolve()` accepts an explicit path — anything with a `/` or a `.liminavm` suffix
  (`vmlib/bundle.rs:115-124`) — so out-of-library VMs already *run*; they just don't
  *enumerate*.
- `list()` follows symlinks (`read_dir` + `path.is_dir()`, `vmlib/bundle.rs:163`), so a
  `Name.liminavm` symlink inside the library enumerates and resolves like a real bundle.
  Symlinking the whole `VMs/` dir works too (the current stop-gap).
- `vm.toml` disk paths may be absolute (`VmBundle::resolve_path`), and `--in-place`
  references a disk where it lies — a library bundle can already keep its big image on
  another volume.
- Caveat that stays true throughout: the external volume should be APFS. Import relies on
  `clonefile(2)` with a plain-copy fallback (`vmlib/import.rs:188-202`) and raw images rely
  on sparse files; exFAT gets slow full-size copies and fully-allocated images.

### 8.1 Persisted library path (host config)

A host-level config file at `~/Library/Application Support/Limina/config.toml`
(deliberately *outside* the `VMs/` dir it points at, so it survives the library moving):

```toml
# config.toml v1 — host-side app settings; nothing here reaches a guest.
[library]
path = "/Volumes/Ext/Limina VMs"   # absent = the built-in default
```

Resolution precedence in `library_dir()`: **`$LIMINA_VM_LIBRARY` env > config.toml >
default** (`~/Library/Application Support/Limina/VMs`). The env var must stay on top: the
test suite mutates it under `ENV_LOCK` (`vmlib/bundle.rs:177`, `center/model.rs:167`) and
config outranking it would break test isolation. `library_dir()` re-reads the file per
call — no `OnceLock` cache — because the control center is long-running and must observe a
settings change without a restart, and every caller is already doing directory I/O so the
extra read is noise. A malformed config.toml is a loud warning + fall through to the
default, never a hard error (the control center must still open so the user can fix it).

Testability: the config path itself needs a scratch override (an env var, e.g.
`$LIMINA_CONFIG`, or deriving it from `$HOME` which the tests already control) so the
precedence tests can run under the existing `ENV_LOCK` pattern without touching the real
`~/Library`.

### 8.2 UI: change the library location

The control center gets its first settings surface — either a standard Settings… window
(Cmd-,) or, cheaper for v1, a single "Change VM Library Location…" item (app menu or a
gear button) driving an `NSOpenPanel` directory picker. Selecting a directory writes
`[library] path` to config.toml and refreshes the list.

**v1 semantics: repoint, don't migrate** — a decision, not an omission. Bundles are
Finder-copyable by design (§0 of this doc), so "move my VMs" is: stop VMs, drag bundles in
Finder, repoint. An in-app migrator (multi-GB copies with progress UI, running-VM flock
guards, partial-failure rollback) is real work for little gain and is deferred until asked
for. The picker should *offer* to reveal the old library in Finder when it's non-empty.

### 8.3 Per-VM placement: symlink-as-registration

For "this one VM lives on the external disk", the mechanism is the one `list()` already
supports: a `Name.liminavm` **symlink in the library** pointing at the real bundle. The UI
grows two affordances that create/manage such links:

- **"Add existing VM…"** — pick a `.liminavm` anywhere; if it's outside the library,
  symlink it in (name-collision checked). Also the natural "re-attach after Finder-moving
  a bundle" flow.
- **Create-at** — the create/import sheet gets an optional location field; when it's not
  the library, create the bundle there and symlink it in (CLI parity: `create --dir`
  learns `--link`, or does this by default when `--dir` ≠ library).

Rejected alternative: a `paths = […]` registry list in config.toml. It quietly violates
§6's "not a registry" tenet — the library would no longer be self-describing as a
directory, and every enumeration/removal path would need a second source of truth. The
symlink *is* the registration, visible in Finder and `ls -l`, removable with the bundle
card's existing Move-to-Trash (which must trash the *link*, not the target, for linked
bundles).

### 8.4 The unmounted-volume failure modes (the part that actually needs care)

With the library (or a linked bundle) on `/Volumes/Ext`, the disk being unplugged must
degrade legibly, not corrupt state:

- **Dangling bundle symlink**: `path.is_dir()` is false → today the VM silently vanishes
  from `ls`/the center. Plan: `list()` (or a center-side sibling) additionally surfaces
  extension-matching entries whose target is missing, and the center renders them
  greyed-out — "volume not mounted" — with actions disabled except "Remove from library"
  (deletes the link only). `limina ls` marks them `unavailable`.
- **Library volume absent + creation**: `cmd_create` and the center's create/import/
  clone destinations all do an unconditional `create_dir_all(library_dir())`
  (`main.rs::cmd_create`; `center/controller.rs:812,916,980`). With
  `path = "/Volumes/Ext/…"` unmounted, that silently creates a *real* directory on the
  boot volume, which the next mount then shadows — two divergent libraries, the worst
  outcome. Plan: when the configured path lies under a volume root that is not mounted
  (or the path is a symlink whose target is missing), **refuse creation** and show a
  banner/alert ("VM library volume not mounted") instead of creating. Enumeration
  already degrades fine (missing dir = empty library, `vmlib/bundle.rs:157`) — but the
  center should show the same banner rather than an empty "create your first VM" state,
  which would invite exactly the shadow-directory mistake.

### 8.5 Increments

1. **config.toml + precedence in `library_dir()`** + the creation guard (§8.4 bullet 2),
   RED-first under `ENV_LOCK`. CLI-visible immediately (no UI needed to benefit).
2. **Location picker UI** writing the config (§8.2).
3. **Symlink registration** — "Add existing VM…", create-at, dangling-link rows (§8.3 +
   §8.4 bullet 1).
