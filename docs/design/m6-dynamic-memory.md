# M6 — Dynamic memory (virtio-balloon, `min..max`)

**Status: M6 COMPLETE — Steps 1–4 LANDED & verified.** This
document is the authoritative, edit-by-edit implementation plan synthesized for Milestone 6.
The actual edit → `scripts/apply-libkrun-patches.sh` → build → codesign → `scripts/test-boot.sh`
loop is driven **serially in the main loop**, not here. Every non-obvious claim below
carries a `path:line` citation into `third_party/libkrun/` or `crates/`; re-verify
before editing (point-in-time anchors).

> **Implementation log:**
> - **Step 1 — DONE (2026-06-26, patch `0033`).** `process_frq` reclaims with
>   `MADV_FREE_REUSABLE` + the `ReclaimCoalescer` (16 KiB-safe). Verified three ways:
>   the spike re-confirmed on macOS 26.5.1 (DONTNEED→0 RED, REUSABLE→full GREEN); 5
>   deterministic coalescer-safety unit tests (`krun-devices`); and the end-to-end L2 test
>   `crates/limina-test/tests/balloon.rs` on stock F44 — worker `phys_footprint` rose +1.7 GiB
>   on a 2 GiB guest allocation, then fell ~1 GiB after the guest freed it. Harness hook
>   `Guest::worker_phys_footprint()` added.
> - **Step 2 — DONE (2026-06-26, patch `0034`).** Inflate/deflate handlers (PFN-array parse),
>   `BalloonControlHandle` (mirrors `DisplayResizeHandle`), `num_pages` target + config-change
>   interrupt (`DeviceState::signal_config_change`), guest `actual` via `write_config`,
>   `DEFLATE_ON_OOM` advertised. Captured on `Vmm::balloon_control_handle`.
> - **Step 3 — DONE (2026-06-26, no libkrun patch).** Worker `--balloon-control-socket`
>   (`install_balloon_listener`: `target <bytes>` / `stats`), supervisor `--memory MIN..MAX`
>   (parsed, MAX→RAM, auto-allocates the socket), harness `with_balloon_control()` +
>   `Guest::set_balloon_target`/`balloon_stats`. Verified end-to-end on stock F44
>   (`crates/limina-test/tests/balloon_inflate.rs`): 1 GiB target → `actual` 1 GiB, guest
>   MemAvailable −1 GiB; target 0 → `actual` 0, memory returns. `parse_memory_range` unit-tested.
> - **Step 4 — DONE (2026-06-26, no libkrun patch).** PSI autoballoon: `limina_proto::MemPressure`
>   (msg-type 7, round-trip + `from_proc` parser host-tested); the `limina-agent` reporter reads
>   `/proc/pressure/memory` + `/proc/meminfo` on its idle tick and sends it (`mempressure` cap;
>   `psi=0` → MemAvailable-only fallback); the supervisor `BalloonPolicy` (`crates/limina/src/balloon_policy.rs`)
>   turns reports into balloon targets with hysteresis + an 800 ms inflate dwell (release-fast,
>   reclaim-gradual; 5 `decide()` unit tests); wired through the control plane (`control.rs`
>   `Message::MemPressure` arm) and a `--memory`-gated policy in `main.rs`. The worker's balloon
>   listener now serves **one thread per connection** (the policy holds a long-lived connection,
>   so a stats query must not block behind it). Verified end-to-end on stock F44
>   (`crates/limina-test/tests/balloon_psi.rs`, harness-as-agent peer): injected idle pressure →
>   policy inflated the guest balloon to 2 GiB; injected high pressure → policy released it to 0.

> Cross-refs: `docs/roadmap.md` "Milestone 6"; the resolved reclaim spike
> `spikes/balloon-madvise/RESULTS.md`; the shipped runtime-resize precedent this plan
> mirrors for its control-handle/control-socket/RED-test shape
> (`docs/design/runtime-display-resize.md`).

---

## 1. Overview & the measured case

The host's RSS for a limina VM is a **guest-page high-water mark**: it only ever rises.
Measured 2026-06-11 on the tier-2 desktop:

| moment | host RSS |
|---|---|
| boot-idle | **5.2 GiB** |
| after a browsing session | **6.8 GiB** |
| after the browser frees memory (no balloon) | **stays at 6.8 GiB** |

The mapping is 4 GiB guest RAM + the 8 GiB venus shm window (lazily mapped). Guest idle
~2 GiB is Fedora's own daemons, not VM overhead — so **reclaim is the lever, not guest
slimming**. GPU/Metal buffers already recycle correctly, so the balloon is the remaining
story.

**Spike result that anchors the whole milestone** (`spikes/balloon-madvise/RESULTS.md`,
macOS 26.5, M1 Max, 16 KiB host pages):

- `MADV_DONTNEED` — what libkrun ships at `balloon/device.rs:100` — drops **nothing**
  (returns rc=0 but `phys_footprint` holds the full region), mapped or not.
- `MADV_FREE_REUSABLE` drops the **full** region even while the backing `MAP_ANON` is
  actively `hv_vm_map`'d, **without** needing `hv_vm_unmap` first.
- `MADV_FREE` is lazy (footprint holds until real pressure) — not usable for accountable
  return.
- `hv_vm_map` does **not** pin/wire the pages, so balloon-style dynamic memory is viable
  on HVF without remapping the IPA range.

A `0` return from `madvise` does **not** mean memory came back — only `MADV_FREE_REUSABLE`
(=7 in the pinned `libc` 0.2.186) / `MADV_FREE_REUSE` (=8) actually debit
`phys_footprint`.

### Memory model: `min..max`

- libkrun allocates the guest-RAM `MAP_ANON` at **`max`** (`set_vm_config` `mem_size_mib =
  max`, `crates/limina-vmm/src/krun/mod.rs:114-120`). The guest sees `max` physical RAM.
- Two **independent, additive** reclaim mechanisms return pages to macOS:
  1. **Free-page reporting (FRQ, opportunistic).** The guest kernel's page-reporting infra
     hands the host its *currently-free* pages; the host `madvise(MADV_FREE_REUSABLE)`s
     them. No target, no guest cooperation beyond the stock driver. **This is the cheap
     first win** — it makes the 6.8 GiB high-water mark actually return when the browser
     frees memory. (Step 1.)
  2. **Inflate/deflate (forcing).** The host sets a **`num_pages` target**; the guest
     balloon driver allocates that many pages and hands them over (inflate), removing them
     from guest-available RAM; the host frees them. Deflate returns them. This **caps**
     effective guest RAM at `max − actual`. (Steps 2–4.)
