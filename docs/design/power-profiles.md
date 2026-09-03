# Power profiles: mostly already working, missing only the host half

Status: **live end to end for CPU reclaim; the worker levers are designed and not built.** The
profile flows guest daemon → `limina-agent` → host policy; `power-saver` reclaims vCPUs and a flip
back restores them at once. `little`/`rt_band` still need the worker channel (§7).

## 1. What already works, measured

On a stock guest with `--cpufreq`, the GNOME Power Mode toggle reaches the guest scheduler:

```
net.hadess.PowerProfiles         owned by tuned-ppd.service
Profiles: power-saver, balanced, performance     Driver: "tuned"

profile → governor, both cpufreq policies:
  performance  → performance
  power-saver  → schedutil
  balanced     → schedutil
```

Fedora 44 does **not** ship `power-profiles-daemon`; `tuned-ppd` owns the same two D-Bus names
(`net.hadess.PowerProfiles`, `org.freedesktop.UPower.PowerProfiles`) and is backed by TuneD
profiles, mapped in `/etc/tuned/ppd.conf`: `power-saver→powersave`, `balanced→balanced`,
`performance→throughput-performance`. Because TuneD profiles are tuning bundles rather than
hardware drivers, all three profiles are offered unconditionally, on any machine.

**The virtual cpufreq device is what gave the toggle teeth.** TuneD's cpu plugin writes
`scaling_governor` directly; before the device existed there were no cpufreq policies for it to
write to, and the profile changed nothing. This is the sense in which the profile was previously
"unbacked", and it is now backed on a stock guest.

## 2. The part that is still theatre

Changing the governor changes nothing real. Our device only echoes back the frequency the guest
requests, and no host code acts on the number. The guest has no cpuidle driver either
(`current_driver = none`), so there is no idle state to deepen. Every joule is spent on the host.

So the profile is a **statement of user intent that we should relay**, with the backing in host
policy — and the levers already exist, built for EAS packing:

| profile | little vCPUs | RT scheduling band | CPU reclaim |
|---|---|---|---|
| `power-saver` | `QOS_CLASS_BACKGROUND` | off | `Moderate` |
| `balanced` | `QOS_CLASS_BACKGROUND` | `rt+dyn` (default) | off |
| `performance` | promoted (`QOS_CLASS_UTILITY`) | `rt+dyn` | off |

`balanced` is today's defaults exactly, so a guest whose user never touches the toggle behaves as
it does now. Little vCPUs are on in `balanced` too — the asymmetry is the machine's normal shape,
not a power-saving mode — and only `performance` gives it up.

**A profile can never add or remove little vCPUs.** Capacity comes from `capacity-dmips-mhz` in
the device tree and `cpu_capacity` is `DEVICE_ATTR_RO` (`drivers/base/topology.c:229`), so the
split is fixed for the life of the VM. `performance` therefore promotes their host QoS instead,
and accepts that the guest still derates those CPUs and will under-load them — misfit migration
pulls big tasks off them and the load balancer weights them at their advertised capacity. That
unused headroom is the price of a topology that cannot be rebuilt at runtime, paid in the mode
that is latency-led anyway.

Balloon giveback is deliberately **not** in the table. The balloon has its own policy driven by
its own pressure signal, and two controllers writing one knob is how the oscillation classes in
`limina-balloon-oscillation` were born.

The relay is `limina-agent` watching the `ActiveProfile` property on the existing D-Bus name
(`guest/limina-agent/src/power.rs`: a background thread blocks on the property stream and
publishes into a cell the serve loop reads on its idle tick) and sending it over the vsock
control plane as `POWER_PROFILE`. No kernel work, no patched daemon, and the same code path works
whether the provider is `tuned-ppd` or `power-profiles-daemon`, since only the D-Bus interface is
involved. A guest with neither (no bus at all, even) reports nothing and retries twice a minute;
the host holds `balanced`. The watcher cost the agent its libc-only diet: zbus takes the static
binary from 493 KB to 1.18 MB plus one executor thread — accepted, since the alternative was a
D-Bus round trip on every idle tick forever.

Two properties the policy must hold, both learned from ballooning:

- **Hysteresis.** A profile flip must not thrash a vCPU's QoS class — changing a vCPU thread's
  scheduling band is exactly when the present path can lose its core.
- **Level-triggered, not edge.** Send the current profile, never a delta, so reconnect, reboot
  and snapshot restore resynchronise for free.

### 2.1 Why every VM now carries a vCPU policy

`VcpuPolicy::new()` used to return `None` for `CpuReclaim::Disabled`, and the control plane keys
the `vcpu` capability off that `Some`. Since capabilities are negotiated once at WELCOME, a VM
started with reclaim off would never be asked to report CPU pressure, and `power-saver` would
turn reclaim on to no effect — silently.

