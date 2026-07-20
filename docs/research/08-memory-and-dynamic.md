# 08 — Memory overhead and dynamic memory

Scope: how libkrun sizes and maps guest RAM on macOS/HVF, what the in-tree
virtio-balloon device actually does (and what it only pretends to do), where host
and guest memory overhead come from, and the concrete options for giving a limina
VM a `min..max` dynamic memory range (free-page reporting, balloon inflate/deflate,
PSI autoballoon, virtio-mem). Ends with a recommendation and a proposed public
balloon API for libkrun. All libkrun claims below were read from local source in
this session and carry `path:line` citations.

---

## 1. What exists today

### 1.1 Setting guest RAM (public API)

The only public way to size guest memory is `krun_set_vm_config`:

```c
int32_t krun_set_vm_config(uint32_t ctx_id, uint8_t num_vcpus, uint32_t ram_mib);
```

- Declared at `/opt/homebrew/include/libkrun.h:98`; `ram_mib` documented at
  `libkrun.h:93` ("the amount of RAM in MiB").
- Rust entry: `krun_set_vm_config` at `src/libkrun/src/lib.rs:569`. It converts
  `ram_mib` to `mem_size_mib: usize` (`lib.rs:570`), builds a `VmConfig`
  (`vcpu_count`, `mem_size_mib`, `ht_enabled=false`, `cpu_template=None`), and
  stores it via `ctx_cfg.get_mut().vmr.set_vm_config(&vm_config)` (`lib.rs:587`).
- RAM is a single fixed scalar set at config time. There is **no** public
  function to pass a `min`/`max` range, set a balloon target, inflate/deflate,
  or trigger reporting. `grep -i 'balloon|madvise|mem' /opt/homebrew/include/libkrun.h`
  returns only the `ram_mib` hits above (confirmed).

### 1.2 The in-tree virtio-balloon device (exists, partly stubbed, NOT public)

Files: `src/devices/src/virtio/balloon/{device.rs (191 lines), event_handler.rs
(189 lines), mod.rs (30 lines)}`. The device is **not referenced from**
`src/libkrun/src/lib.rs` (`grep balloon` over lib.rs → no hits): it is wired into
the VMM but exposed by no `krun_*` C function.

Queues (`mod.rs:11`, `device.rs:16-24`): `NUM_QUEUES = 5`, `QUEUE_SIZE = 256`:

| Idx | Const | Purpose | Handler behavior |
|-----|-------|---------|------------------|
| 0 | `IFQ_INDEX` inflate | guest gives pages to host | **stub** — logs `"unsupported inflate queue event"`, drains eventfd, does nothing (`event_handler.rs:14-26`) |
| 1 | `DFQ_INDEX` deflate | host returns pages to guest | **stub** — logs `"unsupported deflate queue event"`, no-op (`event_handler.rs:28-40`) |
| 2 | `STQ_INDEX` stats | guest memory stats | **ignored** (`event_handler.rs:42-54`) |
| 3 | `PHQ_INDEX` page-hint | free-page hinting (migration) | **stub/unsupported** (`event_handler.rs:56-68`) |
| 4 | `FRQ_INDEX` reporting | free-page reporting | **implemented** → `process_frq()` (`event_handler.rs:70-84`) |

Advertised features (`device.rs:27-30`): `VIRTIO_F_VERSION_1` (32),
`VIRTIO_BALLOON_F_STATS_VQ` (1), `VIRTIO_BALLOON_F_FREE_PAGE_HINT` (3),
`VIRTIO_BALLOON_F_REPORTING` (5). Notably **NOT** advertised:
`VIRTIO_BALLOON_F_DEFLATE_ON_OOM` (2) or `VIRTIO_BALLOON_F_PAGE_POISON` (4).
Feature constants are in `mod.rs:15-21`.

Config space (`device.rs:34-43`, `#[repr(C, packed)]`):
`num_pages: u32` (host's requested balloon size), `actual: u32` (current balloon
size), `free_page_report_cmd_id: u32`, `poison_val: u32`. The device's
`write_config` **rejects** all guest writes (`device.rs:154-160`), and **no code
path ever updates `num_pages` or `actual`** — confirming there is no host-driven
inflate target today. The whole config is only ever read back as the zero-default.

**The only functional reclaim path** is free-page reporting, `process_frq()`
(`device.rs:74-112`):