- Effective guest RAM = `max − actual`. The PSI policy moves the target inside
  `[0, max−min]`, so effective RAM stays in `[min, max]`.

**Mechanism vs policy (the load-bearing split):**

| Layer | Owns |
|---|---|
| **libkrun (mechanism)** | `MADV_FREE_REUSABLE` reclaim + 16 KiB-safe coalescing; inflate/deflate PFN processing; `num_pages` target + config-change interrupt; `DEFLATE_ON_OOM` feature bit; `BalloonControlHandle` (internal Rust). |
| **limina (policy)** | `--memory min..max`; `max`→krun RAM; balloon-control socket; PSI watermarks/hysteresis; target computation; when/how much to inflate. |
| **guest** | stock: page-reporting + balloon driver (no change). enhanced: 16 KiB pages, `psi=1`, the `limina-agent` PSI reporter (installable on stock too). |

---

## 2. Ordered implementation

The order is **mandatory** (spike-confirmed): Step 1 is a self-contained libkrun change
that must **land and be HVF-verified before** Steps 2–4. Each step lists files+anchors, a
diff sketch, its `patches/libkrun/` name (next free number **0033**), the
mechanism/policy split, two-tier behavior, and the gating RED test in `crates/limina-test`.

### Step 1 — Reclaim fix + provably-safe 16 KiB coalescing

**Patch:** `patches/libkrun/0033-limina-balloon-reclaim-with-MADV_FREE_REUSABLE-and-16.patch`
(subject: `limina: balloon — reclaim free pages with MADV_FREE_REUSABLE + 16 KiB-safe coalescing`).

**File:** `third_party/libkrun/src/devices/src/virtio/balloon/device.rs`
(`process_frq`, currently `device.rs:74-112`; the offending `madvise(..., MADV_DONTNEED)`
is `device.rs:96-102`).

**What's wrong today:** `process_frq` madvises each FRQ descriptor's `[host_addr, len)`
with `MADV_DONTNEED` (returns nothing on macOS) **and** does so at the descriptor's native
(4 KiB) granularity with no host-page awareness — which on a 16 KiB host would be unsafe if
the call actually freed anything.

**Diff sketch** (replace the `unsafe { libc::madvise(...) }` inner block and the
`add_used` ordering):

```rust
// new module-private helper (device.rs)
const GUEST_PAGE: usize = 4096; // virtio-balloon page unit (VIRTIO_BALLOON_PFN_SHIFT == 12)

fn host_page_size() -> usize {
    // 16384 on Apple Silicon; queried once per batch.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

/// Accumulates guest-reported FREE 4 KiB sub-ranges and yields only the host pages that are
/// PROVABLY safe to return to macOS. INVARIANT (the whole safety proof): a host page is
/// emitted ONLY when every one of its constituent guest pages was reported free in this batch,
/// so MADV_FREE_REUSABLE can never discard a still-live guest page.
struct ReclaimCoalescer {
    host_page: usize,
    full_mask: u64,                 // (1 << (host_page/GUEST_PAGE)) - 1  == 0b1111 on 16K/4K
    partial: std::collections::HashMap<usize, u64>, // host_page_base -> covered-subpage bitmask
}
impl ReclaimCoalescer {
    fn new(host_page: usize) -> Self {
        let sub = host_page / GUEST_PAGE;
        Self { host_page, full_mask: (1u64 << sub) - 1, partial: Default::default() }
    }
    /// Record [addr, addr+len) (4 KiB-granular free run) as free. Never madvises.
    fn add(&mut self, addr: usize, len: usize) {
        let start = addr & !(GUEST_PAGE - 1);
        let end   = (addr + len) & !(GUEST_PAGE - 1); // drop any sub-4K tail (defensive)
        let mut p = start;
        while p < end {
            let base = p & !(self.host_page - 1);
            let sub  = (p - base) / GUEST_PAGE;
            *self.partial.entry(base).or_insert(0) |= 1u64 << sub;
            p += GUEST_PAGE;
        }
    }
    /// Drain host pages whose every sub-page is now free; partials are DROPPED (see safety note).
    fn take_full_pages(&mut self) -> Vec<(usize, usize)> {
        let (full, hp) = (self.full_mask, self.host_page);
        let mut out = Vec::new();
        self.partial.retain(|&base, &mut m| if m == full { out.push((base, hp)); false } else { true });
        out
    }
}
```

```rust
// process_frq() body — per-head coalescing, madvise BEFORE add_used
while let Some(head) = queues[FRQ_INDEX].queue.pop(mem) {
    let index = head.index;
    let mut coalescer = ReclaimCoalescer::new(host_page_size());
    for desc in head.into_iter() {
        let host_addr = mem.get_host_address(desc.addr).unwrap() as usize;
        coalescer.add(host_addr, desc.len as usize);
    }
    // Pages are still ISOLATED by page_reporting until we add_used this head, so madvise here is safe.
    for (base, len) in coalescer.take_full_pages() {
        // SAFETY: base/len host-page-aligned AND every guest page inside is reported-free this batch.
        let rc = unsafe { libc::madvise(base as *mut libc::c_void, len, libc::MADV_FREE_REUSABLE) };
        if rc != 0 {
            warn!("balloon frq: madvise(REUSABLE) at {base:#x} failed: {}",
                  std::io::Error::last_os_error());
        }
        self.reclaimed_bytes.fetch_add(len as u64, std::sync::atomic::Ordering::Relaxed);
    }
    have_used = true;
    if let Err(e) = queues[FRQ_INDEX].queue.add_used(mem, index, 0) {
        error!("balloon frq: add_used failed: {e:?}");
    }
}
```

Add `reclaimed_bytes: Arc<AtomicU64>` to `Balloon` (init in `Balloon::new`,
`device.rs:57-68`) so Step 2/3 stats can read it.

**Two crucial ordering/safety points (resolve the FATAL refutation):**
1. **Madvise BEFORE `add_used`.** `page_reporting` keeps the reported pages isolated from
   the guest allocator until the descriptor is marked used; freeing them first closes the
   window where the guest could reallocate a page we're about to discard.
2. **FRQ coalescing is per-head and NEVER persisted.** A reported page stays guest-owned
   and may be reallocated after we ack it; persisting a 3-of-4 partial across batches could
   later complete a host page whose 4th sub-page is again live → corruption. So a host page
   split across two *heads* is simply not reclaimed (acceptable: reported runs are
   pageblock-aligned ≥2 MiB — see §3).

