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
├─ disks/               # default home for this VM's images (definition may reference
│                       #   absolute paths elsewhere — e.g. a shared base image)
│  └─ root.raw
├─ snapshots/           # M9: one subdir per named snapshot (RAM+device state + disk refs)
│  └─ 2026-07-01-pre-upgrade/
├─ nvram/               # EFI variable store, when we grow one
└─ logs/                # supervisor + worker + gvproxy logs for the last N runs
```

- **Default library location:** `~/Library/Application Support/limina/VMs/` (macOS-native;
  the app's VM library enumerates this dir). A `.liminavm` dir is **self-contained and
  relocatable** — Finder-copyable like Parallels' `.pvm`, double-clickable later when the
  app registers the extension. `limina` accepts either a library name (`limina start
  fedora`) or a path to a `.liminavm` dir.
- **One file, TOML, versioned.** `vm.toml` starts with `config_version = 1`. Unknown keys
  are a *warning*, not an error (older limina opening a newer VM should degrade, matching
  the two-tier posture). Unknown *required* semantics bump `config_version`.
- **The CLI keeps working flag-only.** No definition required: today's invocation shape
  stays as the "ephemeral VM" path (harness, spikes, CI depend on it). Flags also override
  definition values when both are given (`limina start fedora --window --memory 2G..8G`),
  which is how one-off experiments avoid editing the file.

## 3. `vm.toml` schema (v1 sketch)

```toml
config_version = 1

[identity]
name    = "Fedora"
uuid    = "3f2c1e9a-…"          # allocated at create, never changes; snapshot/networkd key
created = "2026-07-01T12:00:00Z"

[hardware]
cpus   = 4
memory = { min = "2G", max = "8G" }   # M6 dynamic range; a bare string = fixed size

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
resolution = "window"           # window-follow (shipped resize) | WxH fixed

[input]
swap_cmd_opt = true             # the shipped default; false = --no-swap-cmd-opt

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

- **UUID** is the durable key (name is a mutable label). `limina-networkd` (later) keys
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
