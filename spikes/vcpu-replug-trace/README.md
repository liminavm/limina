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