**Mechanism/policy:** pure libkrun mechanism. **Two-tier:** stock 4 KiB guest →
coalesced reclaim, near-full (fringe loss ≤12 KiB per run boundary, ~nil in practice);
enhanced 16 KiB guest → mask is always `full` after one page → exact 1:1 reclaim.

**Gating RED test:** `crates/limina-test/tests/l1_balloon_reclaim.rs`
- Boot a guest with the balloon device + console shell (pattern: `l1_resize.rs`).
- New harness hook `Guest::worker_phys_footprint() -> Result<u64>`: find the worker
  (limina-vmm) as a child of `self.pid` via `proc_listchildpids`, read
  `proc_pid_rusage(worker, RUSAGE_INFO_V2).ri_phys_footprint`.
- Over the console: `mmap`+touch ~512 MiB, confirm footprint rose, `munmap`, then
  `echo 1 > /proc/sys/vm/compact_memory` to nudge reporting; poll until footprint drops by
  ≥~400 MiB.
- **RED today** (`MADV_DONTNEED` returns nothing). GREEN after the patch.
- **Safety sub-gate (libkrun `#[cfg(test)]` in `balloon/device.rs`):**
  `process_frq_coalescing_is_16k_safe` — feed `ReclaimCoalescer` unaligned starts,
  boundary-spanning descriptors, and host pages split across descriptors; assert every
  emitted range is 16 KiB-aligned, a 16 KiB multiple, and fully covered; assert a
  3-of-4 host page is **never** emitted.

---

### Step 2 — Inflate/deflate handlers + `DEFLATE_ON_OOM` + target + `BalloonControlHandle`

> **2026-07-20:** `DEFLATE_ON_OOM` is being **dropped** — it's what makes ballooned memory
> read as "used" inside the guest. See the transparent-accounting addendum at the end of
> this doc; the rest of Step 2 stands as built.

**Patch:** `patches/libkrun/0034-limina-balloon-inflate-deflate-target-DEFLATE_ON_OOM-.patch`
(subject: `limina: balloon — inflate/deflate handlers, num_pages target, DEFLATE_ON_OOM, BalloonControlHandle`).

**Files:** `balloon/mod.rs`, `balloon/device.rs`, `balloon/event_handler.rs`,
`src/devices/src/virtio/device.rs` (DeviceState helper), `src/vmm/src/lib.rs` +
`src/vmm/src/builder.rs` (capture handle on `Vmm`).

**2a. Feature bits & constants** (`balloon/mod.rs:15-21`):

```rust
pub mod uapi {
    // ...existing...
    pub const VIRTIO_BALLOON_F_DEFLATE_ON_OOM: u32 = 2; // OOM safety net
    pub const VIRTIO_BALLOON_PFN_SHIFT: u32 = 12;       // balloon page is ALWAYS 4 KiB
}
```
OR it into `AVAIL_FEATURES` (`device.rs:27-30`):
`| (1 << uapi::VIRTIO_BALLOON_F_DEFLATE_ON_OOM as u64)`.

**2b. `process_ifq` — inflate (resolves the PFN-array MAJOR).** Inflate descriptors are
**NOT page ranges** — each buffer is an array of little-endian `u32` balloon-PFNs. Naively
reusing `process_frq` would madvise the *PFN array buffer itself* (garbage). Correct:

```rust
pub fn process_ifq(&mut self) -> bool {
    let mem = /* Activated(ref mem, _) */;
    let mut have_used = false;
    while let Some(head) = queues[IFQ_INDEX].queue.pop(mem) {
        let index = head.index;
        let mut pages = 0u32;
        for desc in head.into_iter() {
            let count = desc.len as usize / 4; // u32 PFNs
            for i in 0..count {
                let bpfn: u32 = mem.read_obj(desc.addr.checked_add((i*4) as u64).unwrap()).unwrap();
                let guest = GuestAddress((bpfn as u64) << uapi::VIRTIO_BALLOON_PFN_SHIFT);
                let host_addr = mem.get_host_address(guest).unwrap() as usize;
                self.inflate_coalescer.add(host_addr, GUEST_PAGE); // PERSISTED — see safety note
                pages += 1;
            }
        }
        // Inflated pages are owned by the balloon until deflate, so persisting partials IS safe here.
        for (base, len) in self.inflate_coalescer.take_full_pages() {
            unsafe { libc::madvise(base as *mut _, len, libc::MADV_FREE_REUSABLE) };
            self.ballooned.insert(base); // remember for deflate REUSE
            self.reclaimed_bytes.fetch_add(len as u64, Relaxed);
        }
        self.config.actual = self.config.actual.saturating_add(pages);
        self.actual_shared.store(self.config.actual, Relaxed);
        have_used = true;
        queues[IFQ_INDEX].queue.add_used(mem, index, 0).ok();
    }
    if have_used { self.signal_config_change(); } // publish new `actual`
    have_used
}
```

**Coalescing asymmetry (resolves a MAJOR):** the **inflate** coalescer (`self.inflate_coalescer`,
a `ReclaimCoalescer` field) **may persist** across heads/batches because the balloon owns
inflated pages until deflate — so a 16 KiB host page whose 4 sub-PFNs arrive in different
heads is still reclaimed on the stock 4 KiB guest. (Contrast Step 1's FRQ coalescer, which
must be per-head.)

**2c. `process_dfq` — deflate.** For each returned PFN→host page, `MADV_FREE_REUSE` the
containing 16 KiB host page (re-validate accounting; correctness does not strictly require
it — a freed page re-faults zero-filled — but it keeps `phys_footprint` accounting honest
and matches the roadmap), clear it from `self.ballooned`/`inflate_coalescer`, and
`config.actual = actual.saturating_sub(pages)`. Signal config-change.

**2d. Wire the handlers** (`balloon/event_handler.rs:14-40`): replace the two stubs:

```rust
pub(crate) fn handle_ifq_event(&mut self, event: &EpollEvent) {
    if event.event_set() != EventSet::IN { return; }
    if self.queue_event(IFQ_INDEX).read().is_ok() && self.process_ifq() {
        self.device_state.signal_used_queue();
    }
}
// handle_dfq_event symmetric → process_dfq()
```

**2e. `BalloonControlHandle`** (mirror `DisplayResizeHandle`, `gpu/device.rs:46-84,157-163`):