```rust
while let Some(head) = queues[FRQ_INDEX].queue.pop(mem) {
    for desc in head.into_iter() {
        let host_addr = mem.get_host_address(desc.addr).unwrap();   // device.rs:91
        unsafe {
            libc::madvise(host_addr as *mut c_void,
                          desc.len.try_into().unwrap(),
                          libc::MADV_DONTNEED);                      // device.rs:96-102
        }
    }
    queues[FRQ_INDEX].queue.add_used(mem, index, 0);                // device.rs:106
}
```

Two consequences for our macOS target:
1. **It uses Linux-idiom `MADV_DONTNEED`** (`device.rs:100`). On Darwin
   `MADV_DONTNEED` exists but does **not** decrement the process footprint the
   way Linux does; the memory-accounting API macOS respects is
   `MADV_FREE_REUSABLE`/`MADV_FREE_REUSE` (used by libmalloc, reflected in
   `phys_footprint` and Activity Monitor). So reported pages do not actually
   reduce the footprint the user sees. **This is the single most important fix.**
   **[CONFIRMED by spike `spikes/balloon-madvise`, 2026-05-30, macOS 26.5 / M1
   Max.]** Measured on a 1 GiB `hv_vm_map`'d MAP_ANON region: `MADV_DONTNEED`
   leaves `phys_footprint` at the full ~1026 MiB (returns nothing); `MADV_FREE` is
   lazy (also no drop until pressure); only **`MADV_FREE_REUSABLE` drops it to
   ~2 MiB** — and it does so **even while the region stays `hv_vm_map`'d, with no
   `hv_vm_unmap` first** (`hv_vm_map` does not pin the pages). Note `madvise`
   returns `rc=0` in *all* cases, including the ones that free nothing — success
   ≠ reclaim. See the spike's `RESULTS.md` for the full 7-case matrix.
2. **Granularity**: `desc.len` comes straight from the guest in 4 KiB-page-based
   reporting batches. The macOS host page is **16 KiB**. `madvise` only releases
   whole, aligned host pages, so a 4 KiB or unaligned reported range frees
   nothing. Effectiveness on Apple Silicon is gated on the guest reporting in
   16 KiB-aligned runs (modern reporting batches at `pageblock_order`, typically
   2 MiB, so most ranges are large and aligned — but this must be measured).
   **Confirmed: both boot paths use 4 KiB guest pages** — libkrunfw bundles
   `linux-6.12.87` with `CONFIG_ARM64_4K_PAGES=y` (no 16K/64K, THP off;
   `config-libkrunfw_aarch64`), and the M1 EFI path runs Fedora's stock arm64
   kernel (also 4 KiB). So the mismatch is real on both paths.

   Because **we own the guest kernel**, the 4K↔16K mismatch is a menu, not a wall
   (cheapest → deepest):
   - **(a) Coalesce host-side, no guest change.** In `process_frq`, merge adjacent
     reported descriptors and only `madvise` the 16 KiB-aligned sub-runs. Free
     given `pageblock_order` (~2 MiB) batching; the M6 default. Measure the waste.
   - **(b) Boot the guest with 16 KiB pages.** Rebuild libkrunfw (or a Fedora
     kernel) with `CONFIG_ARM64_16K_PAGES`. Page sizes then match 1:1 — every
     reported page is trivially reclaimable *and* stage-2 TLB pressure drops (16K
     is Apple's native granularity). Cost: a custom kernel (lose stock-distro boot
     on EFI) + some 4K-assuming guest userspace. Best for the custom-kernel /
     low-overhead track.
   - **(c) Host-page-aware free-page reporting (new guest kernel feature).** Patch
     `mm/page_reporting.c` / the virtio-balloon reporting path to batch & align
     free runs to a host-page order negotiated via a new virtio-balloon feature
     bit. The correct *general* fix (host-page > guest-page is not unique to us);
     carry as our kernel patch now, upstream later. The "we have the sources" play.
   - **(d) virtio-mem** for large swings — block-based, 16K-friendly granularity,
     but a from-scratch device (see §3 Option E). Long-term, not M6.

   Plan: ship (a) in M6; keep (b) as the lever for the custom-kernel track; pursue
   (c) once measurements show (a)'s waste is material.

