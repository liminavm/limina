# Power profiles: a signal to the host, not a control in the guest

Status: **designed, not built.** The guest half is a D-Bus watcher in `limina-agent`; the host
half reuses the vCPU and balloon policy that already exists. No kernel work, and explicitly *not*
part of the energy-model DKMS module (§6).

## 1. The guest cannot save power, so it must not pretend to

A power profile in a normal desktop resolves to hardware: a lower CPU frequency, a shallower
turbo budget, a fan curve. Our guest has none of that. It has no cpuidle driver at all
(`cpuidle current_driver = none`, so WFI is the floor), no real DVFS, and a "frequency" we
invent — the virtual cpufreq device echoes back whatever the guest requests and no host code
acts on the number.

Every joule is spent on the host. The profile is therefore **a statement of user intent that we
relay**, and all backing lives in host policy. A guest-local implementation — switching the
governor, capping the invented frequency — would be theatre that changes nothing measurable.

This is also why the profile is worth having at all: the host *does* have levers, and they are
exactly the ones built for EAS packing (`--little-vcpus`, the vCPU QoS class, the real-time
scheduling band) plus the ones ballooning already owns.

## 2. Two profiles, which is all there are

`power-profiles-daemon` picks one driver from a fixed list — `fake`, `platform_profile`,
`intel_pstate`, `amd_pstate`, `placeholder` (`src/power-profiles-daemon.c:152`). There is **no
generic cpufreq driver**; `ppd-driver-cpu.c` is an abstract base class, and the only concrete CPU
drivers are Intel's and AMD's. So the virtual cpufreq device does not give PPD a backend and
never will: PPD falls to `placeholder`, which advertises `power-saver | balanced` and does
nothing (`src/ppd-driver-placeholder.c`).

Those two are the entire meaningful range for us. `performance` would be a no-op — by default we
already give the guest every vCPU at full QoS — so the profile PPD cannot offer is the profile we
have nothing to put behind. **The placeholder's limitation and ours coincide**, which is what
makes relaying it, rather than replacing it, the right design.

### 2.1 Why not a real PPD backend

Worth recording, because it looks available and is not. PPD's `platform_profile` driver would
give all three profiles, and `platform_profile_register()` is `EXPORT_SYMBOL_GPL`
(`drivers/acpi/platform_profile.c:624`), so a module registering a handler that forwards to the
host is the obvious design. It is closed:

```c
static int __init platform_profile_init(void)
{
	if (acpi_disabled)
		return -EOPNOTSUPP;
```

The subsystem refuses to initialise on a non-ACPI system. We boot the guest from a device tree —
that is how `virt-cpufreq` binds — so `acpi_disabled` is set, the class never registers, and
there is nothing for a module to register into. PPD reads only the legacy ACPI sysfs path
(`src/ppd-driver-platform-profile.c:19`), which lives under `acpi_kobj` and does not exist
either. Opening this route means patching a gate inside the subsystem's own init, which no DKMS
module can do, to buy a third profile that would do nothing.

## 3. The shape

```
GNOME Power Mode ──▶ PPD (placeholder) ──▶ D-Bus ActiveProfile
                                              │
                                    limina-agent watches
                                              │
                                        vsock control plane
                                              │
                              supervisor: VcpuPolicy + BalloonPolicy
                                              │
   host battery / Low Power Mode ─────────────┘  (proposes a profile back)
```

Both directions, and neither is authoritative over the other: the guest reports what the user
chose, the host may *propose* a change (we already mirror host battery state into the guest), and
the guest desktop remains free to ignore a proposal. Anything else would fight the user.

## 4. Host policy

Mechanism in the guest and the wire; policy in limina, as everywhere else.

| profile | little vCPUs | RT scheduling band | CPU reclaim | balloon giveback |
|---|---|---|---|---|
| `power-saver` | on (`QOS_CLASS_BACKGROUND`) | off | on | eager |
| `balanced` | off | `rt+dyn` (default) | off | default |

`balanced` is exactly today's defaults, so a guest that never selects anything behaves as it does
now. Every row is an existing, tested knob; the profile only chooses among them.

Two properties the policy must hold, both learned from ballooning:

- **Hysteresis.** A profile flip must not thrash the vCPU QoS class. A vCPU thread changing
  scheduling band is precisely when the present path can lose its core, so a change is applied
  once and not revisited on a timer.
- **Level-triggered, not edge.** The current profile is sent, never a delta, so a reconnect,
  a reboot or a snapshot restore resynchronises for free rather than needing replay.

## 5. Two tiers, granularly

Per the additive-capability rule — partial states are normal and must all work:

- **Stock guest, no components.** PPD shows two profiles and they do nothing, exactly as today.
  Nothing regresses; the feature is simply absent.
- **Agent installed.** The profile is relayed and backed. This is the whole feature — it needs
  no module, no custom kernel, and no patched PPD.
- **Agent installed, PPD absent** (a guest with no GNOME). No profile source; the host keeps
  `balanced`. The agent must treat a missing `net.hadess.PowerProfiles` as normal, not an error.

## 6. Its relationship to the energy model

They are separate. The DKMS module registers a synthetic energy model so EAS can pack a lightly
loaded guest onto the little vCPUs; this design decides *whether the little vCPUs exist at all*
for a given user intent. They compose — `power-saver` is the profile under which EAS packing
matters most — but neither needs the other, and the profile work should not wait for it.

## 7. Owed checks

Both are one command in a running guest, and both gate claims made above rather than merely
informing them:

- `ls /sys/firmware/acpi` — confirms `acpi_disabled` empirically, rather than inferring it from
  the guest booting via device tree.
- `powerprofilesctl list` — prints the bound driver, confirming `placeholder` and the two
  profiles it advertises.
