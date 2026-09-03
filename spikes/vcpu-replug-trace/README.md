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

A dogfood desktop under `power-saver` (99 min, 2962 samples, 0 malformed, 0 per-CPU counter
anomalies — which also confirms per-CPU jiffies survive hotplug) sawtoothed: **44 grows to max and
242 single-step shrinks, a bounce every 2.3 minutes**, while the guest averaged 5.6% busy
(0.39 cores) and accumulated 33 s of PSI cpu stall in the whole run. Shrink spacing (min/median
20/22 s) matched the dwell exactly. Reclaim was not starving anything; the harm was pure churn, and
hotplug churn costs host CPU.

Attribution: at **every one of the 44 grows** the guest was burning 0.18–1.61 cores (median 0.32)
on 3–7 online. Over all 2961 intervals the guest never once reached 0.75 × online busy, never had
`load1 >= online`, and PSI `some avg10` peaked at 2.54%. One sample read `online=4 nr_running=12
busy=0.46 cores`: twelve tasks in R state in a second that consumed less than half a core.

That is the whole diagnosis. `procs_running` is a point sample of a spiky quantity, tasks woken
together are runnable for microseconds before the scheduler places them, and on a machine the
policy has already shrunk a handful already exceeds `online`. The rule now requires a spike to be
corroborated by CPU actually burned over the interval, with PSI stall over the interval as an
independent trigger for loads that make tasks wait without filling the machine.

Measured after the change, same sampler, seated F44 enhanced guest, `--cpu-reclaim moderate`,
6 vCPUs:

| stimulus | outcome |
|---|---|
| idle desktop | walks 6 → 2 in 90 s and **stays**; no grow |
| 6 spinners on 2 online | grows to 6 **within 1 s** (1.83 cores busy, 88% stalled) |
| 2 spinners on 2 online | grows to 6 within 1 s (1.99 cores busy, 0.96% stalled) |
| 1 spinner on 2 online, 30 s | **no grow** — one core of work fits in two CPUs |

Note what carried each real burst: `load1` was 0.00–0.40 and the host term 0.04–0.05 cores, so
neither of the pre-existing fast paths would have caught either one, and the two-spinner case
grew on utilisation with almost no stall while the six-spinner case had both. Both new signals
earn their place.

## Reading a trace

Find upward `online` transitions and look at the samples around them. `analyze.py` does the
integrity checks (cadence, per-CPU monotonicity, `online=` set vs which `cpuN` fields are present)
first, because a tuning decision gets made from this data, then time-in-state, utilisation, grow
attribution and shrink spacing.