```rust
#[derive(Clone)]
pub struct BalloonControlHandle {
    target_evt: Arc<EventFd>,
    pending_target: Arc<Mutex<Option<u32>>>, // pages (4 KiB units)
    actual: Arc<AtomicU32>,
    reclaimed: Arc<AtomicU64>,
}
impl BalloonControlHandle {
    pub fn set_target_pages(&self, pages: u32) -> bool {
        *self.pending_target.lock().unwrap() = Some(pages);
        self.target_evt.write(1).is_ok()
    }
    pub fn get_actual(&self) -> u32 { self.actual.load(Relaxed) }
    pub fn get_stats(&self) -> BalloonStats {
        BalloonStats { actual_pages: self.actual.load(Relaxed),
                       reclaimed_bytes: self.reclaimed.load(Relaxed) }
    }
}
```
- Add `target_evt`/`pending_target`/`actual_shared`/`inflate_coalescer`/`ballooned` fields
  to `Balloon` (init in `Balloon::new`, `device.rs:57-68`).
- `Balloon::balloon_control_handle(&self) -> BalloonControlHandle` clones the Arcs.
- **Register `target_evt`** in `handle_activate_event` (`event_handler.rs:86-153`, alongside
  the queue fds) and add a match arm in `Subscriber::process` (`event_handler.rs:166-177`):
  on wake, read `target_evt`, take `pending_target` → `self.config.num_pages = target` →
  `self.signal_config_change()`. The guest's balloon driver reacts to the config-change and
  drives IFQ/DFQ toward `num_pages`.

**2f. config-change plumbing.** Add `DeviceState::signal_config_change(&self)` mirroring
`signal_used_queue` (`device.rs:51-56`) → `interrupt.try_signal_config_change()`
(`virtio/mmio.rs:147`). `read_config` already returns `config` including `num_pages`/`actual`
(`device.rs:140-152`); no `write_config` change needed (the guest doesn't write balloon
config).

**2g. Capture the handle on `Vmm`** (mirror `gpu_resize_handle`, `vmm/lib.rs:221,228-235`):
add `balloon_control_handle: Option<BalloonControlHandle>` field + accessor/setter; set it in
`attach_balloon_device` (`builder.rs:2421-2438`, right after construction `builder.rs:2428`):
`vmm.set_balloon_control_handle(balloon.lock().unwrap().balloon_control_handle());`.

**Mechanism/policy:** pure libkrun mechanism. **Two-tier:** stock 4 KiB → balloon driver
inflates in 4 KiB units, coalesced to host pages; `DEFLATE_ON_OOM` returns pages under guest
OOM. Enhanced 16 KiB → 4 PFNs per 16 KiB page, coalesced perfectly.

**Gating RED test:** `crates/limina-test/tests/l1_balloon_inflate.rs`
- limina-vmm adds `install_balloon_listener(path, handle)` (mirror `install_resize_listener`,
  `krun/mod.rs:374-446`): newline `target <bytes>` → `handle.set_target_pages(bytes >> 12)`;
  `stats` → reply `actual=<bytes> reclaimed=<bytes>`.
- Harness `Guest::set_balloon_target(bytes)` connects to that socket (new `balloon_socket`
  field mirroring `resize_socket`).
- Boot with `--memory`, set target = e.g. 512 MiB; assert guest `/proc/meminfo`
  `MemTotal`/`MemAvailable` drop by ~512 MiB **and** `worker_phys_footprint` drops by ~512 MiB.
- **RED today** (handlers log `unsupported inflate queue event`).

---

### Step 3 — Supervisor `--memory min..max` + control-socket plumbing

**No new libkrun patch** (reuses 0034's `BalloonControlHandle` + `Vmm` accessor — internal
Rust API, no C ABI). All edits in `crates/`.

**3a. limina-vmm** (`crates/limina-vmm/src/`):
- `config.rs` (mirror `control_socket` `config.rs:111-116` and `ram_mib` `config.rs:175`):
  add `mem_min_mib: usize`, `mem_max_mib: usize`, `balloon_control_socket: Option<PathBuf>`.
- `krun/mod.rs:114-120`: `set_vm_config(mem_size_mib = mem_max_mib)`. After build, if
  `balloon_control_socket` is set, `install_balloon_listener(path, vmm.balloon_control_handle())`
  (added in Step 2; same shape as the resize listener `krun/mod.rs:374-446`).
- `main.rs` (mirror `--display-control-socket` `main.rs:128-132,267`): add
  `--memory-min`, `--memory-max`, `--balloon-control-socket`.

**3b. Supervisor** (`crates/limina/src/main.rs`):
- Add `--memory <min..max>` (e.g. `2G..12G`); parse to `min_mib`/`max_mib`.
- Push to the worker: `--ram-mib=<max>` (existing arg, `main.rs:180-181`),
  `--memory-min=<min>`, `--memory-max=<max>`, and an auto-allocated
  `--balloon-control-socket=<path>` (same arg-push pattern as `--net-gvproxy`,
  `main.rs:343-360`). Keep `--ram-mib`-only as the static fallback (no balloon).
- Expose the socket path to the harness/policy (so `Guest::set_balloon_target` and Step 4's
  policy can reach it).

**Two-tier:** identical surface for both tiers; the difference is reclaim efficiency
(Steps 1–2) and whether the agent (Step 4) supplies pressure feedback.

**Gating RED test:** `crates/limina-test/tests/l1_balloon_target.rs`
- `GuestConfig::with_memory(2048, 12288)` (new builder; sets `mem_min/max` + enables the
  balloon socket) booted through the **real supervisor** arg path.
- Drive target end-to-end; read back `stats` (assert `actual` rises); assert guest `MemTotal`
  and worker footprint track it. **RED today** (no `--memory` flag, no socket binding).

---

### Step 4 — PSI autoballoon (guest agent + supervisor policy)

**No libkrun patch.** Edits in `crates/limina-proto`, `guest/limina-agent`, `crates/limina`.

**4a. limina-proto** (`crates/limina-proto/src/lib.rs`) — the **new vsock control message**:
- `msg_type::MEM_PRESSURE = 7` (`lib.rs:67-77`; 7 is free — control uses 1–6, clipboard 16–18).
- `Message::MemPressure(MemPressure)` variant (`lib.rs:172-187`) + `msg_type()` arm +
  encode/decode arms (`lib.rs:215-252`).
