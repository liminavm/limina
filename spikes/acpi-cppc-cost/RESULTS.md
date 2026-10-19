# CPPC counter cost on HVF — RESULTS

Question: if limina's ACPI machine gives each vCPU a `_CPC` whose delivered/reference counters
are MMIO registers, Linux's `cppc_cpufreq` frequency-invariance engine (FIE) polls them, and every
MMIO read is a VM exit. What does that cost, and which `_CPC` layout should we build?

## Answer

Measured 2026-10-19 on an otherwise idle M1 Mac mini (16 GB, macOS 27.0.1), 4-vCPU / 4 GiB stock F44 guest
(kernel 6.19.10), release `limina-vmm` from the installed app (Oct 16 build).

- **One guest MMIO read = one exit ≈ 1.24 µs** (release worker, median; RAM read 0.31 ns).
  Under a full CPU load the effective cost is ≈ 1.6 µs per read (cache/TLB disturbance included).
- **MMIO counters, steady state: −0.34 % throughput vs no module (≈ −0.3 % net of the timer-only control)** on a guest with every vCPU busy, read the
  master/7.2 way (2 reads in the tick, 1000 Hz). That is 2000 exits/s per busy vCPU and nothing on
  idle ones. **Acceptable.**
- The F44 kernel actually shipping (6.19) reads them through an irq_work + one SCHED_DEADLINE
  kthread instead: **−0.88 %**, of which −0.66 % is the irq_work/kthread machinery itself and is
  paid for *any* non-PCC layout, Integer constants included. Only a PCC layout or
  `cppc_cpufreq.fie_disabled=1` avoids it on 6.19; 7.2+ (F44 `updates`) does not have it.
- **FFH/AMU is impossible here:** HVF reports AMU = 0, and an FFH entry without AMU makes Linux
  reject the whole `_CPC`.
- **Recommendation:** build `_CPC` with SystemMemory counters if we want real CPPC FIE — the cost is
  within noise of anything else limina does per tick. If what we want from `_CPC` is only CPU
  capacities (for EAS), put the counters in a layout nobody polls (§5), and let
  `qemu,virtual-cpufreq` keep DVFS/FIE at 1 read per tick. Integer-constant counters are the
  cheapest *valid* layout on 7.2+ but leave FIE registered and pinned at 1024, which also silences
  cpufreq's requested-frequency invariance — prefer PCC counters (FIE self-disables) only if we are
  building a PCCT anyway, since PCC needs a mailbox channel to parse at all.

## 1. How often Linux reads the counters (source)

Sources: torvalds/linux `v6.19` (the F44 guest's 6.19.10), `v7.2` (F44 `updates` ships
7.2.11), and `master` (7.3-rc, fetched 2026-10-19). `cppc_cpufreq.c` in v7.2 differs from master
in one line (the `get_rate` desired-perf fallback), so "master" below also means 7.2.

**Rate: once per scheduler tick, per busy CPU.** The driver registers
`cppc_scale_freq_tick()` as the per-CPU scale-freq source
(`drivers/cpufreq/cppc_cpufreq.c` v6.19:158 / master:183 → `topology_set_scale_freq_source`),
which `topology_scale_freq_tick()` calls from the scheduler tick
(`drivers/base/arch_topology.c` v6.19:123-128). Guest config (`/boot/config-6.19.10-300.fc44.aarch64`):
`CONFIG_HZ=1000`, `CONFIG_NO_HZ_FULL=y` with no `nohz_full=` on the command line, so the tick stops
only on idle CPUs: **1000 samples/s on every busy vCPU**, ~0 on an idle one.

**What one sample costs in register reads** (`cppc_get_perf_ctrs`):
- v6.19 (`drivers/acpi/cppc_acpi.c` v6.19:1475-1541): DELIVERED_CTR, REFERENCE_CTR, **and
  REFERENCE_PERF (or NOMINAL_PERF) every sample** (:1519-1521), plus CTR_WRAP_TIME if supported
  (:1529-1530). An Integer entry costs nothing (`cpc_read` returns `int_value`, v6.19:1010-1013);
  a SystemMemory entry is a `readl/readq_relaxed` of an ioremap (v6.19:1043, :1049-1062) = one exit.
  So with only the two counters as registers: **2 exits per sample**.