> Summary: today the balloon device gives us **free-page reporting only**, and
> even that reclaim call is **wrong for macOS** — `MADV_DONTNEED` returns nothing
> to the host (confirmed by spike `spikes/balloon-madvise`); the fix is
> `MADV_FREE_REUSABLE`, which works even on the live `hv_vm_map`'d region. Classic
> inflate/deflate ballooning is **not implemented** (handlers are stubs), no
> host-driven `num_pages` target is ever written, and there is **no public API**
> for any of it. Those are the real work.

### 1.3 How guest RAM is mapped via HVF

Guest RAM is a host `mmap` registered with Apple's Hypervisor.framework via
`hv_vm_map(uva, ipa, size, flags)` (stage-2 IPA→PA). The `MAP_ANON` region is
demand-paged: host physical pages are committed only when the guest first touches
them. (Confirm exact mapping code in `src/hvf` / `src/vmm`; the balloon's
`mem.get_host_address(desc.addr)` at `device.rs:91` proves guest PA → host UVA is
a simple offset into this one mapping.) macOS/HVF facts that matter for reclaim:
mappings must be 16 KiB-aligned; `hv_vm_unmap`/`hv_vm_protect` change stage-2 but
do not by themselves free host RAM — the backing `mmap` pages must be `madvise`-d.

### 1.4 macOS reclaim primitives (host side)

| Primitive | Behavior on macOS (measured 2026-05-30, macOS 26.5 / M1 Max) | Fit for ballooning |
|-----------|-------------------|--------------------|
| `madvise(MADV_DONTNEED)` (current code) | `rc=0` but `phys_footprint` **does not drop** (~1026 MiB stays), mapped or not. | Wrong primitive — what's used today, returns nothing |
| `madvise(MADV_FREE)` | Lazy: `rc=0`, footprint **does not drop until pressure**; contents may persist. | Soft return only (not prompt) |
| `madvise(MADV_FREE_REUSABLE)` + `MADV_FREE_REUSE` | Darwin libmalloc pair: `REUSABLE` drops footprint immediately (1 GiB → ~2 MiB, measured); `REUSE` re-validates before re-touch. | **Best fit** — accurate, immediate footprint reduction |
| `mmap(MAP_FIXED|MAP_ANON)` over the range | Hard reset to zero-fill-on-demand. | Heavyweight fallback |

All operate at 16 KiB granularity on Apple Silicon. **Measured:** `hv_vm_map` does
**not** pin pages — `MADV_FREE_REUSABLE` reclaims fully while the region stays
mapped into the VM (no `hv_vm_unmap` first). See `spikes/balloon-madvise/RESULTS.md`.

### 1.5 Guest-side overhead and requirements

- **Kernel**: libkrunfw (Homebrew `libkrunfw 5.3.0`) is a minimized, fixed
  microVM kernel — small static footprint but its own build. Booting the Fedora
  43 raw with the distro kernel instead uses `krun_set_kernel`/external-kernel
  (see `examples/external_kernel.*`); a distro kernel costs more RAM but covers
  all guest drivers (3D/USB).
- **Guest balloon driver**: needs `CONFIG_VIRTIO_BALLOON` (module in Fedora).
- **Free-page reporting** (the only working path): needs `CONFIG_PAGE_REPORTING`
  (default-y in modern Fedora) + the guest negotiating `F_REPORTING`.
- **PSI** for an autoballoon policy: `CONFIG_PSI` (default-y), read from
  `/proc/pressure/memory`.
- **virtio-mem**: `CONFIG_VIRTIO_MEM` + `CONFIG_MEMORY_HOTPLUG/HOTREMOVE`.

### 1.6 virtio-mem in libkrun?

No `virtio_mem`/virtio-mem in `lib.rs` and no `mem` device directory under
`src/devices/src/virtio/`. **libkrun has no virtio-mem device.** Adding one is a
from-scratch device + region/ACPI/DT effort.

---

## 2. How it works end to end

### 2.1 Static RAM today

```
limina -> krun_set_vm_config(ctx,vcpus,ram_mib)
          lib.rs:569 -> mem_size_mib (lib.rs:570) -> VmConfig -> set_vm_config (lib.rs:587)
krun_start_enter: mmap(MAP_ANON, mem_size) -> hv_vm_map(uva, ipa, size); load kernel/initramfs
guest faults pages in on first touch -> host commits 16 KiB pages on demand
```

Demand paging means host RSS tracks the working set at first, but it is a
**one-way ratchet**: once the guest's page cache fills RAM, host RSS stays high
because nothing tells the host the pages are free. Reporting/ballooning breaks the
ratchet.