- ```rust
  #[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
  pub struct MemPressure {
      #[n(0)] pub some_avg10: u32, #[n(1)] pub some_avg60: u32,
      #[n(2)] pub full_avg10: u32, #[n(3)] pub full_avg60: u32, // /proc/pressure/memory *100
      #[n(4)] pub mem_available_kib: u64, #[n(5)] pub mem_total_kib: u64,
  }
  ```
- Direction guest→host on `CHANNEL_CONTROL`. **Additive & Unknown-tolerant** per the proto's
  robustness rules (`lib.rs:17-23`): an old host answers `ERR_UNSUPPORTED`, channel stays up.

**4b. Guest agent** (`guest/limina-agent/src/main.rs:166-212`):
- Add cap `"mempressure"` to `Hello.caps` (`main.rs:171-174`).
- In the poll(2)-driven `serve` loop, on a cadence (e.g. every 2 s) read
  `/proc/pressure/memory` + `/proc/meminfo` and emit `Message::MemPressure`. If
  `/proc/pressure/*` is absent (`psi=0`), send zeros for the PSI fields (MemAvailable-only
  fallback). Frame-tearing-safe: emit between heartbeats, never from a timer interrupt
  (matches the existing cadence discipline, `main.rs:163-166`).

**4c. Supervisor policy** (`crates/limina/src/control.rs`): on `MemPressure`, compute a target
with watermarks + hysteresis and forward `target <bytes>` to the balloon socket:
- pressure high (`some_avg10` over watermark) or `MemAvailable` low → **deflate** (target→0),
  let the guest grow toward `max`.
- idle + much-free for N samples → **inflate** (target→`max−min`), shrink toward `min`;
  Step 1's FRQ reclaim mops up the rest opportunistically.
- Hysteresis band + min dwell time to avoid thrash.

**Two-tier:** the enhanced image ships the agent + `psi=1` → full PSI policy. Stock can
**install** the agent (it's a userspace musl binary, no kernel dependency — fits the
"bootstrap floor" rule); without the agent the supervisor falls back to a conservative
time/idle target plus opportunistic FRQ reclaim.

**Gating RED test:** `crates/limina-test/tests/l1_balloon_psi.rs`
- Boot `with_control_agent()` + `with_memory(2048, 12288)`; drive in-guest pressure (alloc),
  then release; assert the supervisor moves the target and worker footprint falls toward `min`
  within a timeout. **RED today** (agent emits no `MemPressure`; supervisor has no policy).

---

## 3. The safe coalescing algorithm (in detail)

This is the heart of the milestone's correctness and the answer to the FATAL refutation.

