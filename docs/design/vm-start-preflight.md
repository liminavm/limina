# Design — pre-flight checks for VM start

Status: PROPOSED 2026-08-27. Host-side only; nothing here touches the worker, libkrun, or a
guest. Builds on the managed-VM machinery in `docs/design/vm-definitions.md`.

## 1. The problem

A managed VM whose `vm.toml` names a disk that is not there fails to start, and the control
center says nothing at all: the play button flashes, the row stays "stopped", and the reason
lands in a per-run-truncated `logs/supervisor.log` nobody opens.

The check itself is not missing. Three layers already hold the disqualifying fact:

- `center/model.rs:95-97` computes `root.raw (missing)` and renders it in the row's disk line.
- `main.rs:2157 validate_disk_path` produces the precise message — *after* the supervisor has
  forked, so it reaches only the log.
- `limina ls` prints `stopped` with no hint.

What is missing is any path from that fact to the button. `center/spawn.rs:23-48` returns
`Ok(())` the instant `Command::spawn` succeeds and a detached reaper discards the child's exit
status (`:44-47`), so the only errors the UI can ever raise from a click are `current_exe`
failing, `logs/` or `supervisor.log` not being creatable, and `spawn` itself failing. **Every
substantive start failure is invisible by construction.**

## 2. Two layers

**Pre-flight** makes failures *specific and early*. It is a prediction, and predictions have
gaps.

**The reaper** makes failures *impossible to miss*. It catches whatever pre-flight did not
anticipate.

Pre-flight alone re-creates the silent class the first time something fails for an
unenumerated reason. The reaper alone leaves the user clicking a button that was never going
to work. The property we want — *no start ever fails silently* — needs both.

## 3. Layer 1 — `vmlib/preflight.rs`

A pure module beside `bundle`/`runtime`/`schema`: no AppKit, no process spawning, unit-testable
without HVF.

```rust
pub fn check(bundle: &VmBundle, cfg: &VmConfig, depth: Depth) -> Report

pub enum Depth { Cheap, Full }   // Cheap = stat-only, safe on the center's 1 s timer
pub enum Severity { Blocker, Warning }
pub struct Finding {
    code: Code,               // enum — tests assert on this, never on prose
    severity: Severity,
    subject: String,          // the RESOLVED absolute path, or the config key
    message: String,
    remedy: Option<String>,   // what to actually do about it
}
```

### Invariants

1. **Read-only.** No `create_dir_all`, no touch, no lock left held. This is load-bearing:
   `vm-definitions.md` §8.4 documents how an unconditional `create_dir_all` against an
   unmounted volume silently creates a shadow library on the boot disk. A pre-flight that
   "helpfully" repaired things would be that bug wearing a new name.
2. **Conservative bias.** A check that cannot *prove* failure emits a Warning, never a Blocker.
   Pre-flight must never be the reason a VM that would have booted does not.
3. **Advisory; the start path stays authoritative.** There is an irreducible TOCTOU gap between
   check and spawn. `run_vm` keeps every check it has today. Pre-flight front-runs them, it does
   not replace them.
4. **One implementation per check.** `validate_disk_path` and `build_disk_args`' uniqueness test
   move *into* preflight and `main.rs` calls them there, so the pre-spawn refusal and the
   supervisor's own error are the same string rather than two that drift.

### Tier A — static, `Depth::Cheap`

Stat-only and cross-VM-lock probes; cheap enough for the center's refresh.

| Check | Severity | Today |
|---|---|---|
| `config_version`, non-empty `identity.name` | Blocker | `schema::validate` |
| each `[[disk]]`: exists · file-or-blockdev · readable · writable unless `ro` | Blocker | `main.rs:2157` |
| each `[[cdrom]]`: same predicate | Blocker | `main.rs:2098` |
| same image attached twice (canonicalized) across disks+cdroms | Blocker | `main.rs:2071-2078` |
| a writable disk is already locked by another running VM | Blocker | probe-and-release, §3.3 |
| `hardware.cpus` within a sane range | Blocker | **nothing, anywhere** |
| `hardware.memory` parses and is ≥ the 1 GiB dynamic floor | Blocker | `schema.rs:203` (late) |
| `network[0].ssh_port` is 0 or ≥ `SSH_PORT_MIN` | Blocker | `main.rs:1472` (late) |
| `ssh_port` set with no `[[network]]` | Blocker | `main.rs:980` |
| `boot.firmware`, when configured, exists | Blocker | **never checked** |
| firmware resolvable when not configured | Blocker | `resolve_windowed_firmware` |
| GOP firmware absent, krunkit `silent.fd` present | **Warning** | `main.rs:1227` |
| `[[network]]` present → gvproxy binary exists and is executable | Blocker | resolution only |
| bundle on an unmounted volume / dangling symlink | Blocker | `vm-definitions.md` §8.4 |
| `run/lock` exists but is unreadable | Blocker | reads as `Stopped`, §3.4 |

The `silent.fd` Warning matters more than it looks: that blob is a DEBUG build whose live
ASSERTs end in `CpuDeadLoop` — the #14 cold-boot wedge. Booting onto it is legitimate as a last
resort and worth saying out loud.

### Tier B — environmental, `Depth::Full` only

Never on the timer.

| Check | Severity |
|---|---|
| bundle `run/lock` already held (`runtime::status`) | Blocker |
| explicit `ssh_port` already bound on the host | **Warning** — a bind probe is racy by nature |

### 3.3 The disk-conflict check is reliable, not best-effort

The worker already takes a non-blocking `LOCK_EX` on every **writable** disk's backing file
(`limina-vmm/src/main.rs:346-373`), with the message written:

> `disk {:?} is already attached read-write to another running VM; attach it :ro to share it,
> or stop the other VM`