- master/v7.2 (`cppc_acpi.c` master:1678-1740, v7.2:1540-1609 with the two reads inline): DELIVERED + REFERENCE (master via `cppc_read_fb_ctrs`,
  :1622-1647) + CTR_WRAP_TIME if supported; REFERENCE_PERF is cached in `perf_caps` instead
  (`cppc_cpufreq.c` master:83). Again **2 exits per sample** with the counters as the only registers.

**From which context:**
- v6.19: tick → `irq_work_queue` (`cppc_cpufreq.c` v6.19:113-122) → `kthread_queue_work` on ONE
  unbound `cppc_fie` kthread with SCHED_DEADLINE (v6.19:110, :187-230), which does the reads for
  every CPU. So each busy-CPU tick also costs a self-IPI (irq_work) and a cross-CPU kthread wakeup —
  for every counter layout that is not PCC, including all-Integer.
- master/v7.2: non-PCC counters are read **directly in the tick** on the local CPU
  (`__cppc_scale_freq_tick`, master:69-104, `cppc_sftd` :106-109); the irq_work+kthread path is kept
  only for CPUs whose counters are in PCC (master:127-148, :163-167).

**When FIE turns itself off:**
- `fie_disabled` module param (`cppc_cpufreq.c` v6.19:31-39, master same) —
  `cppc_cpufreq.fie_disabled=1` on the guest command line; a guest-side knob, not a firmware one.
- Counters in PCC: if any CPU's DELIVERED/REFERENCE/CTR_WRAP (v6.19 also REFERENCE_PERF) is a PCC
  register, `cppc_freq_invariance_init` sets `fie_disabled` (v6.19:204-210, master:250-266, using
  `cppc_perf_ctrs_in_pcc`, `cppc_acpi.c` v6.19:1435-1451 / master:1612-1666). Applies only while the
  param is unset.
- kthread creation / SCHED_DEADLINE failure (v6.19:215-229).
- Arch counters win: `topology_set_scale_freq_source` never replaces a `SCALE_FREQ_SOURCE_ARCH`
  (AMU) source (`arch_topology.c` v6.19:82).

**Constant (Integer) counters do not disable FIE, they neuter it.** `_CPC` accepts an Integer for
any entry (`cppc_acpi.c` v6.19:776-778). FIE registers, reads two constants each tick (0 exits),
`cppc_perf_from_fbctrs` sees zero deltas and returns 0 (v6.19:711-712, master:780-781), the tick
returns early (v6.19:90-91, master:86-87) and `arch_freq_scale` stays at 1024. Because the CPUs are
now in `scale_freq_counters_mask`, cpufreq's own requested-frequency invariance is suppressed too
(`topology_set_freq_scale` returns early, `arch_topology.c` v6.19:148-149). `scaling_cur_freq` /
`.get` falls back to desired perf (`cppc_cpufreq_get_rate`, master:831-841). In v6.19 the per-tick
irq_work + kthread wakeup still runs.

**Other readers (on demand only):** `.get` (`cppc_cpufreq_get_rate`) takes two samples 2 µs apart
(master:786-799) when something reads `cpuinfo_cur_freq`/`scaling_cur_freq`; sysfs
`acpi_cppc/feedback_ctrs` (`cppc_acpi.c` master:269-283). Neither is periodic.

## 2. Exit cost of one MMIO read on HVF

Measured 2026-10-19, M1 mini. `mmio_cost.ko mode=measure`: 20000 back-to-back `readl_relaxed`
of virtio-mmio MagicValue (`0x0a004000`, read-only, no side effects) with preemption off, then the
same count of RAM reads; 7 repeats per load, 5 loads (35 samples), CPU wherever insmod ran.