**Problem.** The host page is **16 KiB**; a stock guest reports/inflates free memory at
**4 KiB** granularity. `madvise(MADV_FREE_REUSABLE)` operates on whole host pages. If we
madvise a host page when only *some* of its four 4 KiB guest pages are free, macOS discards
the **entire** 16 KiB — including up to 12 KiB of **live guest data** → silent corruption.
(macOS's exact sub-range rounding direction is immaterial to us — see the invariant.)

**Invariant (the proof obligation).** A 16 KiB host page is handed to
`MADV_FREE_REUSABLE` **only when all four constituent 4 KiB guest pages have been reported
free** (mask `== 0b1111`), and the call passes a host-page-aligned base with a 16 KiB-multiple
length. Because we never pass a partially-free or unaligned range, rounding direction cannot
cause over-free.

**Algorithm (`ReclaimCoalescer`, §Step 1 sketch):**
1. For each free descriptor, translate `desc.addr`→host address (`mem.get_host_address`), and
   walk it in 4 KiB steps. For each 4 KiB page at host addr `p`, set bit
   `((p - base) / 4096)` in the mask of host page `base = p & !(host_page-1)`. This naturally
   handles **unaligned** descriptor starts (leading sub-pages set partial bits) and
   **boundary-spanning** descriptors (contribute to two adjacent masks).
2. After accumulating, emit `(base, host_page)` for every host page whose mask is full;
   **drop** partials.
3. `madvise(MADV_FREE_REUSABLE)` each emitted range, **before** returning the buffers to the
   guest.

**Batch/persistence rules (subtle, safety-critical):**
- **FRQ (Step 1): per-head, never persisted.** Reported pages remain guest-owned and may be
  reallocated after we ack; a persisted partial could later "complete" a host page whose 4th
  sub-page is again live. So a host page split across two *heads* is left mapped (safe, and
  practically nonexistent — see below). Madvise happens **before `add_used`**, while
  `page_reporting` still isolates the pages.
- **Inflate (Step 2): persisted across heads.** Inflated pages are owned by the balloon until
  deflate, so accumulating partials across heads is safe and recovers the cross-head host
  pages on the stock guest.

**Why stock-tier waste is negligible.** Linux `page_reporting` reports in chunks of
`page_reporting_order` (default `pageblock_order`, typically ≥2 MiB on arm64 4 KiB) — i.e.
reported runs are large and pageblock-aligned, so they are also 16 KiB-aligned. The only
non-reclaimed memory is a ≤12 KiB fringe at a run boundary, which essentially never occurs.
(Verify `pageblock_order` on the shipping kernel.)

**Enhanced 16 KiB guest.** Each reported/inflated guest page is exactly one host page →
`host_page/GUEST_PAGE == 1`, so the mask is full after the first sub-page and the coalescer
degenerates to "madvise each reported page" — **exact 1:1 reclaim, zero waste.**

---

## 4. API surface

**libkrun ↔ limina — internal Rust API (NO C ABI).** limina-vmm links libkrun's internal
crates (memory `limina-key-findings`; precedent `docs/design/runtime-display-resize.md`
"internal Rust API, no C ABI"). The roadmap's `krun_add_balloon`/`krun_balloon_set_target`/
`krun_balloon_get_actual`/`krun_balloon_get_stats` names are realized as the **internal
handle methods** below; a `krun_*` C shim is **optional future upstreaming**, off the M6
path.

```rust
// libkrun (src/vmm) — captured at attach, held by limina-vmm:
Vmm::balloon_control_handle(&self) -> Option<BalloonControlHandle>

// BalloonControlHandle (Clone; mechanism only, mirrors DisplayResizeHandle):
fn set_target_pages(&self, pages: u32) -> bool   // 4 KiB units; kicks the device eventfd
fn get_actual(&self) -> u32                       // balloon-held pages
fn get_stats(&self) -> BalloonStats { actual_pages: u32, reclaimed_bytes: u64 }
```

**limina-vmm ↔ supervisor/harness — control socket** (newline text, mirrors the
display-control socket): the worker binds `--balloon-control-socket <path>` and accepts:
- `target <bytes>\n`  → `handle.set_target_pages(bytes >> 12)`
- `stats\n`           → replies `actual=<bytes> target=<bytes> reclaimed=<bytes>\n`

**guest ↔ host — new limina-proto vsock message** (`CHANNEL_CONTROL`,
`msg_type::MEM_PRESSURE = 7`): `Message::MemPressure(MemPressure{ some_avg10, some_avg60,
full_avg10, full_avg60, mem_available_kib, mem_total_kib })`, guest→host, additive and
Unknown-tolerant (old hosts reply `ERR_UNSUPPORTED`, channel survives).

**Supervisor CLI:** `limina --memory <min..max>` (e.g. `2G..12G`). `max`→guest RAM;
`[0, max−min]`→target range driven by policy.

---

## 5. Two-tier matrix

| Feature | Stock 4 KiB Fedora (degraded, must work) | Enhanced 16 KiB guest (optimal) |
|---|---|---|
| FRQ reclaim (Step 1) | Coalesced to 16 KiB host pages; reported runs pageblock-aligned ≥2 MiB → ~full reclaim, ≤12 KiB fringe. Needs `CONFIG_PAGE_REPORTING`. | Exact **1:1** reclaim (mask always full). |
| Inflate/deflate target (Step 2) | Works; 4 KiB balloon pages coalesced; `DEFLATE_ON_OOM` returns pages under guest OOM. | 4 PFNs per 16 KiB page, coalesced perfectly. |
| `--memory min..max` (Step 3) | Identical surface; `max`→RAM, target floor `min`. | Identical. |
| PSI policy (Step 4) | Only if the agent is **installed** (userspace, no kernel dep). `psi=1` may be off → `MemAvailable`-only fallback. Without the agent: conservative idle/time target + opportunistic FRQ. | Agent + `psi=1` shipped by default → full pressure-driven hysteresis. |
| `phys_footprint` returns to host | Yes (the whole point) — high-water mark finally drops. | Yes, with zero coalescing waste. |

Per CLAUDE.md, capabilities are detected **granularly/additively**: FRQ reclaim lights up
from the stock balloon driver alone; the target/PSI loop lights up as the socket/agent
appear. A guest mid-upgrade (some pieces present) is normal and tolerated.

---

## 6. Risk register — adversarial refutations resolved

| # | Sev | Refutation | Resolution baked into the plan |
|---|---|---|---|
| F1 | **FATAL** | Coalescing could madvise a 16 KiB host page when only some 4 KiB guest pages are free → discards live guest memory. | **Only-fully-covered invariant** (mask `==0b1111`) + host-page-aligned ranges + **madvise before `add_used`** + **FRQ per-head/non-persisted** coalescing (§3). Proven by unit test `process_frq_coalescing_is_16k_safe`. |
| M1 | major | Inflate descriptors are **PFN arrays**, not page ranges; reusing `process_frq` would madvise the PFN-array buffer (garbage). | `process_ifq` reads each buffer as LE `u32` balloon-PFNs and translates `pfn<<12`→guest→host (Step 2b). |
| M2 | major | macOS `MADV_FREE_REUSABLE` sub-range rounding direction is unknown → possible over-free. | Invariant means we **only ever** pass host-page-aligned, 16 KiB-multiple, fully-free ranges → rounding direction is immaterial. Confirm with a sub-range extension to `spikes/balloon-madvise/footprint.c` before declaring Step 1 done. |
| M3 | major | Persisting FRQ partials across batches is unsafe (guest may reallocate a reported page). | FRQ coalescer is **per-head, discarded**; inflate coalescer **may persist** (balloon owns inflated pages until deflate). Asymmetry documented (§3). |
| M4 | major | `MADV_FREE_REUSE` re-touch correctness/cost on deflate/realloc. | Freed pages re-fault zero-filled (correct); deflate calls `MADV_FREE_REUSE` for honest accounting. Re-fault cost flagged for a micro-benchmark (open risk). |
| M5 | major | The L1 reclaim test's page-reporting may not fire deterministically. | `munmap`+`compact_memory` nudge + generous poll; Step 2's inflate path is the **deterministic forcing oracle**; the safety unit test is fully deterministic. |
| M6 | major | Roadmap says add a `krun_*` C API; project decision is internal Rust only. | `BalloonControlHandle` internal Rust API (mirrors shipped `DisplayResizeHandle`); C shim **deferred** (§4). |
| M7 | major | `phys_footprint` of which process? Supervisor RSS won't reflect reclaim. | Measure the **worker** (limina-vmm holds the guest-RAM `MAP_ANON`) via `proc_listchildpids`+`proc_pid_rusage` `ri_phys_footprint`. |
| M8 | major | `DEFLATE_ON_OOM` must be advertised **before** driving inflate. | Feature bit lands in the **same patch (0034)** as the inflate handlers; inflate is never driven by a build lacking it. **Superseded 2026-07-20:** the bit is being dropped — see the transparent-accounting addendum at the end of this doc. |
| M9 | minor | config-change raised while the device is briefly Inactive. | `target_evt` stays hot; `num_pages` applied on activate; the guest reads it after activate (parallels the GPU resize "applied on next service" path). |
| m10 | minor | Balloon STATS_VQ guest stats not parsed. | Deferred — the PSI agent supplies richer pressure data; `handle_stq_event` keeps draining (`event_handler.rs:42-54`). |
| m11 | minor | `psi=1` may be off on stock Fedora. | Agent `MemAvailable`-only fallback; enhanced image sets `psi=1`. |
| m12 | minor | ≤12 KiB fringe loss per reported run on stock. | Reported runs are pageblock-aligned (≥2 MiB) → fringe ~nil; verify `pageblock_order`. |

---

## 7. Definition of done (mapped to the roadmap done-test)

Roadmap done-test: *start a VM with `--memory 2G..12G`; run a memory-heavy guest workload
and watch the footprint rise toward max; quit it and watch `vmmap`/Activity Monitor show
limina's `phys_footprint` drop back toward 2G as pages are madvised back to macOS.*

- **Step 1 (must land + HVF-verify first):** `l1_balloon_reclaim` GREEN — worker
  `phys_footprint` drops after the guest frees memory (FRQ reclaim works); the safety unit
  test GREEN. `MADV_FREE_REUSABLE` replaces `MADV_DONTNEED`.
- **Step 2:** `l1_balloon_inflate` GREEN — a host-set target shrinks guest `MemTotal` and
  worker footprint by ~target; `DEFLATE_ON_OOM` advertised.
- **Step 3:** `l1_balloon_target` GREEN — `limina --memory 2G..12G` boots, `max`→RAM,
  target reaches the live device end-to-end; `stats` readback shows `actual` rise.
- **Step 4:** `l1_balloon_psi` GREEN — under guest pressure the supervisor deflates (footprint
  rises toward `max`); on idle it inflates (footprint falls toward **2 GiB**).
- **Manual milestone confirmation:** `limina --memory 2G..12G` on the real desktop; a browsing
  session pushes footprint up; closing it returns footprint toward ~2 GiB in Activity Monitor
  / `proc_pid_rusage` (the 6.8 GiB high-water mark finally returns).

**Patches produced:** `patches/libkrun/0033-*` (reclaim + coalescing),
`patches/libkrun/0034-*` (inflate/deflate + `DEFLATE_ON_OOM` + `BalloonControlHandle` +
`Vmm` accessor). Steps 3–4 carry **no** libkrun patch (limina + guest only). Re-export the
series per `patches/libkrun/README.md`.

## Addendum (2026-07-02): reclaim modes — host-pressure-driven squeeze

`spikes/mem-overhead-2026-07-02` Run D quantified what the always-squeeze-on-idle policy costs
the guest: full inflation permanently evicts the guest page cache — warm 4 KiB random reads go
852k IOPS / 1 µs → 13.3k IOPS / 75 µs (**64×**; metadata walks 4×; sequential re-reads are
rescued by the host UBC since we run buffered) — to buy ~2–4 GB of host RAM the host may not
even need. So the squeeze is now keyed to **host** memory pressure
(`kern.memorystatus_vm_pressure_level`), selected by `--reclaim` / vm.toml
`hardware.reclaim` (`crates/limina/src/balloon_policy.rs::ReclaimMode`):

| mode | host normal | host warn | host critical |
|------|-------------|-----------|---------------|
| `disabled`   | never engage (FRQ still returns freed guest memory) | — | — |
| `light`      | no balloon (target drifts to 0) | leave 25% of max (≥2 GiB) as cache | full squeeze |
| `moderate` (default) | leave 12.5% of max (≥1 GiB) as cache | full squeeze | full squeeze |
| `aggressive` | full squeeze whenever the guest is idle (the original Step-4 policy, host pressure ignored) | 〃 | 〃 |

Mechanics: guest-pressure release-to-0 is unchanged and immediate in every mode; inflation
still requires an idle guest (PSI `some ≤ 2%`) and is allowance-bounded — the balloon may only
take `MemAvailable − allowance`, and deflates by the shortfall if the guest dips below the
allowance while idle. A 16 MiB dead band prevents target dribble. Host pressure is sampled per
guest report and injected into the pure `decide()` (unit-tested per mode/pressure).

### Managed VMs are always dynamic (2026-07-02, follow-up)

vm.toml's `hardware.memory` is now **just the maximum** (`"8G"`, `"8GiB"`, bare MiB — GB/GiB/
MB/MiB spellings all binary); every managed VM boots `--memory 1024..MAX`
(`schema::DYNAMIC_MIN_MIB`, clamped to max for tiny VMs — min == max degrades to a no-op
policy). `reclaim = "disabled"` is the static-like escape hatch (balloon idle, FRQ still
active). The flat CLI keeps explicit `--ram-mib` / `--memory MIN..MAX` for tests and
special cases.

