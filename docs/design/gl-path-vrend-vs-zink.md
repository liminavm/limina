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

### …but "relocate" is the win, and most of them do not even relocate

Revised the same day, on the point that **we control the host and patching the guest is far more
troublesome** — so moving a patch guest→host is a gain, not a cost. The rule that follows: do not
import the zink series wholesale; establish for each whether the host build needs it at all.

The host zink-on-KK build today carries **exactly one** zink change — a driconf guard in
`zink_screen.c` (`d434b73a2c2`). It has none of 0001/0003/0004/0006/0014, and on it the desktop
*and* a sustained WebGL aquarium both run correctly on the vrend path (browser pid confirmed with
`GALLIUM_DRIVER=virgl`, mapping `libgallium` and no libvulkan — i.e. GL through virgl → vrend →
host zink, not venus). Per-patch, on preconditions rather than on "it works":

| patch | host verdict | basis |
|---|---|---|
| 0001 nullDescriptor emulation | **not needed** | KK advertises `.nullDescriptor = true` + `EXT_robustness2`/`KHR_robustness2` (`kk_physical_device.c:146,182,358`). The patch emulates the feature for drivers lacking it. |
| 0003 / 0004 semaphore-fd guards | **not needed** | KK advertises `.KHR_external_semaphore_fd = true` (`:195`). The patches guard its *absence*. |
| 0006 kopper surface guard | **not needed** | Host zink is surfaceless (no kopper swapchain), and every `Create*SurfaceKHR` sits behind `VK_USE_PLATFORM_XCB/WAYLAND/WIN32`. |
| 0014 lost wakeup | **carried host-side** | See below. |

So four of the five simply die with the guest zink deployment rather than moving.

**0014 is carried deliberately, without a reachability proof.** The buggy code is present
byte-for-byte in the host tree (unlocked `unflushed=false` at `zink_batch.c:657`, un-mutexed
`cnd_broadcast` at `:846`, and a waiter that neither loops nor rechecks its predicate whose
timedwait uses an epoch-absolute `{0, 10000}` timespec, so trywait has never actually waited). The
hazardous branch is `else //multi-context` — one zink context waiting on another's unflushed batch,
then `cnd_wait` indefinitely. vrend creates a zink context per guest context, so it is structurally
reachable.

A probe on that branch was built and then abandoned on purpose: **a race cannot be disproven by not
observing it**, so a quiet probe run would have bought false confidence. The fix is small and
correct-by-inspection, so it is carried. Applied to the host branch as `47308c0f026` and pinned in
`third_party/manifest.toml`.

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

**BLOCKER found 2026-08-04 — vrend rendering corrupts under sustained GL load.** On the
virgl/vrend path, content progressively corrupts under load: wrong colors first, then structured
garbage, and eventually the whole compositor, which stays corrupted after the load stops. Silent —
no host-side error at onset. Reproduced after a single full glmark2 suite. Full write-up + three
reference images: `spikes/vrend-texture-corruption/RESULTS.md`. This must be fixed before vrend can
be considered for the compositor's GL path, and it outweighs any perf number.

*(An earlier revision of this section blamed `--display-resolution`. That was wrong — resolution and
load-duration had moved together in the observations, and running the same heavy load at the
supposedly-safe resolution reproduced it immediately. Whether zink-on-venus survives the same load
is **not yet tested**, so "vrend-specific" is not proven.)*

**The GL A/B numbers below are VOID.** A first pass measured virgl 2498 vs zink 1273 on the full
glmark2 suite — but every run in both arms was composited through the corrupted virgl pipeline (the
A/B varied only the *client's* driver, never the compositor's), so it measured a broken envelope.
Do not cite those figures. (Also correcting an earlier claim in this doc: glmark2 *is* installed on
this image.)

**Performance is a re-measure, not a lookup.** The record is genuinely mixed:
`docs/perf/venus-cmdstream-overhead.md:3` says the tier battery showed *virgl/vrend beating
zink-on-venus GL*, while the 07-29 `limina-virgl-vrend-perf` finding has venus winning or tying
every guest cell and explicitly flags that loose-era numbers need re-judging. Neither settles it.
`vkmark` is the wrong instrument here (it is Vulkan → venus on both arms). ~~`glmark2` is not
installed on this image~~ — it is (`glmark2-es2-wayland`); that earlier claim was wrong. A GL A/B
needs to be set up deliberately, and must control the **compositor's** GL path, not only the
client's — the first attempt did not, which is why it produced void numbers.

## Strategic read

The argument for the switch is not performance and not capability — it is **owning less**. But the
zink relocation means the saving is smaller than it first looks, and it comes with a cost the
optimistic framing hides: the compositor's GL would then depend on the host zink build, which is
the *less* exercised of our two zink deployments and carries a known unfixed deadlock.

A defensible ordering, if this is pursued:

1. Delete the stale selector lines (independent, zero-risk, do it regardless).
2. ~~Land mesa 0014 in the host zink-on-KK build~~ — **DONE** (`47308c0f026`, pinned).
3. Explain the modifier-probe inconsistency, then re-test which patches actually go quiet.
4. Only then run a real GL A/B and decide the tier on measured numbers.

Revised strategic read: the earlier framing ("the saving is smaller than it looks") was weighing the
wrong thing. Guest patches are the expensive kind — they need an RPM respin, an image refresh, and a
delivery pass; host patches are a rebuild we control end to end. Four of five zink patches retire
outright with guest zink, and the fifth moves to the tier where carrying it is cheap. The cost side
of this trade is smaller than first stated.