### 2.2 Free-page reporting (the one working flow)

```
Guest (CONFIG_PAGE_REPORTING + F_REPORTING negotiated), when it has idle free pages:
  -> batches free ranges onto FRQ (queue 4), kicks
Host event_handler.rs:70 handle_frq_event -> device.rs:74 process_frq:
  -> per descriptor: host_addr = get_host_address(guest_pa)  (device.rs:91)
  -> madvise(host_addr, len, MADV_DONTNEED)                  (device.rs:100)  [WRONG on macOS]
  -> add_used (device.rs:106); signal used queue (event_handler.rs:82)
```

### 2.3 Inflate/deflate (NOT implemented)

`handle_ifq_event`/`handle_dfq_event` only log `"unsupported"` and drain the
eventfd (`event_handler.rs:14-40`); nothing reads PFNs, nothing reclaims, and the
host never sets `num_pages`. A target-driven balloon would require writing all of
this plus a config-change interrupt path.

### 2.4 Proposed PSI autoballoon control loop (does not exist)

```
limina-agent (guest) reads /proc/pressure/memory + /proc/meminfo every ~1s
  -> reports {some/full avg10, MemAvailable} over virtio-vsock (src/devices/src/virtio/vsock exists)
limina host loop:
  pressure high / MemAvailable low  -> deflate toward max
  pressure low  / MemAvailable high -> inflate toward min (hysteresis, rate-limit)
  clamp to [min_mib, max_mib] -> new krun_balloon_set_target()
```

---

## 3. Options inventory for limina

### A — Do nothing: static RAM + demand paging (upstream as-is)
- **Pros**: zero work; shrinks to working set on first boot; no agent.
- **Cons**: one-way ratchet; no `min..max`; trends to `ram_mib` over time. Fails dynamic requirement.

### B — Fix + use free-page reporting (the path that already works)
Replace `MADV_DONTNEED` with `MADV_FREE_REUSABLE` on macOS, handle 16 KiB
alignment, and (optionally) expose an enable flag.
- **Pros**: smallest real change — the FRQ handler already exists; hands-off
  continuous return of genuinely free pages; no policy loop, no agent; stock
  Fedora reports automatically. Breaks the ratchet.
- **Cons**: only returns *free* pages (page cache stays until guest frees it);
  reclaim cadence is the guest's; no enforceable `max`; effectiveness depends on
  guest reporting in 16 KiB-aligned runs.

### C — Implement target-driven balloon inflate/deflate (B + write the stubs)
Make IFQ/DFQ functional, update `num_pages`/`actual`, add config-change
interrupt, expose a target API.
- **Pros**: enforceable shrink toward `min` even when pages are page-cache, not
  free; industry-standard; works with stock `virtio_balloon`.
- **Cons**: more code (inflate handler, PFN reclaim, config writes, interrupt);
  inflate can OOM the guest — and `F_DEFLATE_ON_OOM` is **not** advertised today,
  so we'd add it as a safety net; needs a policy (Option D) to be useful.

### D — Balloon (C) + PSI autoballoon agent over vsock  ← composite
- **Pros**: true `min..max`, reacts to real guest pressure (PSI); grows under
  load, shrinks under host pressure; vsock + balloon device already in-tree.
- **Cons**: most moving parts (agent, vsock protocol, control loop, anti-thrash
  tuning); needs the macOS reclaim fix and `F_DEFLATE_ON_OOM`.

### E — virtio-mem hotplug
- **Pros**: cleanest large-range grow/shrink; online blocks; no 4 KiB balloon
  accounting; designed for `min..max`.
- **Cons**: does **not exist** in libkrun — new device + region reservation +
  guest config + aarch64 ACPI/DT under HVF; unplug needs movable-zone cooperation
  and can fail/fragment. Highest cost/risk; overkill for milestone.

### F — B + D together
Reporting handles automatic baseline return; balloon+PSI enforces bounds.
- **Pros**: best of both. **Cons**: superset complexity of D.

---

## 4. Recommendation

**Phase 0 (boot Fedora raw):** Option A. Static `ram_mib` + demand paging is
already what `krun_set_vm_config` gives us; do not block the boot milestone on
dynamic memory.

**Phase 1 (dynamic-memory MVP):** **Option B first, then D** (i.e. land toward F):

