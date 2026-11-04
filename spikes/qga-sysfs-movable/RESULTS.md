# Can the stock guest's qemu-ga set memory-hotplug onlining?

**Yes.** On stock Fedora 44, Enforcing, qemu-guest-agent's own domain (`virt_qemu_ga_t`) writes
all three onlining knobs, and the value takes. It works through `guest-file-open`/`guest-file-write`
on the sysfs file, and through `guest-exec` of `sh -c 'echo … > …'`. The policy grants it
outright: `allow virt_qemu_ga_t sysfs_t:file { append write };`, needing no boolean. What the
agent **cannot** do is leave a hook in the guest's configuration: creating a file in
`/etc/modprobe.d`, `/etc/udev/rules.d` or `/etc/tmpfiles.d` is denied, so nothing written through
QGA can act before the next boot's `virtio_mem` loads.

So the stock tier can be onlined movable with no guest change, as long as the host plugs the
hotplug region only **after** the agent has set the knob. A Split VM that plugs the whole region
as soon as `virtio_mem` loads cannot be fixed from QGA, because blocks already onlined to
ZONE_NORMAL stay there until they are unplugged.

## Measured 2026-11-04

Vehicle: `probe.sh` on a CoW clone of `Fedora-Workstation-44.stock.test.raw`, booted by the EFI
path with the image's own kernel. The guest ran kernel `7.2.15-200.fc44.aarch64`,
`qemu-guest-agent-10.2.2-1.fc44`, `selinux-policy-targeted-44.11-1.fc44`, Enforcing, with
`semodule -DB` (dontaudit off) for the run. The live agent runs as
`system_u:system_r:virt_qemu_ga_t:s0`. Its sysconfig blocks no RPCs, and every
`virt_qemu_ga_*` boolean is off.

The probe agent is a second `qemu-ga` started by `systemd-run`, so it enters the agent's domain
through the same transition as the real one; its `/proc/<pid>/attr/current` read
`system_u:system_r:virt_qemu_ga_t:s0`. It listens on a socket systemd creates and hands to it,
because the domain may not create one itself. That bind denial is a property of the probe's
transport, not of the question: `avc: denied { create } … tclass=sock_file`.

| Knob (all `sysfs_t`) | Before | Written | `guest-file-write` | `guest-exec sh` |
|---|---|---|---|---|
| `/sys/devices/system/memory/auto_online_blocks` | `online` | `online_movable` | took, no AVC | took, no AVC |
| `/sys/module/memory_hotplug/parameters/online_policy` | `contig-zones` | `auto-movable` | took, no AVC | took, no AVC |
| `/sys/module/memory_hotplug/parameters/auto_movable_ratio` | `301` | `400` | took, no AVC | took, no AVC |

| New file | Directory label | Result |
|---|---|---|
| `/etc/modprobe.d/limina-probe.conf` | `modules_conf_t` | `Permission denied`; `avc: denied { search } … tclass=dir` |
| `/etc/udev/rules.d/99-limina-probe.rules` | `udev_rules_t` | `Permission denied`; `avc: denied { search } … tclass=dir` |
| `/etc/tmpfiles.d/limina-probe.conf` | `etc_t` | `Permission denied`; `avc: denied { write } … tclass=dir` |

The file types the agent's own rules let it write (`sesearch -A -s virt_qemu_ga_t -c file -p
write`, leaving out the generic `domain`/`daemon` grants): `sysfs_t`, `sysctl_kernel_t`, `devicekit_var_run_t`, `systemd_passwd_var_run_t`, its own
`virt_qemu_ga_{t,var_run_t,log_t,tmp_t}`, and two behind booleans that are off (`ssh_home_t`,
`virt_qemu_ga_data_t`).

Fedora's default `auto_online_blocks` is `online` and its default policy is `contig-zones`. Under
those, every virtio-mem block lands in ZONE_NORMAL. `crates/limina-test/tests/virtio_mem.rs`
already shows that `online_movable` blocks unplug whole when kernel objects are pinned elsewhere.

## Running it

```
spikes/qga-sysfs-movable/probe.sh [image.raw]   # default: the stock test image
```

`qga_probe.py` is the guest half. It writes through the agent and only reads and restores values
from root's shell.
