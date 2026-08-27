# A 4 KiB stage-2 granule is available, and it removes the 4 KiB-guest blob wall

`hv_vm_config_set_ipa_granule(config, HV_IPA_GRANULE_4KB)` — macOS 26.0+, declared in
`Hypervisor.framework/Headers/hv_vm_config.h:111-143` — sets the granule of the **stage-2**
(guest-physical → host) translation. Set it to 4 KiB and `hv_vm_map` accepts 4 KiB-aligned guest
addresses and 4 KiB sizes.

**The default on this host is 16 KiB**, so every VM we have ever booted has been running with the
coarse granule and had no way to express a 4 KiB-granular guest layout.

## Measured (M1 Max, macOS 26.5, `./run.sh`, full output in `out.txt`)

| case | 16 KiB granule | 4 KiB granule |
|---|---|---|
| A — everything 16 KiB-aligned (control) | OK | OK |
| B — guest addr 4 KiB-aligned, **not** 16 KiB-aligned, 1 MiB | `0xfae94003` | **OK** |
| C — two **separate** host allocations at adjacent 4 KiB guest addresses | `0xfae94003` | **OK** |
| D — a 4 KiB-sized mapping | `0xfae94003` | **OK** |

`0xfae94003` is `HV_BAD_ARGUMENT`. Case B is the reported failure reproduced exactly, down to the
shape of the addresses:

```
hv_vm_map failed: ret=0xfae94003 host=0x14b314000 guest=0x280021000 size=0x100000
                  (host%16k=0 guest%16k=4096 size%16k=0)
```

## Why case C is the one that matters

B alone only shows that a misaligned guest address is accepted. C shows that **two independent host
allocations can be presented to the guest as contiguous across a 4 KiB boundary** — which is
precisely two adjacent virtio-gpu blobs, allocated separately by Metal, packed back to back by the
guest's arena. That is the property the entire workaround family existed to manufacture:

- rounding blob sizes guest-side, so no two blobs ever share a host page;
- pooling host-visible memory into one mapped heap, so placement is arithmetic instead of
  translation;
- inflating reported memory requirements, so the guest's offsets land on a coarse lattice.

All three were downstream of one assumption: that the stage-2 granule is pinned to the host page
size. It is configurable, and none of them are needed to map a 4 KiB guest's blobs.

## Confirmed on a booted guest: venus comes up on a stock 4 KiB Fedora

`Fedora-Workstation-44.accessible` (stock kernel `6.19.10-300.fc44`, 4 KiB pages, stock
`mesa-vulkan-drivers-26.0.3-4.fc44`) — **no guest components of any kind**. One clone, both arms in
sequence, identical but for `--ipa-granule 4k`:

| | host default (16 KiB) | `--ipa-granule 4k` |
|---|---|---|
| guest dmesg, `RESOURCE_MAP_BLOB` (`0x208`) → `ERR_UNSPEC` (`0x1200`) | repeated | **none** |
| `vulkaninfo --summary` | `vkCreateInstance failed with ERROR_OUT_OF_HOST_MEMORY` | `Virtio-GPU Venus (Apple M1 Max)`, `driverName = venus` |
| `vkcube --c 120` | exit 1, no Vulkan device | **exit 0** |
| worker log | — | `stage-2 IPA granule set to FourK`, 0 map failures |

The control's `ERROR_OUT_OF_HOST_MEMORY` is the usual venus disguise for "could not get a buffer",
and the guest kernel names the buffer: the blob map is refused, so the ring never exists. Which is
what makes this the mechanism and not merely a correlation — the failing command in the control is
the exact one the granule governs.

**So venus on the stock tier was never a venus problem.** It needed a host setting, not a guest
kernel, not a DKMS module, not a Mesa patch.

## Still owed before relying on it

- **The cost.** A 4 KiB granule means roughly 4× the stage-2 entries and more TLB pressure for the
  **whole** VM, not just GPU memory. It is a global setting bought for one subsystem, so it needs an
  A/B on the perf battery before it becomes the default.
- **Interactions.** Free-page reporting and `MADV_FREE_REUSABLE` operate on host addresses and
  should be unaffected, but a finer unmap granularity may change what the balloon can return from a
  4 KiB guest — worth measuring rather than assuming, in either direction.

## Trap

Cases are spaced 1 MiB apart deliberately. An **overlapping** `hv_vm_map` returns `HV_ERROR`
(`0xfae94001`), not `HV_BAD_ARGUMENT` — one digit apart, and it reads exactly like a granule
refusal. The first run of case C failed that way because case B's 1 MiB mapping ran through it.