So a policy is now built for every VM with more than one vCPU, with `Disabled` meaning "floor
equals max", which the shrink guard (`online <= floor`) never passes. The cost is that every VM's
guest reports CPU pressure periodically, whether or not it will ever reclaim.

## 3. `performance` disables EAS, and that is correct

Measured, with `sched_debug` on:

```
performance : rd 0-3: Checking EAS: cpufreq is not ready
balanced    : pd_init: no EM found for CPU0
```

`throughput-performance` sets `governor=performance` with no fallback list, while `powersave` and
`balanced` both ask for `schedutil|…` and get schedutil. Since `cpufreq_ready_for_eas()` requires
schedutil, selecting `performance` turns energy-aware packing off at a gate *earlier* than the
energy model. Writing `/proc/sys/kernel/sched_energy_aware` is then refused with `EOPNOTSUPP`,
which is worth knowing before it is mistaken for a bug.

This composes correctly rather than accidentally: a user asking for performance is asking not to
be packed onto efficiency cores. Record it so that "EAS stopped working" is diagnosed as the
profile, not chased as a regression.

## 4. Host → guest is already half-live

`/etc/tuned/ppd.conf` carries `battery_detection=true` and a `[battery]` mapping of
`balanced=balanced-battery`. We mirror the host battery into the guest over virtio-i2c SBS, and
the guest sees it (`/sys/class/power_supply/sbs-0-000b`, `type=Battery`). So a host running on
battery should already shift the guest's profile with no work from us.

The direction we would add is a *proposal*, not a command — the guest desktop stays free to
ignore it, or we are fighting the user.

## 5. Two tiers, granularly

- **Stock guest, no components.** All three profiles, governor switching, battery-driven
  profile changes. What is missing is only the host response.
- **Agent installed.** The profile reaches host policy. That is the whole feature.
- **Agent installed, no profile daemon** (a guest with no GNOME). No source; the host holds
  `balanced`. A missing D-Bus name is normal, not an error.

## 6. A dead end worth not re-walking

`power-profiles-daemon` picks one driver from a fixed list (`src/power-profiles-daemon.c:152`)
and has no generic cpufreq driver — `ppd-driver-cpu.c` is an abstract base class, only Intel and
AMD pstate are concrete. Reasoning from that, the apparent route to a real backend is a module
registering a `platform_profile` handler; `platform_profile_register()` is `EXPORT_SYMBOL_GPL`
(`drivers/acpi/platform_profile.c:624`).

It is closed twice over. `platform_profile_init()` returns `-EOPNOTSUPP` when `acpi_disabled`,
and we boot the guest from a device tree — confirmed on the guest: `/sys/firmware/` holds
`devicetree, dmi, efi, fdt, qemu_fw_cfg` and no `acpi`, and there is no
`/sys/class/platform-profile/`. And it is moot regardless, because Fedora does not run PPD.

The lesson that generalises: **check which daemon actually owns the D-Bus name on the target
distro before designing against an upstream project's driver model.** The whole PPD driver
analysis was answering a question about a daemon this guest does not run.

## 7. What is built

The whole reclaim loop, verified on a real guest against tuned-ppd: the agent's watcher (§2), the
wire message (`POWER_PROFILE`, level-triggered; vocabulary shared through
`limina_proto::PowerProfileMsg` so the two ends cannot drift), the profile-to-policy mapping
(`crates/limina/src/power_profile.rs`), and the control plane applying **CPU reclaim** —
`power-saver` walks idle vCPUs down to the floor, and a flip back grows the machine at once
rather than waiting for the next pressure report. The agent advertises `powerprofile` in HELLO.

Not yet built: the worker half. `little` and `rt_band` need a QoS override on the vCPU thread's
`pthread_t`, which only the worker's own process holds, so they need a supervisor→worker channel —
the shape exists twice already (the balloon and display-resize control sockets: supervisor
allocates the path, worker binds it and wires it to the live handle), and the re-banding
mechanism itself already runs inside the worker (the `rt+dyn` sampler arms and disarms live
threads by mach port every 200 ms; only its *policy* is fixed at spawn, via `LIMINA_VCPU_SCHED`).
Until that lands a profile still moves reclaim, and `balanced` — what a guest reports unless its
user says otherwise — asks for today's defaults anyway.

## 8. Open

- The `balanced-battery` switch is expected but unverified — it needs the host actually on
  battery, which a test cannot arrange.
- `cpu.uclamp.max`, `cpu.uclamp.min` and `cpu.idle` exist on child cgroups
  (`CONFIG_UCLAMP_TASK_GROUP=y`), so `power-saver` could additionally clamp background slices.
  Unexplored, and worth measuring before adding: it is a second mechanism aimed at the same
  outcome as EAS packing.