| worker | MMIO ns/read: median (min–max) | RAM ns/read |
|---|---|---|
| release, `/Applications/Limina.app` (Oct 16) | **1236** (1224–1518) | 0.31–0.37 |
| debug, staged `target/debug` (Oct 10) | 1925 (1895–2235), 21 samples | 0.31–0.37 |

The first sample of a load is the slow one (1518 / 2235 ns); the rest sit within ±2 %. Reading an
address no device decodes the same way (pl061 at `0x0a003000`, returns 0) costs the same 1231–1253
ns: the price is the exit round trip, not the device model. A real counter would be a `readq`;
it is still one exit. The A/B below gives the effective cost under load: (tick-mmio-x10 − tick-ram)
= 3.16 % for 20 reads/ms ⇒ ≈ 1.6 µs per read, and (tick-mmio − tick-ram) =
0.30 % for 2 reads/ms ⇒ ≈ 1.5 µs — consistent with each other, ~30 % above the quiet loop.

Why a kernel module and not `/dev/mem`: the guest has `CONFIG_STRICT_DEVMEM=y` and
`CONFIG_IO_STRICT_DEVMEM=y`, so every MMIO range a driver has claimed (all of them here) is
refused to `/dev/mem` without `iomem=relaxed`. A stage-2 data abort is the same exit from EL1 or EL0.

## 3. Throughput impact

Measured 2026-10-19, M1 mini, release worker. `bench 10` runs one pinned xorshift loop per vCPU
(all 4 busy, so every vCPU ticks at 1000 Hz — the worst case for a per-tick cost). Arms load
`mmio_cost.ko` in a mode that reproduces a FIE shape on every CPU with a 1 ms hrtimer (hard-irq,
pinned); `-ram` arms do the same reads from RAM (same interrupt/wakeup cost, no exit). 10 rounds,
arm order shuffled per round; "paired" is each run against the same round's base run.
Raw lines: `ab-2026-10-19-release.txt`; table: `summarize.py`.

| arm | what it reproduces | median Miter | min–max | Δ median | paired Δ median (min…max) |
|---|---|---|---|---|---|
| base | no module | 19790.8 | 19768.0–19806.3 | — | — |
| tick-ram | timer only | 19783.1 | 19776.4–19793.2 | −0.04 % | −0.04 % (−0.12…+0.06) |
| tick-mmio | **master/7.2 FIE, MMIO counters**: 2 reads in the tick | 19722.6 | 19696.9–19733.6 | **−0.34 %** | **−0.37 %** (−0.51…−0.22) |
| kthread-ram | v6.19 FIE machinery, reads free | 19660.0 | 19621.1–19671.9 | −0.66 % | −0.66 % (−0.89…−0.53) |
| kthread-mmio | **v6.19 FIE, MMIO counters**: irq_work → DL kthread → 2 reads | 19621.4 | 19419.3–19637.9 | **−0.86 %** | **−0.88 %** (−1.85…−0.66) |
| tick-mmio-x10 | sensitivity check: 20 reads per tick | 19157.8 | 19137.5–19214.7 | −3.20 % | −3.18 % (−3.33…−2.92) |

Every module arm held its rate: 10989–11021 fires per CPU over the ~11 s a module was loaded
(1 kHz), with exactly 2 (or 20) reads per fire. Base run-to-run range is 0.19 %, so the
tick-mmio and kthread arms are separated from base and from each other; tick-ram is not.

Reading: the exit cost of MMIO counters is ≈ 0.3 % of a fully busy guest (tick-mmio − tick-ram);
the v6.19 path's extra ≈ 0.6 % is its irq_work self-IPI and cross-CPU kthread wakeup per tick,
which do not depend on where the counters live. A partly idle guest pays proportionally less
(no tick, no read on an idle vCPU).

Host load during all runs: the mini's "LiminaOS Builder" VM (8 vCPU / 12 GiB, the installed app)
was up and idle (~2 % CPU), and the spike guest was switched to `multi-user.target`.
The host's vCPU scheduling band (`[VCPU-RT]` lines in the worker log) was active as shipped.