## Addendum (2026-07-03): control-loop stabilization — the dogfood-guest oscillation

Live diagnosis on the dogfood VM (24 GiB, `moderate`, active desktop) caught the Step-4 policy
in a ~40 s **limit cycle**, repeating whenever the user was active: a 10 s calm PSI window let
the policy ratchet the target up in ¼-of-room steps (5.9 GiB per 800 ms dwell — 0→20 GB in
seconds, far faster than the ~10 s PSI sensor could push back); the squeeze forced the guest's
cache out and ~GiBs of anon to its disk swapfile; PSI climbed into the 2–10% band where the
policy could neither inflate nor correct (the below-allowance give-back was gated on ≤2%), so
the guest thrashed against an unreachable target (`fill_balloon` → "Out of puff!" kernel spam,
4.5M major faults / ~17 GB cumulative swap-in over 12 h); at 10% the panic release dumped the
whole balloon, PSI decayed, and the cycle restarted — all while the 48 GB host sat at pressure
*normal*. Classic bang-bang oscillation: actuator ≫ sensor, hold-only neutral band, no memory
of blowouts.

Fixes (all in `decide()` / `on_pressure`, unit-tested as `inflation_steps_are_small_and_bounded`,
`neutral_band_still_deflates_below_the_allowance`, `light_normal_gives_back_in_the_neutral_band`,
`inflation_requires_sustained_calm`, `release_cooldown_blocks_reinflation`):

- **Inflation steps 256 MiB per 2 s dwell** (was ¼ room per 800 ms): the actuator now moves
  slower than the PSI sensor, bounding overshoot between reports to a few hundred MiB.
- **Deflation is unconditional**: below-allowance give-back (and Light@normal drift-to-0) now
  fires at *any* PSI, so an over-tight target self-corrects within seconds instead of waiting
  for the 10% blowout. Only the *inflate* direction is idle-gated.
- **Inflation requires sustained calm**: `some avg60 ≤ 2%` in addition to avg10 — a busy guest
  catching its breath for 10 s no longer reads as idle.
- **Post-release cooldown (5 min)**: any avg10 ≥ 10% report arms it (even at target 0); a
  blowout proves the guest needs its memory, so the policy stops re-testing that boundary.

Debuggability: the worker's balloon `stats` reply now includes the last commanded
`target=<bytes>` alongside `actual`/`reclaimed` — the target/actual gap (guest failing to
inflate) is exactly the oscillation signature, and it was invisible during the diagnosis.

## Addendum (2026-07-20): drop `DEFLATE_ON_OOM` — transparent balloon accounting

**Decision: stop advertising `VIRTIO_BALLOON_F_DEFLATE_ON_OOM`.** This supersedes Step 2's
"OOM safety net" rationale and risk **M8** in §6. Implementation pending (see the
compensations below — the burst test lands RED-first, before the bit is dropped).

**The symptom.** A freshly booted managed VM looks nearly out of memory: the autoballoon
inflates toward `min` right after boot, and `free`/htop/GNOME System Monitor show
`MemTotal = max` with almost all of it "used" — alarming, and indistinguishable from real
exhaustion inside the guest.

