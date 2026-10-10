# The debug port: which host build is this guest running on?

Every limina guest has a virtio-serial port named **`org.limina.debug.0`**. A process inside the
guest writes a request to it and the host answers with the build stamp of the `limina` that is
running the VM, plus facts about this launch. It works on a **stock guest with nothing of ours
installed**: the port is a plain named port on the guest's virtio-console device, so a stock
kernel exposes it as `/dev/virtio-ports/org.limina.debug.0`.

It exists so a test harness running inside a guest can attribute a result to the host build that
produced it, without anyone copying a hash by hand.

**Its answers are off by default.** The port is always there, but until the VM's `debug-port`
lever is on, every request gets `error=disabled` and nothing else (see *Disabled* below).

Code: the worker attaches the port (`crates/limina-vmm/src/krun/console.rs`, `Ports::debug`); the
supervisor answers (`crates/limina/src/debug_port.rs`), started from `supervisor::spawn_worker`, so
every worker launch has it — flat and managed, windowed and headless, after a guest reboot and
after a resume. Test: `crates/limina-test/tests/l2_debug_port.rs` (boots with the answers off,
checks the disabled answer and the helper's exit 4, turns them on at runtime, then reads the
identity before and after a reboot).

## Reading it

As **root** (stock udev leaves `/dev/vport*` mode 0600), with bash and nothing else:

```bash
exec 3<>/dev/virtio-ports/org.limina.debug.0
echo identity >&3
on=; while IFS= read -r -t 10 l <&3; do
  case $l in format=*) on=1;; esac; [ -n "$on" ] || continue
  [ "$l" = . ] && break; echo "$l"
done
exec 3>&-
```

The same, packaged: `guest/limina-debug-identity` (copy it into the guest; bash only). With no
argument it prints the whole answer; with a key it prints that value
(`limina-debug-identity limina_git_rev`). Exit 1 = no port, 2 = no complete answer within
`LIMINA_DEBUG_TIMEOUT` seconds (default 10), 3 = no such key, 4 = the host has the answers
disabled (stderr carries the `enable=` hint).

Do not `cat` the port. It never reports end-of-file (the host end stays open for the life of the
VM), and nothing arrives until a request is written, so a bare `cat` blocks forever.

## Protocol

**Request/response.** The guest writes one request per line; the host answers every non-blank
line, in order. Requests:

| request | answer |
|---|---|
| `identity` | the identity fields below |
| `help` | `requests=<space-separated list>` |
| anything else | `error=<message>` |

The host cannot push an answer when the guest opens the port, because it never learns of the
open: libkrun handles the guest's `VIRTIO_CONSOLE_PORT_OPEN` internally and marks every port
host-connected at `PORT_READY` (`third_party/libkrun/src/devices/src/virtio/console/device.rs`,
the `PORT_READY` and `PORT_OPEN` arms). A blob written at spawn would be read by the first opener
only, and every later reader would block. Answering requests has neither problem.

**Framing.** An answer is `key=value` lines. The first is always `format=<n>`; the last is a line
holding a lone `.`, the only line in an answer without an `=`. A value is the rest of its line
(it may contain spaces, never a newline). A reader must:

- **skip everything before the first `format=` line.** The port is a byte stream that outlives
  its readers: one that closed in the middle of an answer leaves the tail for the next opener.
- **stop at `.`**, rather than at end-of-file, which never comes.
- **ignore keys it does not know.** New keys are appended without a format change; `format` is
  bumped only for a change an existing reader would misread.

`key=value` rather than JSON because the reader is a stock guest, possibly one without `jq` or
Python, and bash's `read`/`case` parse this with nothing else installed.

## Disabled (the default)

The answers are gated by the supervisor's `debug-port` lever (`debug_ctl::DEBUG_PORT`). Turn it
on by starting the VM with `LIMINA_DEBUG_PORT=1`, by ticking `debug-port` under *Harness Access*
in the window's Debug menu, or with `limina debug <vm> lever debug-port on`. While it is off,
every non-blank request — `identity`, `help`, an unknown one, an over-long one — gets exactly:

```
format=1
error=disabled
enable=start the VM with LIMINA_DEBUG_PORT=1, tick debug-port in the window's Debug menu, or run `limina debug <vm> lever debug-port on`
.
```

No build, host or launch fact is in it, `help` included. A reader that only looks for `error=`
sees an error, as it would for any refused request.

- **The port stays on the bus regardless.** The device set must not depend on the setting: a
  snapshot restores only onto the device set it was taken with, and a VM suspended with the lever
  one way must resume with it the other.
- **Read per request.** A toggle applies to the next request, on the same open port. The lever
  lives in the supervisor, so it holds across guest reboots and a resume from the parked window
  (each a new worker), not across a fresh `limina start`.
- **The host-side copy is unaffected.** The `limina: identity …` lines below go to the worker log
  on the host at every launch, on or off.
- **What it protects.** It keeps a guest from learning the host's build, OS and model unless
  someone asked for that. It is not a boundary against host code of the same user, which can turn
  the lever on over the debug socket.

## The `identity` answer (format 1)

| key | value |
|---|---|
| `format` | `1` |
| `limina_version` | the workspace version |
| `limina_git_rev` | limina's source revision (12 hex digits), `unknown` outside a checkout |
| `limina_build_date` | build date, UTC (the minute for a release bundle, the day for a dev build) |
| `dep.<name>` | full revision of each host dependency this build carries: `libkrun`, `virglrs`, `imago`, `kosmickrisp`, `edk2` (`crates/limina/build.rs` resolves them) |
| `launch_id` | a random UUID, fresh for every worker launch: cold boot, guest-reboot relaunch, and resume each get a new one |
| `resumed` | `yes` when this launch restored a suspended VM, else `no` |
| `vm_kind` | `managed` (`limina start <vm>`) or `flat` (`limina --disk …`) |
| `vm` | the managed VM's name, or the first disk's file name |
| `boot` | `efi` (firmware, the guest's own bootloader) or `kernel` (`--kernel` direct boot) |
| `gpu` | `coexist` (software-2D + venus/vrend), `software-2d`, or `none` (no display device) |
| `display` | `window`, `capture` (`--display-capture`), or `none` |
| `display_pool` | scanouts on the virtio-gpu; `0` without one |
| `cpus` | vCPUs |
| `ram_mib` | guest RAM as allocated (the top of a `--memory` range) |
| `host_os` | `macOS <version>` |
| `host_model` | the Mac's model identifier (`hw.model`) |
| `supervisor_pid` | the `limina` process |
| `worker_pid` | this launch's `limina-vmm` process |

There is deliberately **no dirty-tree flag**: `crates/limina/build.rs` explains why one baked in
at compile time would be wrong in both directions. Whether a bundle was cut from a dirty tree is
recorded where it can be observed, by `scripts/park-bundle.sh`.

## The same fields on the host

At every worker launch the supervisor prints the identity to stdout, one field per line:

```
limina: identity format=1
limina: identity limina_git_rev=0123456789ab
…
```

Printed rather than logged, so it is in the worker log at the default `warn` filter
(`grep '^limina: identity '`). A guest-side answer and a host-side block with the same
`launch_id` describe the same launch.

## Device topology

The port is appended after `com.redhat.spice.0` and `org.qemu.guest_agent.0` on the one
virtio-console device, so the order is fixed across launches. The device's snapshot topology
counts its ports, so a VM suspended by a build without this port is refused on resume by a build
with it (the ordinary "different set of devices" refusal), and keeps its suspended session.