## 4. AMU / FFH

- Guest (measured 2026-10-19, M1 mini): `ID_AA64PFR0_EL1` = `0x1101000011110011` both raw
  (`mrs` at EL1 from `mmio_cost.ko`) and sanitised — **AMU field (47:44) = 0**. No
  "Activity Monitors Unit (AMU)" line in `dmesg` (the cpufeature that would print it is
  `arch/arm64/kernel/cpufeature.c` v6.19:2688). Userspace cannot see this field at all (it is
  `FTR_HIDDEN`, cpufeature.c v6.19:292); `/sys/devices/system/cpu/cpu0/regs/identification/` holds
  only `midr_el1`, `revidr_el1`, `aidr_el1`.
- Host: `sysctl hw.optional.arm` on the M1 mini (macOS 27.0.1) has 81 keys and none for AMU.
  Whether M1 silicon implements FEAT_AMUv1 is **not established**; HVF exposes 0 either way.
- Consequence: arm64's `cpc_ffh_supported()` is true only if some CPU has AMU
  (`arch/arm64/kernel/topology.c` v6.19:361-376), and FFH counters are read only as AMU
  `corecnt`/`constcnt` (v6.19:381-400). With AMU absent, an FFH entry fails the **whole** `_CPC`
  parse (`cppc_acpi.c` v6.19:839-843 / master:922-926, `goto out_free`) — not a degraded FIE but no
  `_CPC` at all, taking CPPC capacities with it. Even if HVF let the VMM set the ID register
  (`hv_vcpu_set_sys_reg` on ID regs: **unconfirmed**), AMU counter reads on hardware without AMU
  UNDEF to the guest's EL1 rather than trapping to the VMM. **FFH is not an option on this host.**

## 5. Variant: `_CPC` for capacities only + `qemu,virtual-cpufreq` for DVFS/FIE