**Why — guest accounting is keyed on this exact bit.** Linux's `virtio_balloon` subtracts
ballooned pages from the managed page count **only when `DEFLATE_ON_OOM` is NOT negotiated**
(verified against v6.12 `drivers/virtio/virtio_balloon.c`, `fill_balloon()`/`leak_balloon()`):

```c
if (!virtio_has_feature(vb->vdev, VIRTIO_BALLOON_F_DEFLATE_ON_OOM))
    adjust_managed_page_count(page, -1);   /* +1 on leak */
```

With the bit (what we ship today): inflated pages stay in `MemTotal` but sit allocated and
unreclaimable → they read as *used*, `MemAvailable` collapses. Without the bit: `MemTotal`
itself shrinks/grows with the balloon, so the guest's totals track *effective* RAM
(`max − actual`) and "used" reflects only real guest usage. That is the transparent
accounting we want, and it's the stock-driver code path — a **host-side-only change that
benefits stock and enhanced guests alike** (two-tier clean).

**The two ownership models (upstream rationale: commit `997e120843e8`, Denis V. Lunev,
2015, "virtio_balloon: do not change memory amount visible via /proc/meminfo").** The bit
encodes contract semantics, not just an OOM hook:

- **Bit absent — pages are donated.** The guest has no mechanism to take them back (only
  the host deflates), so honest accounting removes them from the totals; watermarks and
  overcommit heuristics are sized against memory that actually exists.
- **Bit present — pages are a loan callable at OOM.** The Virtuozzo-style SLA reading:
  "the memory is still yours by contract", so the total keeps showing the full guarantee,
  and the driver registers an OOM notifier that hands pages back at true OOM instead of
  letting the kill proceed. The visible "usage" is **load-bearing** in this model — it
  *relies* on the guest building real memory pressure to trigger the give-back.

**Why the guest-side net is worth less than it looks on a modern guest.** The kernel OOM
notifier fires at the last moment, inside the kernel OOM path. Fedora ships
**systemd-oomd**, which watches PSI/`MemAvailable` and kills *earlier* — and to oomd,
balloon-induced apparent usage is indistinguishable from real pressure. So a heavily
inflated `DEFLATE_ON_OOM` balloon can get user apps killed by oomd **before the kernel net
ever engages**: the bit's safety mechanism is preempted by the very pressure appearance it
creates. Meanwhile our host-side policy is the entity with the actual global view and
deflates *before* guest pressure builds (unconditional deflation + sustained-calm gating,
§Addendum 2026-07-03; MemAvailable-floor starvation release, 76f3ec2 2026-07-09).

**Trade-off accepted, with compensations.** Dropping the bit removes the guest's
last-resort synchronous deflate at OOM; the PSI policy (2 s cadence, 256 MiB steps) becomes
the only pressure response, and a fast multi-GiB allocation burst is the case that could
outrun it. Required with the change:

1. **RED-first allocation-burst test** (`crates/limina-test`): guest ballooned near `min`
   suddenly allocates several GiB; assert no OOM kill — the release path must outrun the
   burst. Tune release step/cadence if RED. Lands *before* the bit is dropped.
2. Consider a per-VM escape hatch (vm.toml) to re-advertise the bit, at least while we
   gain confidence.

**Cosmetic flip side (expected, fine):** with the bit gone, a fresh `min..max` VM shows
`MemTotal` ≈ `min`, growing under load — the standard dynamic-memory presentation (QEMU's
default balloon, Hyper-V dynamic memory, virtio-mem). Anything reading `MemTotal` as "the
configured size" (monitoring dashboards, `nproc`-style sizing heuristics in guest apps)
will now see the effective size instead; that's the honest number.

**Rejected alternative:** keep the bit and patch the *enhanced* kernel's driver to adjust
the managed count anyway. Fixes only the enhanced tier, forks accounting semantics from
upstream (the tie between the bit and the accounting is deliberate there), and isn't
upstreamable — the host-side bit drop is the smaller, two-tier-clean change.

### Implemented (2026-07-20, libkrun 0087)

Shipped exactly as decided, in commit 69a2551:

- **libkrun 0087**: `AVAIL_FEATURES` no longer carries the bit;
  `Balloon::new(free_page_reporting, deflate_on_oom)` re-advertises it per VM via
  `VmResources::balloon_deflate_on_oom` (default false).
- **Escape hatch (compensation 2)**: `--balloon-deflate-on-oom` on both binaries, plus
  vm.toml `[hardware] balloon_deflate_on_oom = true` (default false, round-trip-tested).
- **Burst guard (compensation 1)**: `balloon_burst.rs` / `allocation_burst_survives_inflated_balloon`,
  registered in `test-boot.sh`. The harness plays the agent (as in `balloon_psi.rs`),
  inflates to the policy cap via synthetic idle reports (3840 MiB on the 2048..6144 test
  VM — effective guest memory ≈ 2 GiB), then runs a 3 GiB C-speed anonymous memset burst
  (python3 ctypes, 12×256 MiB) in the guest while relaying its **real**
  `/proc/pressure/memory` + `/proc/meminfo` to the policy at the agent's ~1 s cadence.
  Assertions: the burst completes AND `journalctl -k` shows zero OOM kills.
- **Post-drop verdict: GREEN.** The relay trace caught `MemAvailable` cratering to
  757 MiB, then the PSI spike (`some_avg10` = 4.16 %) with a 256 MiB policy release while
  direct reclaim spilled the burst's (compressible) pages to zram — burst completed, no
  OOM. On a stock guest the release path is **policy deflate + zram absorbing the
  overshoot**, and that combination held even with the balloon pinned at its cap.
- Transparent accounting observed live: guest `MemTotal` tracked the balloon 1:1 all the
  way down to ≈ `min` (2055 MiB shown for the 2048 MiB floor — the delta is the kernel's
  own reservations), and back up on release. The MemTotal≈min presentation described
  above is exactly what the guest now shows.

Two test-vehicle traps worth remembering (both bit silently): a `\`-continued Rust string
literal strips the next line's leading whitespace, which de-indented the staged python so
it died instantly with `IndentationError` while the constant avail/total gap in the relay
trace was the only tell the burst never ran; and `pgrep -f <pattern>` over ssh matches the
ssh-spawned shell whose own command line carries the pattern — bracket the first character
(`pgrep -f '[b]urst.py'`) or a dead allocator looks alive forever.
