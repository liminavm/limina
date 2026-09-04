# vcpu-replug-trace — the guest-side sampler that tuned the vCPU grow rule

**What it is.** `limina-vcpu-trace` samples, every 2 s, the inputs the host vCPU policy consumes
(`crates/limina/src/vcpu_policy.rs`; the agent builds its `CpuPressure` report from the same files)
plus everything a candidate policy might want to weigh: the online *set*, `nr_running`
(`procs_running`), loadavg, PSI cpu, and cumulative `busy:idle:iowait:steal` jiffies for the
aggregate `cpu=` and every online `cpuN=`. Diff consecutive samples for exact per-vCPU and total
utilisation over any window (steal = the host descheduling us). Zero forks per sample — every read
is a bash builtin, the 2 s wait is a `read -t` on a fd that never delivers — so the sampler cannot
perturb the `nr_running` it measures. One `key=value` line per sample into
`/var/log/limina-vcpu-trace/vcpu-trace.<date>.log`, ~15 MB/day at 10 vCPUs. `analyze.py` reads it.

It is a tuning aid, not a product: install it on a guest when the vCPU policy needs evidence,
remove it when the tuning lands.

**Trap it exists to avoid.** `/proc/stat`'s aggregate `cpu` line sums only the CPUs online at the
instant of the read, so it *drops* by an offlined CPU's whole accumulated history and jumps by an
onlined one's. Differencing it across a hotplug yields a wild number in exactly the samples a vCPU
policy is most sensitive in. Utilisation must come from per-CPU deltas over the set present at both
ends — `limina_proto::CpuSampler` does the same thing in the agent, for the same reason.

## What it established

**A runnable-task spike is not demand.** A dogfood desktop under `power-saver` (99 min) sawtoothed:
44 grows to max and 242 single-step shrinks, a bounce every 2.3 minutes, while the guest averaged
0.39 busy cores and accumulated 33 s of PSI cpu stall in the whole run. At **every one of the 44
grows** the guest was burning 0.18–1.61 cores (median 0.32) on 3–7 online; over all 2961 intervals
it never once reached 0.75 × online busy and never had `load1 >= online`. One sample read
`online=4 nr_running=12 busy=0.46 cores`. `procs_running` is a point sample of a spiky quantity,
tasks woken together are runnable for microseconds before the scheduler places them, and on a
machine the policy has already shrunk a handful already exceeds `online`. So a spike now has to be
corroborated by CPU actually burned over the same interval.

**PSI stall is not scale-free either.** With the spike gated, the stall backstop became the
dominant trigger, and 18 h of ordinary desktop use exposed why. On a workload averaging 0.50 busy
cores throughout, *baseline* median stall rose steadily as the policy shrank the machine — 0.4% at
9 online, 2.9% at 3, **5.0% at 2** (p90 7.8%) — because the same wakeups contend harder on fewer
CPUs. An absolute gate therefore gets more hair-triggered the better the shrink works: grows ran at
**30.7/h at 2 online, 5.6/h at 3, and ~3/h at 4 and above**. The host term was not involved (the
worker was burning 0.42 cores; at 2 online it would need 1.75). The stall path now carries a
utilisation floor of its own, lower than the spike gate because non-saturating loads are exactly
what it exists for.

Neither threshold came from the other's evidence, and both were kept honest by a controlled rig —
a seated F44 enhanced guest, `--cpu-reclaim moderate`, 6 vCPUs:

| stimulus | outcome |
|---|---|
| idle desktop | walks 6 → 2 in 90 s and **stays**; no grow |
| 6 spinners on 2 online | grows to 6 **within 1 s** (1.83 cores busy, 88% stalled) |
| 2 spinners on 2 online | grows to 6 within 1 s (1.99 cores busy, 0.96% stalled) |
| 1 spinner on 2 online, 30 s | **no grow** — one core of work fits in two CPUs |

`load1` was 0.00–0.40 and the host term 0.04–0.05 cores at both real bursts, so neither pre-existing
fast path would have caught either one; and both bursts sat at 91% and 99.5% of online, far above
the stall path's floor. Every signal earns its place, and the floor costs neither burst.

## Reading a trace

Find upward `online` transitions and look at the samples around them. `analyze.py` does the
integrity checks (cadence, per-CPU monotonicity, `online=` set vs which `cpuN` fields are present)
first, because a tuning decision gets made from this data, then time-in-state, utilisation, grow
attribution against the live thresholds, and shrink spacing.

The sampler's 2 s interval is coarser than the agent's 1 s report, so it smooths stall peaks the
policy acts on: a trace showing no crossing while the host log shows grows is that gap, not a
contradiction. Attribution mirrors the policy's constants — keep the copies at the top of
`analyze.py` in step with `vcpu_policy.rs`.