**What virtual-cpufreq costs (the DT machine with `--cpufreq`, off by default).** Source
`drivers/cpufreq/virtual-cpufreq.c` master (v6.19 differs only in `get_freq_info`'s table setup):
- FIE: `virt_scale_freq_tick` does **one** `readl_relaxed(cur_perf)` per tick per busy CPU, in the
  tick itself, no irq_work (:70-85), registered at :224. So 1000 exits/s per busy vCPU — half of
  CPPC's two counters; by the per-read cost above ≈ 0.15–0.17 % of a fully busy guest
  (interpolated from §3, **not measured as its own arm**).
- DVFS: one `writel_relaxed(set_perf)` per frequency request via `fast_switch` (:92-105). The driver
  sets no `transition_latency`, so cpufreq's rate limit is `USEC_PER_MSEC`
  (`drivers/cpufreq/cpufreq.c` master:524-536): at most one write per ms per policy. That schedutil
  only writes when the requested frequency changes (so ~0 at a steady load) is from memory of
  `cpufreq_schedutil.c`, **not verified in this spike**.
- Everything else (perftbl, perf_domain) is read once at probe/policy init (:107-113, :132-154,
  :276-284).

**Capacities come from `_CPC` regardless of which cpufreq driver binds.**
`acpi_processor_driver_init` (`drivers/acpi/processor_driver.c` master:242-280, built in on F44:
`CONFIG_ACPI_PROCESSOR=y`) calls `acpi_processor_init_invariance_cppc()` (:276) →
`topology_init_cpu_capacity_cppc()` (`drivers/base/arch_topology.c` master:327-376, wrapper
:378-381), which needs only `acpi_cpc_valid()` and reads `cppc_get_perf_caps` (highest / nominal /
lowest — capability registers, never the feedback counters), sets `cpu_scale` from `highest_perf`
and `capacity_freq_ref`. On ACPI the cpufreq-policy capacity path is not even registered
(`register_cpufreq_notifier` returns `-EINVAL` when `!acpi_disabled`, arch_topology.c master:429-438),
so `_CPC` is the *only* capacity source; if any CPU's `_CPC` fails to parse, all CPUs fall back to
1024 (:355-357).

**And the counters are never polled when virtual-cpufreq registers first.** `cppc_cpufreq_init`
(`cppc_cpufreq.c` master:1055-1070, v6.19 same shape) runs `cppc_freq_invariance_init()` *before*
`cpufreq_register_driver`, which fails with `-EEXIST` when a driver is already registered
(`cpufreq.c` master:2911, :2940-2942); it then calls `cppc_freq_invariance_exit()`. The per-CPU FIE
hook is installed only from the policy `init` callback (`cppc_cpufreq_cpu_fie_init`), which never
runs. On v6.19 the `cppc_fie` kthread is created and destroyed during that failed load; nothing
recurs. Remaining counter readers: sysfs `acpi_cppc/feedback_ctrs`, on demand only.

**The catch: who registers first is not fixed.** On F44 both are modules
(`CONFIG_ACPI_CPPC_CPUFREQ=m`, `CONFIG_CPUFREQ_VIRT=m`) and both autoload — `cppc_cpufreq` on the
ACPI processor HID (`cppc_cpufreq.c` master:1085-1090), virtual-cpufreq on its `of:` compatible
(via PRP0001 under ACPI, per the coordinator's source check — **not re-verified here**). The order is
udev coldplug's; **unconfirmed** which wins, and it may differ boot to boot. If `cppc_cpufreq` wins,
virtual-cpufreq is the one that fails `-EEXIST` and CPPC FIE polls whatever the counters are.
So the counter layout must be safe in that case too:
- **PCC counters**: FIE self-disables (§1), no polling on any kernel. Costs a PCCT plus a PCC
  mailbox channel: `acpi_cppc_processor_probe` requests the channel at parse time
  (`cppc_acpi.c` master:983 → `pcc_mbox_request_channel` :650; v6.19:879, :567) and fails without
  it; the doorbell (an MMIO exit) is rung only by `.get`/sysfs reads, and by desired-perf writes if
  `cppc_cpufreq` is the driver.
- **Integer counters**: valid, 0 exits per tick on 7.2+, but FIE stays registered at 1024 and v6.19
  still pays the −0.66 % kthread machinery if `cppc_cpufreq` wins.
- **MMIO counters**: −0.34 % (7.2+) / −0.88 % (6.19) on a fully busy guest if `cppc_cpufreq` wins,
  and real counter-based FIE in exchange.
Avoiding the race outright needs something outside `_CPC` (e.g. not shipping one of the two
drivers' match for a VM); not explored here.

Two more `_CPC` requirements on arm64 independent of layout: SystemMemory registers with a
non-zero address need `_OSC` to confirm the CPPC flexible-address-space bit, because arm64 does not
override `cpc_supported_by_cpu()` (`cppc_acpi.c` v6.19:801-808, weak default :615); FFH needs AMU (§4).

## Method and files

- `boot.sh` — headless 4 vCPU / 4 GiB boot on the M1 mini (`LIMINA_WORKER_LAUNCH=spawn`,
  software-2D to a PNG, no audio/USB/battery) of a CoW clone of `piglit-kkq.raw` (stock F44,
  6.19.10-300). Defaults to the installed release app's binaries, used read-only.
- `mmio_cost.c` + `Makefile` — the guest module (needs `kernel-devel` for the running kernel) and
  `bench.c`. `insmod mmio_cost.ko` with `mode=measure|tick|kthread`, `nreads=`, `ram=1`, `addr=`;
  `measure` also prints ID_AA64PFR0_EL1.
- `ab.sh` — the shuffled 10-round A/B, run in the guest as `bash ab.sh 10 10`;
  `summarize.py ab-2026-10-19-release.txt` prints the table in §3.
- The guest ran `systemctl isolate multi-user.target` first. Release worker's parked bundle name
  was not resolved, so its exact revisions are unknown (app binaries dated 2026-10-16 13:36).
