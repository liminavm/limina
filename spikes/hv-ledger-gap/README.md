# hv-ledger-gap — naming the phys_footprint bucket footprint(1) can't itemize

Context (2026-08-10, dogfood Mac): the Dev VM worker showed phys_footprint 49.2 G, but
`footprint(1)` itemized only 26 G and vmmap's region table accounted ~29 G. The
missing ~23 G tracks guest touched-high-water through the HVF mapping — the known
"footprint(1) misses hv-mapped guest RAM" blind spot — but no userland tool names
the ledger entry it's charged to. `ledger-dump.c` asks the kernel directly via the
private-but-stable `ledger(2)` syscall (LEDGER_TEMPLATE_INFO + LEDGER_ENTRY_INFO):
every per-task ledger entry, by name, balance/credit/debit.

Build: `clang -O2 -o ledger-dump ledger-dump.c`
Run:   `./ledger-dump <pid>` (as the task's owner or root); `-a` includes zeros.

## churn-probe — the local reproducer (round 3+)

The restart experiment (RESULTS.md round 3) showed the worker's excess is real
compressed slots that detach from the guest-RAM object, keep billing the task
ledger until death, then survive as unattributed swapfile garbage. `churn-probe.c`
mimics the dogfood shape to find the trigger: anon `MAP_PRIVATE|MAP_NORESERVE`
buffer (vm-memory's `from_ranges` flags), optional `hv_vm_map` (`-H`), dirty →
ballast-forced compression (disposition-gated; a cycle that never compresses
prints VOID) → optional `MADV_FREE_REUSABLE` (`-R`, the balloon-FRQ suspect) →
re-dirty, times N.

Build+run via `./run-churn.sh <label> [probe args]` — it refuses to run beside
an HVF bench, samples the system before/after, and the verdict is the POST-EXIT
residue: system "stored in compressor" not returning to baseline = the orphan
leak reproduced. `churn-probe` needs the hypervisor entitlement for `-H`
(codesign with `../balloon-madvise/hv.entitlements`; `run-churn.sh` assumes a
built, signed binary).

Self-test traps already caught (both would silently fake a negative):
- a ballast child forked AFTER the buffer is dirtied holds a COW reference and
  `MADV_FREE_REUSABLE` silently no-ops (rc=0, nothing reclaimed);
- re-dirtied REUSABLE pages stay off-footprint (`internal`/`phys_footprint`
  unmoved) when nothing calls `MADV_FREE_REUSE` — which production never does.

Read `RESULTS.md` (written after the dogfood run) before drawing conclusions.