1. **Fix the macOS reclaim call** in `process_frq()` (`balloon/device.rs:96-102`):
   on macOS use `madvise(MADV_FREE_REUSABLE)` (with `MADV_FREE_REUSE` where the
   guest later re-touches via deflate) instead of `MADV_DONTNEED`. **Confirmed by
   spike:** `MADV_DONTNEED` returns nothing on macOS 26.5 while `MADV_FREE_REUSABLE`
   reclaims the full region even while `hv_vm_map`'d. This is the highest-leverage,
   smallest patch and is required by every option except A. (Re-measure on the
   shipping macOS version — this is OS-specific behavior.)
2. **Enforce 16 KiB alignment** in the reclaim loop: only `madvise` the
   host-page-aligned, fully-covered sub-range of each descriptor; log/measure
   waste. Confirm with a spike how much stock Fedora reporting actually returns.
3. **Implement target-driven inflate/deflate** (Option C): write the IFQ handler
   (read PFNs, reclaim with the macOS-correct call), update `num_pages`/`actual`,
   add the config-change interrupt, and **advertise `F_DEFLATE_ON_OOM`** as a
   safety net (`device.rs:27-30`).
4. **Expose a public balloon API** (§4.1) and wire the device through `VmConfig`
   next to `mem_size_mib` (`lib.rs:570`).
5. **Build limina-agent + vsock protocol** (Option D): tiny guest daemon reporting
   `/proc/pressure/memory` + `MemAvailable`; host control loop with hysteresis,
   rate-limiting, clamped to `[min,max]`, driving `krun_balloon_set_target`.

> **Update 2026-07-20 — `F_DEFLATE_ON_OOM` recommendation reversed.** The bit was advertised
> as planned (M6, patch 0034) and is now being **dropped**: Linux keeps ballooned pages in
> `MemTotal`/counted-as-used exactly when the bit is negotiated (`fill_balloon()` skips
> `adjust_managed_page_count` — the accounting is keyed on the bit, commit `997e120843e8`),
> which makes a freshly ballooned VM look out of memory and feeds systemd-oomd false
> pressure that preempts the bit's own OOM notifier. Without the bit, `MemTotal` tracks
> effective RAM and inflation is transparent. Analysis + compensations: the 2026-07-20
> addendum in `docs/design/m6-dynamic-memory.md`.

**Defer Option E (virtio-mem)** until balloon proves insufficient (very large
swings, fragmentation). It is the right long-term tool for big grow ranges but a
large, risky from-scratch device.

### 4.1 Proposed public API to add to libkrun

```c
/* Enable virtio-balloon; min/max bound the dynamic range. max==0 -> ram_mib.
   flags: bit0 REPORTING, bit1 DEFLATE_ON_OOM, bit2 STATS. */
int32_t krun_add_balloon(uint32_t ctx_id, uint32_t min_mib, uint32_t max_mib, uint32_t flags);

/* Set desired current guest RAM target; host inflates/deflates. Clamped [min,max]. */
int32_t krun_balloon_set_target(uint32_t ctx_id, uint32_t target_mib);

/* Read current actual guest-available RAM (ram_mib - inflated balloon). */
int32_t krun_balloon_get_actual(uint32_t ctx_id, uint32_t *out_mib);

/* Optional: snapshot of the guest stats vq (free/available/swap). Needs STQ impl. */
int32_t krun_balloon_get_stats(uint32_t ctx_id, struct krun_balloon_stats *out);
```

Mirrors libkrun's `krun_add_*`/`krun_set_*` style; keeps the PSI/PID policy in
limina (host) so libkrun stays mechanism, not policy.

---

## 5. What must be patched / built (checklist)

