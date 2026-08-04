# The guest GL path: vrend/virgl vs zink-on-venus

Status: **measured 2026-08-04** (task #25). Prompted by the proposal to stop forcing zink in the
guest — "we do have virgl working with the vrend path … so we'd have vrend/virgl for GL and venus
for Vulkan native".

Vehicle: `vrend-arm.raw`, an APFS clone of `Fedora-Workstation-44.enhanced.test.raw`, booted with
the default EFI+venus recipe (`boot-enhanced-efi-kk.sh`). The whole experiment is **config-only** —
no rebuild of guest mesa was needed, which is itself the first useful finding.

## Result: the vrend path works, end to end

Forcing the session onto virgl:

```
# /etc/environment.d/90-limina-zink.conf
GALLIUM_DRIVER=virgl
MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu
VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.aarch64.json
```

gives, in the guest:

```
OpenGL ES profile renderer: virgl (zink Vulkan 1.3(Apple M1 Max (MESA_KOSMICKRISP)))
OpenGL ES profile version:  OpenGL ES 3.1 Mesa 26.1.4
OpenGL core profile version: 3.2 (Core Profile)
```

and Vulkan is untouched and still native venus:

```
deviceName = Virtio-GPU Venus (Apple M1 Max)
driverName = venus
```

`gnome-shell` came up on this config with **0 segfaults and 0 aborts** in the boot journal, and the
desktop was **human-confirmed rendering correctly** in the window (the log checks alone would not
catch a shear-class fault). So the proposal is viable: GL on vrend, Vulkan on venus.

Capability delta vs zink-on-venus: **GLES 3.1 either way** (which is what mutter/gnome-shell uses),
desktop **GL core 3.2 on virgl vs 3.3 on zink**. The GL-core drop is real but narrow.

## The correction that matters: dropping guest zink does not remove zink

Read the renderer string again — `virgl (zink Vulkan 1.3(… MESA_KOSMICKRISP))`. vrend's *host* GL
is served by zink-on-KK (`crates/limina-vmm/src/krun/mod.rs:87-96`; the boot script exports
`GALLIUM_DRIVER=zink` / `MESA_LOADER_DRIVER_OVERRIDE=zink` **for the host**).

So moving GL to vrend **relocates zink from the guest to the host**; it does not take it out of the
stack. The consequence for the patch series is the opposite of the optimistic reading:

- zink patches **0003 / 0004 / 0006 / 0014** do not retire. They move from "needed in the guest
  mesa RPM" to "needed in the host zink-on-KK build".
- 0014 (the lost-wakeup deadlock) is already flagged in `ledger/mesa.md` as a **HOST GAP** — "same
  unfixed code ships in the host zink-on-KK build". Routing all guest GL through host zink turns
  that latent gap into a load-bearing one. **That fix must land host-side before, not after, any
  such switch.**

The patches that could genuinely collapse are the venus-modifier ones (mesa 0010(b), virgl 0005,
kk 0002's trigger) — but only if the modifier traffic is in fact zink-kopper's, which is **not yet
established** (see below).

## Unrelated simplification found for free: the selector file is stale

With **both** zink selectors commented out, Mesa 26.1.4 still resolves to
`zink Vulkan 1.3(Virtio-GPU Venus …)` on its own. The file's own comment —

> `# limina enhanced tier: route GL through zink -> venus (virtio Vulkan), not llvmpipe.`

— describes a world that no longer exists: the fallback it was written to prevent is not what
happens today. **The two `GALLIUM_DRIVER=zink` / `MESA_LOADER_DRIVER_OVERRIDE=zink` lines are
redundant** and can be deleted independently of which GL path we choose. (Caveat: verified by
`eglinfo` under a bare env and by a clean gnome-shell boot with the lines removed; I did not
separately confirm which driver *gnome-shell itself* selected on that boot.)

## What is NOT established

**The modifier-traffic differential is inconclusive.** The `[LIMINA-VKRMODLIST]` probe logged
3 hits (`LIST count=1: 0x0 … fmt=44 2560x1440`), all inside the first boot
(16:05:58–16:06:05), and **zero** across the two later boots. That looks like "virgl removes the
modifier traffic" — but the middle boot ran **zink-on-venus** and also produced none. Since the
arm that should have reproduced it didn't, the probe count is not a trustworthy differential yet.
Do not cite it as evidence until that is explained.

**Performance is a re-measure, not a lookup.** The record is genuinely mixed:
`docs/perf/venus-cmdstream-overhead.md:3` says the tier battery showed *virgl/vrend beating
zink-on-venus GL*, while the 07-29 `limina-virgl-vrend-perf` finding has venus winning or tying
every guest cell and explicitly flags that loose-era numbers need re-judging. Neither settles it.
`vkmark` is the wrong instrument here (it is Vulkan → venus on both arms); `glmark2` is not
installed on this image. A GL A/B needs to be set up deliberately.

## Strategic read

The argument for the switch is not performance and not capability — it is **owning less**. But the
zink relocation means the saving is smaller than it first looks, and it comes with a cost the
optimistic framing hides: the compositor's GL would then depend on the host zink build, which is
the *less* exercised of our two zink deployments and carries a known unfixed deadlock.

A defensible ordering, if this is pursued:

1. Delete the stale selector lines (independent, zero-risk, do it regardless).
2. Land mesa 0014 in the host zink-on-KK build — required before host zink carries the desktop.
3. Explain the modifier-probe inconsistency, then re-test which patches actually go quiet.
4. Only then run a real GL A/B and decide the tier on measured numbers.
