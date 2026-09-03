# vcpu-replug-trace — a real-life sample for tuning grow eagerness

**Why.** With power-saver reclaim live on the dogfood guest (agent 0.6.0, 2026-09-03), the
observed behavior is: shrink walks down correctly, but the guest **re-plugs too eagerly** — small
activity bursts repeatedly grow the machine back to max. That is the policy working as designed
(`crates/limina/src/vcpu_policy.rs`: grow is immediate, jumps to max, no dwell), but the design
needs numbers: every grow trigger's threshold *shrinks with the machine* (`nr_running > online`,
`loadavg1 >= online`, worker CPU ≈ `online` cores), so a shrunk guest is disproportionately easy
to startle. Whether that is wrong — and what dwell/hysteresis/partial-grow would fix it without
re-opening the balloon-style oscillations — is a question for a trace of real use, not intuition.

**What.** `limina-vcpu-trace` samples, every 2 s, the inputs the policy consumes today (the
agent builds its CpuPressure report from the same files) plus what a better policy would likely
weigh: `online`, `nr_running` (`procs_running`), loadavg, PSI cpu, and cumulative
`busy:idle:iowait:steal` jiffies for the aggregate `cpu=` and every online `cpuN=` — diff
consecutive samples for exact total and per-vCPU utilization over any window (steal = the host
descheduling us). An offline vCPU has no `cpuN` field, recording the online *set*, not just the
count. Zero forks per sample (all bash builtins; the 2 s wait is a
`read -t` on a never-ready fd), so the sampler cannot perturb the `nr_running` it measures.
One `key=value` line per sample into `/var/log/limina-vcpu-trace/vcpu-trace.<date>.log`,
~15 MB/day at 10 vCPUs; prune old days by hand. Deployed to the dogfood guest 2026-09-03 as
`limina-vcpu-trace.service`; it is a temporary tuning aid, remove it when the tuning lands.

**Reading it.** Find `online` transitions upward, look at the surrounding lines:

- `nr_running > online` at the transition → the runnable-spike trigger.
- `load1 >= online` → the loadavg trigger.
- neither → by elimination the **host-side** term (`host_busy + slack >= online*100`), which the
  guest cannot see. The worker's constant overhead (display, timers — ~0.7 cores observed idle)
  counts toward it, so at small `online` this term is the prime eager-replug suspect.

The trace also measures shrink cadence (one step per 20 s dwell) against real quiet periods, and
PSI (`some avg10`) around shrunken operation shows whether reclaim ever actually starved anything
— the v2 signal the policy carries but does not yet use.

## Early findings (first 99 min of dogfood trace, 2026-09-03)

The sampler validates: 2962 samples, 0 malformed, cadence 2 s (26 samples at 3 s), no stalls, the
`online=` set always agrees with the `cpuN` fields, and 0 per-CPU counter anomalies — which also
confirms empirically that per-CPU jiffies persist across hotplug. `analyze.py` is the reader.
(Trap it already caught: `/proc/stat`'s aggregate `cpu` line sums only online CPUs, so it *drops*
by a CPU's whole history at every offline — utilization must come from per-CPU deltas.)

What the trace shows: a full sawtooth. 44 grow-to-max events and 242 single-step shrinks in
99 minutes — a bounce every ~2.3 min — while the guest averaged 5.6% busy (0.39 cores) and total
PSI cpu stall was 33 s. Shrink spacing (min/median 20/22 s) matches the dwell by design. Reclaim
is not starving anyone; the problem is pure churn, and hotplug churn is not free
(see the vCPU-hotplug cost notes: worker CPU rises with hotplug activity).

Attribution so far — three suspects eliminated, one standing:

- **Guest triggers, as visible at 2 s:** only 2/44 grows coincide with `nr_running > online` or
  `load1 >= online`. The other 42 show load1 ≪ online and small nr_running.
- **The host term:** ruled out by measurement — 180 s of 1 Hz worker CPU on the host showed
  mean 0.65, p99 1.43, max 1.98 cores; the eliminated grows needed ≥ 2.75–6.75 cores in a second.
- **Profile floor rise:** tuned/tuned-ppd journals show no profile changes at the grow moments.
- **The guest acting alone:** no — the agent's own `cpuN -> online` journal lines show it
  applying host-sent targets.

Standing hypothesis: **instantaneous `nr_running` spikes at the agent's own sampling moment.**
`procs_running` is a point sample with no decay; the agent reads it right after writing its
heartbeat + MemPressure to vsock, so the TX kworker it just woke may still be runnable — the
sensor partly measures its own wake path, and the threshold shrinks with the machine (at 3
online, four momentarily-runnable tasks suffice). A 2 s external sampler cannot confirm this.

**The decisive instrument already exists**: the policy's grow line
(`dynamic vCPUs: N online but R runnable (load1 L, host H cores) → asking for M`) logs the exact
values it acted on, at `info`. Launch the dogfood app with `RUST_LOG=warn,limina=info` (keep the
bare `warn`) and the next day of supervisor.log attributes every grow directly.