libkrun:
- [ ] macOS reclaim branch (`MADV_FREE_REUSABLE`/`_REUSE`) in `balloon/device.rs:96-102`
      — **required** (spike: `MADV_DONTNEED` returns nothing on macOS; `_REUSABLE` works,
      even while `hv_vm_map`'d).
- [ ] 16 KiB host-page alignment/coalescing in `process_frq()` (and the new inflate handler).
- [ ] Implement IFQ (and DFQ) handlers (currently stubs, `event_handler.rs:14-40`); update `num_pages`/`actual` in config; config-change interrupt.
- [ ] ~~Advertise `F_DEFLATE_ON_OOM` (and consider `STATS` impl) at `device.rs:27-30`.~~
      **Reversed 2026-07-20** — the bit makes ballooned pages read as used in the guest;
      being dropped (see §4 update above).
- [ ] Public C API (`krun_add_balloon`, `_set_target`, `_get_actual`, `_get_stats`) in `src/libkrun/src/lib.rs` + `include/libkrun.h`; thread `min/max` through `VmConfig` (`lib.rs:570`).

limina:
- [ ] limina-agent (guest): read `/proc/pressure/memory` + `/proc/meminfo`, report over vsock.
- [ ] Host control loop: hysteresis, rate-limit, clamp `[min,max]`, drive `krun_balloon_set_target`.

guest image (Fedora 43 likely already provides — verify on the raw):
- [ ] `virtio_balloon` loaded; `CONFIG_PAGE_REPORTING`, `CONFIG_PSI` present.

---

## 6. Open questions / things to prototype

1. **RESOLVED (spike `spikes/balloon-madvise`, 2026-05-30, macOS 26.5 / M1 Max):**
   `MADV_FREE_REUSABLE` **does** drop `phys_footprint` (1 GiB → ~2 MiB) while the
   region stays `hv_vm_map`'d, and `hv_vm_map` does **not** pin the pages — no
   `hv_vm_unmap`/`hv_vm_protect` needed first. `MADV_DONTNEED` (current code)
   returns nothing; `MADV_FREE` is lazy. So balloon reclaim on macOS/HVF is viable
   **with the `MADV_FREE_REUSABLE` fix**. Re-confirm on the shipping OS version.
2. **How much does stock Fedora free-page reporting actually return on Apple
   Silicon** given the 4 KiB→16 KiB mismatch? Does the guest report in
   ≥16 KiB-aligned runs (pageblock_order batching), or do we need a guest patch?
3. **Re-touch latency** after `MADV_FREE_REUSABLE` (cost of `_REUSE`/fault-in) on
   guest deflate — fast enough for an interactive desktop VM?
4. **Inflate-target effectiveness** once IFQ is implemented: does Fedora's
   balloon driver inflate in host-page-aligned chunks, or do we waste reclaim?
5. **PSI thresholds / anti-thrash tuning** for desktop workloads (build, browser,
   IDE) — watermarks and hysteresis that avoid oscillation.
6. **Cost of virtio-mem** if Phase 2 needs it (region reservation, aarch64
   ACPI/DT under HVF, unplug fragmentation).
7. **libkrunfw vs distro-kernel boot/runtime footprint** for the same guest —
   quantify guest overhead to inform the kernel choice for the milestone.

---

## 7. References

Local source (read this session):
- `/opt/homebrew/include/libkrun.h:93,98` — `krun_set_vm_config` / `ram_mib`.
- `.../libkrun/src/libkrun/src/lib.rs:569,570,587` — config entry, `mem_size_mib`, `set_vm_config`.
- `.../libkrun/src/devices/src/virtio/balloon/device.rs` — features (27-30), config struct (34-43), `process_frq` reclaim incl. `get_host_address` (91) and `madvise(MADV_DONTNEED)` (96-102), `add_used` (106), `write_config` reject (154-160).
- `.../libkrun/src/devices/src/virtio/balloon/event_handler.rs` — inflate/deflate stubs (14-40), stats ignored (42-54), page-hint stub (56-68), FRQ handler (70-84).
- `.../libkrun/src/devices/src/virtio/balloon/mod.rs` — `NUM_QUEUES=5`, `QUEUE_SIZE=256` (11-13), feature constants (15-21).

To verify (not re-read this session):
- `.../libkrun/src/hvf/`, `.../src/vmm/` — `mmap(MAP_ANON)` + `hv_vm_map` guest-RAM setup.
- Absence of a virtio-mem device under `.../src/devices/src/virtio/`.

External:
- Virtio 1.x spec §5.5 "Memory Balloon Device" (feature bits, virtqueues, config, free-page reporting/hinting).
- Linux `drivers/virtio/virtio_balloon.c`, `mm/page_reporting.c`; `CONFIG_PAGE_REPORTING`, `CONFIG_PSI`, `CONFIG_VIRTIO_MEM`, `CONFIG_MEMORY_HOTPLUG`.
- Apple `man madvise` (Darwin) — `MADV_FREE`, `MADV_FREE_REUSABLE`, `MADV_FREE_REUSE`, `MADV_DONTNEED`.
- Apple Hypervisor.framework — `hv_vm_map`, `hv_vm_unmap`, `hv_vm_protect` (arm64).
- virtio-mem design (D. Hildenbrand) for the Phase-2 comparison.