Pre-flight performs the same probe and releases it. Because the lock lives on the file rather
than on a bundle, this catches flat `limina --disk X` runs started from a terminal as well as
managed VMs. Read-only disks are not locked and correctly need no check.

### 3.4 Ordering

`cmd_start` (`main.rs:702-713`) takes the run flock at `:709` — after config translation, but
before any resource validation, all of which lives inside `run_vm`. Two consequences: disk,
firmware and gvproxy failures all occur with the lock held, and a config that fails to
translate errors out before the "already running" message can ever be reached. Pre-flight runs
**before `runtime::acquire`**, which settles both.

Separately, `runtime::status` treats an unreadable `run/lock` as `Stopped`
(`runtime.rs:167-169`), so a permission-broken bundle reads as stoppable in `ls` and the center
and then fails at `acquire`. Tier A distinguishes the two.

### Tier C — deliberately not checked

Written down so it is not "improved" later.

- **GPU / venus availability.** Degrading to software-2D is designed behavior under the
  two-tier guarantee, not a start failure. Absent venus env already returns `None`
  (`venus_env.rs:34-36`) and the run continues. A Blocker here would fight the architecture.
- **Control-plane bind failure.** Degrades by design (`main.rs:1343-1347`).
- **Codesign / hypervisor entitlement.** Not usefully predictable from outside the process; the
  `hv_vm_create` call is the honest oracle. Handled by error mapping in §4 instead.
- **Free host disk space.** Every image here is a sparse 40 G file; any threshold is a guess,
  and a wrong Blocker is worse than no check.
- **Total configured memory vs. host RAM.** Managed VMs are dynamic with a floor;
  overcommitment is the design, not an error.
- **Anything guest-side.** Pre-flight ends at the VM boundary.

### 3.5 Call sites

One module, four callers:

1. **`spawn::start_vm`** — `Depth::Full`; Blockers become `Err` *before* the spawn, and the
   existing alert at `controller.rs:134,147` fires unchanged.
2. **`limina start <vm>`** — the same call, findings to stderr, non-zero exit. A terminal start
   and a button click give the identical message.
3. **`model.rs::snapshot`** — `Depth::Cheap`, adding `VmRow.blocked: Option<Finding>`. The row
   shows the reason inline (extending the `(missing)` it already computes) and the play button
   is **disabled with a tooltip naming the cause**, so the un-startable button is never
   clickable. `limina ls` gains the same column.
4. **`reset_vm`** — inherits it through `start_vm`; already off the main thread.

`startClicked` runs on the AppKit main thread and a `stat()` against a dead network mount can
block for seconds. `disks_line` already stats on the 1 s timer, so the exposure is pre-existing
rather than new — but Tier A moves to the background refresh thread as part of this rather than
inheriting the hazard silently.

## 4. Layer 2 — the reaper

`spawn.rs:44-47` currently discards the child's exit status. Instead: keep it, and when the exit
is non-zero *and* the child lived less than a grace window (~5 s, long enough that a guest's own
later crash is not misreported as a start failure), read the tail of `logs/supervisor.log` into
the controller's existing error queue, which `controller.rs:444` already drains into an alert.

`spawn.rs` is deliberately AppKit-free while the queue lives in the controller, so `start_vm`
takes the `Arc<Mutex<Vec<String>>>` (or a small `FnMut(String)` sink) as a parameter — the shape
`reset_vm` already threads.

**Error mapping.** Some failures are unpredictable but recognizable. A worker without
`com.apple.security.hypervisor` surfaces as `Error::VmCreate` out of `krun::boot`
(`limina-vmm/src/main.rs:689`); the reaper recognizes that variant and reports the missing
entitlement rather than the raw error. The same trick applies to any class we can name after the
fact but not before.

## 5. Message discipline

Reliability here is mostly a property of the strings.

- Name the **resolved absolute path**, never the config-relative one.
- Distinguish **not-found / permission-denied / wrong-type**; `validate_disk_path` already does,
  and every new check follows it.
- Carry a **remedy clause**. `(pass :create=SIZE to make a new disk)` is the model.
- **One `Code` per finding**, so tests pin the code and the prose stays free to improve.

## 6. `limina check <vm>`

The same `Depth::Full` report, printed, exit code by severity. Near-free once the module exists;
it diagnoses a VM copied to another Mac in one line, and gives the L1 test a stable surface to
assert against instead of parsing `start`'s stderr.

## 7. Testing (RED-first)

- **Unit, per finding**, in `preflight.rs`, using the `ENV_LOCK` + `scratch_library` pattern from
  `vmlib/bundle.rs::tests`. No HVF, so these run under a plain `cargo test`.
- **Center model** — a snapshot over a VM whose disk was deleted marks the row blocked with the
  right code; extends the existing broken-bundle test in `model.rs`.
- **L1 CLI**, in `limina-test/tests/vmdef.rs` — create a managed VM, delete its disk, assert
  `limina start` exits non-zero with the specific message **and that no worker was spawned**.
  That last assertion is what actually pins the "before the spawn" property.
- **Reaper** — a bundle rigged to fail post-spawn produces a non-empty error queue. This is the
  regression test for the originating bug and is written first.

## 8. Out of scope, noted here because the audit surfaced them

- **`[[share]]` paths are not bundle-resolved** (`main.rs:964-972`) while `[[disk]]` and
  `boot.firmware` are (`:953-958`, `:1000-1003`). A relative `share.path` therefore resolves
  against the process CWD — for a center-launched VM, wherever the app happened to be launched
  from. A latent bug, fixed separately.
- **`hardware.cpus` is unvalidated end to end**: `cpus = 0` loads, translates (`main.rs:1020`),
  and reaches the worker raw (`limina-vmm/src/main.rs:651`). Tier A stops it at the door; the
  worker should reject it too.
