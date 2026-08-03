# suspend/resume on the ad-hoc dev path — investigation (2026-08-03)

Surfaced by the imago fork-model pilot's live-validation poke VM: **resume of an ad-hoc
(`cargo xtask run` / `boot-enhanced-efi-kk.sh`) EFI+venus VM wedges**, while the **managed
product path resumes fine** on the same image, build, and host.

## Wedge signature

- Window stuck on "Resuming…" indefinitely; one full core pegged.
- The pegged core is a **guest vCPU executing guest code 100% of the time** (`sample`:
  all frames inside `hv_trap`; the other vCPUs idle in WFI via `wait_for_interrupt`) —
  a guest-side livelock, not a VMM spin.
- Guest never answers ARP (gvproxy: `no route to host` forever); resume-leg serial
  console (`--console`) stays **empty** — not one byte printed after restore.
- Host-side threads all parked (event loop, vmm worker, vcpu 0 in WFI).

## Exonerated by A/B (each failed/passed identically)

| candidate | verdict | evidence |
|---|---|---|
| imago at upstream tip | **exonerated** | old imago (0.2.2+2 patches) wedges identically |
| dynamic memory + FRQ (`--memory`, `--balloon-free-page-reporting`) | **exonerated** | minimal no-balloon config wedges |
| poisoned first snapshot (taken from a supervisor-orphaned worker) | **exonerated** | clean suspends (supervisor alive, SIGUSR1 to the worker) wedge too |
| snapshot mechanism / product path | **works** | `create`/`start`/`suspend`/`start` cycle resumes OK (`MANAGED-RESUMED-OK`), matching dogfood |

Suite context: all 47 test targets green the same day (including `l1_snapshot`,
`venus_session_preserved` — seated venus + net + snapshot round-trip).

## Prime suspect (A/B pending at time of writing)

Ad-hoc launches roll a **random `--vsock-port` per spawn** (worker cmdline, e.g.
`--vsock-port 1279872329`), while resume requires "the SAME machine config it was
suspended with" (`--snapshot-file` docs; a mismatched restore is undefined — the disk-
destruction case is why there's no `--restore` flag). Managed bundles pin their config.
`adhoc-cycle.sh --vsock-port 52000` is the discriminating experiment.

## Scripts

- `adhoc-cycle.sh [extra limina flags]` — one full ad-hoc boot → SIGUSR1 suspend →
  resume → SSH probe cycle against a fresh APFS clone of the F44 enhanced image.
  Disciplined: preflight asserts **zero** limina processes and port 2299 free, asserts
  exactly **one** worker before signaling, verifies the snapshot file, reaps on every
  failure path. (Three earlier hand-run legs were invalidated by stragglers holding the
  pinned port / a second worker answering the probe — don't run cycles without this.)
- `managed-cycle.sh <limina subcommand...>` — wraps `target/debug/limina` in the dev KK
  env (devenv ICD + zink selectors from `boot-enhanced-efi-kk.sh`) so managed
  `create`/`start`/`suspend` work from a dev tree.

Operational traps burned into memory ([[limina-github-migration]]):
- SIGUSR1 must target the worker matched by `limina-vmm --cpus` argv; `pgrep -f
  limina-vmm` also matches the supervisor's `--vmm-bin` argument — signaling the
  supervisor terminates it (default disposition) and orphans the worker.
- A snapshot taken from a supervisor-orphaned worker is poisoned (window, present-ack
  path, and gvproxy all dead at capture) — discard it.

## Follow-ups (queued, not started)

1. Ad-hoc red-button suspend: honor `on_window_close = Suspend` when `--snapshot-file`
   is given (today a flat `--disk` run hard-resolves to Shutdown; `main.rs` ~line 136).
2. Resume config mismatch must fail LOUDLY (refuse to restore) or be made robust —
   see the roadmap item once the vsock experiment concludes.
3. L2 test: snapshot round-trip under dynamic memory + FRQ (no coverage exists).
4. Fix the stale "only the un-tested EFI path runs the real 7.1.4" comment in
   `crates/limina-test/src/lib.rs` (~795) — EFI is the shipped boot path and managed
   suspend/resume on it demonstrably works.
